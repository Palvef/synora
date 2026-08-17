//! HTTP directory-mirroring provider (spec §14/§60, tsumugu-style): parse the
//! upstream index and download only files that differ. Single-file failures
//! are skipped with detailed warnings — never abort the run; symlinks are
//! ignored on both sides (tsumugu behavior).

use crate::{ProviderError, SyncContext, SyncResult};

pub struct HttpProvider {
    pub parser: String,
    pub delete: bool,
}

impl HttpProvider {
    pub async fn sync(&self, ctx: &SyncContext) -> Result<SyncResult, ProviderError> {
        let upstream = ctx
            .upstream
            .as_deref()
            .ok_or_else(|| ProviderError::Config("http provider requires `upstream`".into()))?;
        // Egress: the manager-dispatched proxy env (e.g. cf-warp expose) wins.
        // The expose endpoint is an authenticated HTTP CONNECT proxy, so a
        // socks5h:// dispatch URL is used as http:// here (reqwest tunnels
        // CONNECT; it has no socks feature).
        let proxy = ctx
            .proxy_env
            .iter()
            .find(|(k, _)| k == "HTTP_PROXY" || k == "ALL_PROXY")
            .map(|(_, v)| {
                v.strip_prefix("socks5h://")
                    .map(|rest| format!("http://{rest}"))
                    .unwrap_or_else(|| v.clone())
            });
        let fetcher = httpfetch::Fetcher::with_proxy(proxy.as_deref())
            .map_err(|e| ProviderError::Other(e.to_string()))?;
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
                "downloaded {} files ({} skipped, {} deleted)",
                stats.files_downloaded, stats.files_skipped, stats.files_deleted
            )),
            ..Default::default()
        })
    }
}
