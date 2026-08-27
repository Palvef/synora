//! HTTP directory-mirroring provider (spec §14/§60, tsumugu-style): parse the
//! upstream index and download only files that differ. Per-file download
//! errors are logged and the rest of the tree still transfers, but the run
//! is failed if any file could not be fetched. Local symlinks are left
//! alone; listing-marked symlinks are mirrored as local links.

use crate::{ProviderError, SyncContext, SyncResult};

pub struct HttpProvider {
    pub parser: String,
    pub delete: bool,
    /// Max concurrent directory-listing requests and downloads;
    /// `None` = httpfetch default (5).
    pub threads: Option<u32>,
    /// Root-relative path prefixes excluded from traversal and deletion.
    pub exclude: Vec<String>,
}

impl HttpProvider {
    pub async fn sync(&self, ctx: &SyncContext) -> Result<SyncResult, ProviderError> {
        let upstream = ctx
            .upstream
            .as_deref()
            .ok_or_else(|| ProviderError::Config("http provider requires `upstream`".into()))?;
        // Manager-dispatched proxy env wins. Expose is HTTP CONNECT
        // (`http://host:port`); reqwest uses that as-is. Do not rewrite.
        let proxy = ctx
            .proxy_env
            .iter()
            .find(|(k, _)| {
                k.eq_ignore_ascii_case("all_proxy") || k.eq_ignore_ascii_case("http_proxy")
            })
            .map(|(_, v)| v.clone());
        let bytes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        if let Some(usage) = ctx.usage.clone() {
            let counter = bytes.clone();
            let cancel = ctx.cancel.clone();
            tokio::spawn(async move {
                let mut last: Option<u64> = None;
                let mut last_tick = std::time::Instant::now();
                let mut ticker = tokio::time::interval(std::time::Duration::from_secs(2));
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                ticker.tick().await;
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        _ = ticker.tick() => {
                            let now = counter.load(std::sync::atomic::Ordering::Relaxed);
                            let t = std::time::Instant::now();
                            let dt = t.duration_since(last_tick).as_secs_f64();
                            if let Some(prev) = last {
                                if dt >= 0.5 {
                                    let bps = now.saturating_sub(prev) as f64 / dt;
                                    usage.lock().unwrap().record_bandwidth(bps);
                                }
                            }
                            last = Some(now);
                            last_tick = t;
                        }
                    }
                }
            });
        }
        let fetcher = httpfetch::Fetcher::with_proxy(proxy.as_deref())
            .map_err(|e| ProviderError::Other(e.to_string()))?
            .with_threads(self.threads.unwrap_or(httpfetch::DEFAULT_THREADS as u32) as usize)
            .with_excludes(self.exclude.clone())
            .with_byte_counter(bytes);
        let started = std::time::Instant::now();
        let stats = fetcher
            .sync(
                upstream,
                &self.parser,
                &ctx.storage,
                self.delete,
                16,
                &ctx.cancel,
                ctx.log_file.as_deref(),
            )
            .await
            .map_err(|e| match e {
                httpfetch::FetchError::Cancelled => ProviderError::Cancelled,
                other => ProviderError::Other(other.to_string()),
            })?;
        tracing::info!(
            "http sync `{}`: {} files, {} bytes in {:.1}s (skipped {}, failed {})",
            ctx.job_name,
            stats.files_downloaded,
            stats.downloaded_bytes,
            started.elapsed().as_secs_f32(),
            stats.files_skipped,
            stats.files_failed
        );
        if stats.files_failed > 0 {
            return Err(ProviderError::Other(format!(
                "http sync failed: {} file(s) could not be downloaded",
                stats.files_failed
            )));
        }
        Ok(SyncResult {
            exit_code: Some(0),
            stdout: stats.log_lines.join("\n").into_bytes(),
            bytes_transferred: Some(stats.downloaded_bytes),
            size_hint: stats.total_size_hint,
            message: Some(format!(
                "downloaded {} files ({} skipped, {} deleted, {} symlinked)",
                stats.files_downloaded,
                stats.files_skipped,
                stats.files_deleted,
                stats.files_symlinked
            )),
            ..Default::default()
        })
    }
}
