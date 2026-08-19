//! HTTP directory-mirroring provider (spec §14/§60, tsumugu-style): parse the
//! upstream index and download only files that differ. Single-file failures
//! are skipped with detailed warnings — never abort the run; local symlinks
//! are left alone and listing-marked symlinks are mirrored as local links
//! (tsumugu behavior).

use crate::{ProviderError, SyncContext, SyncResult};

pub struct HttpProvider {
    pub parser: String,
    pub delete: bool,
    /// Max concurrent downloads; `None` = httpfetch default (8).
    pub threads: Option<u32>,
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
        let fetcher = httpfetch::Fetcher::with_proxy(proxy.as_deref())
            .map_err(|e| ProviderError::Other(e.to_string()))?
            .with_threads(self.threads.unwrap_or(httpfetch::DEFAULT_THREADS as u32) as usize);
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
            "http sync `{}`: {} files, {} bytes in {:.1}s (skipped {})",
            ctx.job_name,
            stats.files_downloaded,
            stats.downloaded_bytes,
            started.elapsed().as_secs_f32(),
            stats.files_skipped
        );
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
