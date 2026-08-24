//! HTTP mirror sync (spec §60): diff an upstream directory listing against
//! local storage by size (mtime as a tiebreaker when the listing carries
//! timestamps), download what changed, optionally delete what vanished
//! upstream. Planning is parser-driven ([`parser`] crate) and yields a
//! [`Plan`]; [`Fetcher::execute`] runs it concurrently with a cancel token.
//!
//! Failure discipline: a broken *file* never aborts a sync — it is logged
//! and skipped ([`FetchStats::files_skipped`]). Only an unreachable base
//! URL (the first index request) or explicit cancellation returns an error.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

/// Upper bound on index entries visited while planning, so a hostile or
/// runaway index can't spin the planner forever.
// Large upstreams such as download.postgresql.org exceed 200k entries. Keep
// a defensive ceiling, but high enough to finish a real distribution tree.
const MAX_ENTRIES: usize = 1_000_000;
/// Hard cap on recursion depth even when the caller asks for more.
const MAX_DEPTH: u32 = 16;
/// Default max concurrent downloads during [`Fetcher::execute`]; override
/// per fetcher via [`Fetcher::with_threads`] (tunasync
/// `TUNASYNC_TSUMUGU_THREADS`).
pub const DEFAULT_THREADS: usize = 5;
/// Connection and idle-read timeouts. A total 30-second request deadline made
/// every large package fail even while bytes were still arriving.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Live progress lines are appended at most this often (per-run log noise cap).
const PROGRESS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

fn synora_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    format!("{v:.1} {}", UNITS[u])
}

/// Append a progress line to the run log (fire-and-forget; best-effort).
fn append_log(log_file: Option<&std::path::Path>, line: &str) {
    let Some(path) = log_file else { return };
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{line}");
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("http error: {0}")]
    Http(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("cancelled")]
    Cancelled,
}

impl From<reqwest::Error> for FetchError {
    fn from(e: reqwest::Error) -> Self {
        FetchError::Http(e.to_string())
    }
}

impl From<std::io::Error> for FetchError {
    fn from(e: std::io::Error) -> Self {
        FetchError::Io(e.to_string())
    }
}

#[derive(Debug, Clone, Default)]
pub struct FetchStats {
    pub downloaded_bytes: u64,
    pub files_downloaded: u32,
    pub files_deleted: u32,
    /// Local symlinks created (or left in place because they already
    /// pointed at the right target).
    pub files_symlinked: u32,
    /// Files skipped after a per-file failure (download error, timeout,
    /// failed delete). Download failures also increment `files_failed`.
    pub files_skipped: u32,
    /// Download errors. The HTTP provider fails the run when this is > 0.
    pub files_failed: u32,
    /// Sum of remote sizes of every regular file in the listing (the
    /// repository size), whether or not it needs downloading. `None` when
    /// any remote file's size was unknown.
    pub total_size_hint: Option<u64>,
    /// Per-file detail lines (downloaded/skipped/deleted) for run logs.
    pub log_lines: Vec<String>,
}

/// A planned sync: what to download (url → destination), what to delete,
/// and which local symlinks to (re)create (destination → link target).
#[derive(Debug, Default)]
pub struct Plan {
    pub downloads: Vec<(String, PathBuf)>,
    pub deletes: Vec<PathBuf>,
    pub symlinks: Vec<(PathBuf, String)>,
    // Planning internals feeding `total_size_hint`; not part of the public
    // shape so users can't fake a hint.
    size_sum: u64,
    size_unknown: bool,
    total_size_hint: Option<u64>,
    /// True when the listing was truncated or a subdirectory failed — deletes
    /// must not run against a partial remote set.
    incomplete: bool,
}

/// HTTP fetcher: a reqwest client with rustls, redirects followed (max 10),
/// no proxy by default, a 30 s connect timeout plus 120 s idle-read timeout,
/// and up to `threads` concurrent downloads.
pub struct Fetcher {
    client: reqwest::Client,
    threads: usize,
    byte_counter: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
}

impl Fetcher {
    pub fn new() -> Result<Self, FetchError> {
        Self::with_proxy(None)
    }

    /// Build a client with an explicit proxy URL (`http://[user:pass@]host:port`)
    /// when given — the manager-dispatched egress path — and no proxy otherwise.
    pub fn with_proxy(proxy: Option<&str>) -> Result<Self, FetchError> {
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(10))
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT);
        builder = match proxy {
            Some(url) if url.to_ascii_lowercase().starts_with("socks") => {
                return Err(FetchError::Http(
                    "SOCKS proxies are not supported by the HTTP provider; the manager must dispatch the HTTP CONNECT expose (cf-warp)".into(),
                ));
            }
            Some(url) => builder
                .proxy(reqwest::Proxy::all(url).map_err(|e| FetchError::Http(e.to_string()))?),
            None => builder.no_proxy(),
        };
        Ok(Self {
            client: builder.build()?,
            threads: DEFAULT_THREADS,
            byte_counter: None,
        })
    }

    /// Override max concurrent downloads (tunasync `TUNASYNC_TSUMUGU_THREADS`).
    /// `0` is clamped to 1 — a zero-permit semaphore would deadlock.
    pub fn with_threads(mut self, threads: usize) -> Self {
        self.threads = threads.max(1);
        self
    }

    /// Live downloaded-byte counter for the worker bandwidth sampler.
    pub fn with_byte_counter(
        mut self,
        counter: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        self.byte_counter = Some(counter);
        self
    }

    /// Recursively fetch the index from `base_url` with the given parser and
    /// compare each remote entry against local files by size (mtime when the
    /// parser provided `modified` and the local mtime matches it — size-only
    /// is the documented lazy default). Missing/size-changed files are
    /// planned for download; when `delete` is true, local files absent from
    /// the index are planned for deletion. Dir entries recurse by appending
    /// the entry path to `base_url`. Fails only if the base index itself is
    /// unreachable; a broken subdirectory listing is warned about and
    /// skipped.
    async fn get_listing(&self, url: &str) -> Result<Vec<u8>, FetchError> {
        let mut last = FetchError::Http(format!("GET {url} failed"));
        for attempt in 1..=4u32 {
            let sent = self.client.get(url).send().await;
            match sent {
                Ok(resp) => match resp.error_for_status() {
                    Ok(ok) => match ok.bytes().await {
                        Ok(body) => return Ok(body.to_vec()),
                        Err(e) => last = e.into(),
                    },
                    Err(e) => last = e.into(),
                },
                Err(e) => last = e.into(),
            }
            if attempt == 4 || !listing_error_transient(&last) {
                return Err(last);
            }
            tracing::warn!("listing {url} attempt {attempt} failed: {last}; retrying");
            tokio::time::sleep(std::time::Duration::from_millis(
                300 * u64::from(attempt).pow(2),
            ))
            .await;
        }
        Err(last)
    }

    pub async fn plan(
        &self,
        base_url: &str,
        parser_name: &str,
        storage: &Path,
        delete: bool,
        depth: u32,
        log_file: Option<&Path>,
    ) -> Result<Plan, FetchError> {
        let parser: Box<dyn parser::IndexParser> = parser::parser_for(parser_name)
            .ok_or_else(|| FetchError::Http(format!("unknown index parser `{parser_name}`")))?;
        let mut plan = Plan::default();
        let mut remote = HashSet::new();
        let mut planner = Planner {
            fetcher: self,
            storage,
            parser: parser.as_ref(),
            plan: &mut plan,
            remote: &mut remote,
            seen: 0,
            dirs: 0,
            last_report: tokio::time::Instant::now(),
            log_file,
            listing_failed: false,
        };
        planner.dir(base_url, "", depth.min(MAX_DEPTH)).await?;
        if delete && planner.listing_failed {
            return Err(FetchError::Http(
                "subdirectory listing failed; refusing delete to avoid wiping the mirror".into(),
            ));
        }
        if delete && plan.incomplete {
            tracing::warn!(
                "listing incomplete (entry/depth cap); skipping deletes for {}",
                storage.display()
            );
        } else if delete {
            plan.deletes = local_extras(storage, &remote).await?;
        }
        plan.total_size_hint = if plan.size_unknown {
            None
        } else {
            Some(plan.size_sum)
        };
        Ok(plan)
    }

    /// Execute a plan concurrently (up to `self.threads` in-flight) with a
    /// cancel token. Each file downloads to
    /// `dest.partial` then renames into place; a failed file is warned about
    /// and skipped, never fatal — only cancellation aborts the run. Planned
    /// symlinks are (re)created after downloads and deletes. Summed stats
    /// are returned.
    pub async fn execute(
        &self,
        plan: &Plan,
        cancel: &CancellationToken,
        log_file: Option<&std::path::Path>,
    ) -> Result<FetchStats, FetchError> {
        if cancel.is_cancelled() {
            return Err(FetchError::Cancelled);
        }
        let mut stats = FetchStats {
            total_size_hint: plan.total_size_hint,
            ..FetchStats::default()
        };
        let mut last_report = tokio::time::Instant::now();
        let report = |log_file: Option<&std::path::Path>, s: &FetchStats| {
            append_log(
                log_file,
                &format!(
                    "progress: downloaded {} files ({}) skipped {} deleted {}",
                    s.files_downloaded,
                    synora_size(s.downloaded_bytes),
                    s.files_skipped,
                    s.files_deleted
                ),
            );
        };
        append_log(
            log_file,
            &format!(
                "planning done: {} downloads, {} deletes, {} symlinks",
                plan.downloads.len(),
                plan.deletes.len(),
                plan.symlinks.len()
            ),
        );
        let mut remaining = plan.downloads.iter().cloned();
        let mut in_flight: tokio::task::JoinSet<Result<(u64, String), (FetchError, String)>> =
            tokio::task::JoinSet::new();
        let spawn_one = |set: &mut tokio::task::JoinSet<_>, url: String, dest: PathBuf| {
            let client = self.client.clone();
            let cancel = cancel.clone();
            set.spawn(async move {
                let u2 = url.clone();
                download_one(&client, &url, &dest, &cancel)
                    .await
                    .map(|b| (b, u2.clone()))
                    .map_err(|e| (e, u2))
            });
        };
        for _ in 0..self.threads {
            match remaining.next() {
                Some((url, dest)) => spawn_one(&mut in_flight, url, dest),
                None => break,
            }
        }
        // Single-file failures must not sink the whole sync: warn (done in
        // download_one) and carry on; only cancellation aborts. At most
        // `threads` tasks exist at once (not one task per file).
        let mut cancelled = false;
        while let Some(joined) = in_flight.join_next().await {
            match joined {
                Ok(Ok((bytes, url))) => {
                    stats.downloaded_bytes += bytes;
                    stats.files_downloaded += 1;
                    stats
                        .log_lines
                        .push(format!("downloaded {url} ({bytes} bytes)"));
                    if let Some(counter) = &self.byte_counter {
                        counter.store(stats.downloaded_bytes, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                Ok(Err((FetchError::Cancelled, _))) => cancelled = true,
                Ok(Err((e, url))) => {
                    stats.files_skipped += 1;
                    stats.files_failed += 1;
                    let line = format!("skipped {url}: {e}");
                    append_log(log_file, &line);
                    stats.log_lines.push(line);
                }
                Err(_) => {
                    tracing::warn!("download task panicked");
                    stats.files_skipped += 1;
                    stats.files_failed += 1;
                    append_log(log_file, "skipped download: worker task panicked");
                }
            }
            if stats.log_lines.len() > 256 {
                let drop_n = stats.log_lines.len() - 256;
                stats.log_lines.drain(0..drop_n);
            }
            if !cancelled {
                if let Some((url, dest)) = remaining.next() {
                    spawn_one(&mut in_flight, url, dest);
                }
            }
            if last_report.elapsed() >= PROGRESS_INTERVAL {
                report(log_file, &stats);
                last_report = tokio::time::Instant::now();
            }
        }
        report(log_file, &stats);
        if cancelled {
            return Err(FetchError::Cancelled);
        }
        for path in &plan.deletes {
            if cancel.is_cancelled() {
                return Err(FetchError::Cancelled);
            }
            if let Err(e) = tokio::fs::remove_file(path).await {
                tracing::warn!(
                    "skipping failed delete of {}: {}",
                    path.display(),
                    error_chain(&e)
                );
                stats.files_skipped += 1;
                stats.log_lines.push(format!(
                    "skipped delete of {}: {}",
                    path.display(),
                    error_chain(&e)
                ));
                continue;
            }
            stats.files_deleted += 1;
            stats.log_lines.push(format!("deleted {}", path.display()));
        }
        // Symlink entries are mirrored after downloads and deletes so a
        // stale regular file at the same path is gone before the link goes
        // in. Idempotent: a local link already pointing at the target is
        // left untouched.
        #[cfg(unix)]
        for (dest, target) in &plan.symlinks {
            if cancel.is_cancelled() {
                return Err(FetchError::Cancelled);
            }
            match ensure_symlink(dest, target).await {
                Ok(true) => {
                    stats.files_symlinked += 1;
                    stats
                        .log_lines
                        .push(format!("symlinked {} -> {target}", dest.display()));
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!("skipping symlink {}: {}", dest.display(), error_chain(&e));
                    stats.files_skipped += 1;
                    stats.log_lines.push(format!(
                        "skipped symlink {}: {}",
                        dest.display(),
                        error_chain(&e)
                    ));
                }
            }
        }
        #[cfg(not(unix))]
        if !plan.symlinks.is_empty() {
            tracing::warn!(
                "skipping {} symlink entries (symlinks unsupported on this platform)",
                plan.symlinks.len()
            );
        }
        Ok(stats)
    }

    /// Convenience: [`Fetcher::plan`] then [`Fetcher::execute`].
    /// `log_file`, when given, receives live progress lines (best-effort).
    #[allow(clippy::too_many_arguments)]
    pub async fn sync(
        &self,
        base_url: &str,
        parser_name: &str,
        storage: &Path,
        delete: bool,
        depth: u32,
        cancel: &CancellationToken,
        log_file: Option<&Path>,
    ) -> Result<FetchStats, FetchError> {
        let plan = self
            .plan(base_url, parser_name, storage, delete, depth, log_file)
            .await?;
        self.execute(&plan, cancel, log_file).await
    }
}

impl Default for Fetcher {
    fn default() -> Self {
        Self::new().expect("reqwest client with default TLS setup cannot fail to build")
    }
}

fn listing_error_transient(err: &FetchError) -> bool {
    let s = err.to_string().to_ascii_lowercase();
    s.contains("timeout")
        || s.contains("timed out")
        || s.contains("connection")
        || s.contains("reset")
        || s.contains("refused")
        || s.contains("proxy")
        || s.contains("error sending request")
        || s.contains("502")
        || s.contains("503")
        || s.contains("504")
        || s.contains("522")
        || s.contains("523")
        || s.contains("524")
}

/// Recursive planner: fetches one index, diffs files against local storage,
/// recurses into dirs. One planner per `plan()` call; `seen` bounds total
/// entries and `dir` is reentrant per depth.
struct Planner<'a> {
    fetcher: &'a Fetcher,
    storage: &'a Path,
    parser: &'a dyn parser::IndexParser,
    plan: &'a mut Plan,
    remote: &'a mut HashSet<String>,
    seen: usize,
    dirs: usize,
    last_report: tokio::time::Instant,
    log_file: Option<&'a Path>,
    listing_failed: bool,
}

impl Planner<'_> {
    async fn dir(&mut self, url: &str, dir_rel: &str, depth: u32) -> Result<(), FetchError> {
        self.dirs += 1;
        // Progress at most every few seconds — deep trees would otherwise
        // flood the run log with a line per directory.
        if self.dirs.is_multiple_of(20) && self.last_report.elapsed() >= PROGRESS_INTERVAL {
            self.last_report = tokio::time::Instant::now();
            append_log(
                self.log_file,
                &format!("planning: listed {} entries so far (at {url})", self.seen),
            );
        }
        let body = self.fetcher.get_listing(url).await?;
        for entry in self.parser.parse(&body) {
            if self.seen >= MAX_ENTRIES {
                tracing::warn!("index entry cap ({MAX_ENTRIES}) reached at {url}");
                self.plan.incomplete = true;
                break;
            }
            if !is_safe_rel(&entry.path) {
                tracing::warn!("skipping unsafe entry path `{}` at {url}", entry.path);
                continue;
            }
            self.seen += 1;
            let rel = if dir_rel.is_empty() {
                entry.path.clone()
            } else {
                format!("{dir_rel}/{}", entry.path)
            };
            let rel_key = rel.trim_end_matches('/').to_string();
            self.remote.insert(rel_key.clone());
            match entry.kind {
                parser::EntryKind::Dir => {
                    if depth == 0 {
                        self.plan.incomplete = true;
                        continue;
                    }
                    let child_url = join_slash(url, &entry.path);
                    let child_rel = rel.trim_end_matches('/').to_string();
                    let result = Box::pin(self.dir(&child_url, &child_rel, depth - 1)).await;
                    if let Err(e) = result {
                        // A broken subdirectory must not sink the whole plan,
                        // but it DOES make the remote set incomplete — deletes
                        // are refused by the caller.
                        tracing::warn!("listing {child_url} failed: {e}");
                        self.plan.incomplete = true;
                        self.listing_failed = true;
                    }
                }
                parser::EntryKind::File => {
                    match entry.size {
                        Some(s) => self.plan.size_sum += s,
                        None => self.plan.size_unknown = true,
                    }
                    let dest = self.storage.join(&rel);
                    if local_matches(&dest, &entry).await {
                        continue;
                    }
                    self.plan.downloads.push((join(url, &entry.path), dest));
                }
                parser::EntryKind::Symlink => {
                    // Mirrored as a local symlink, never downloaded; the
                    // entry stays in the remote set so a matching local
                    // link is neither re-created nor deleted. The target is
                    // the listing's target when it has one, else the entry
                    // name itself (tsumugu's "ignore symlinks" semantics
                    // with no target info in the listing).
                    let target = match entry.symlink_target.as_deref() {
                        Some(t) if is_safe_rel(t) => t.to_string(),
                        _ => entry.path.trim_end_matches('/').to_string(),
                    };
                    self.plan
                        .symlinks
                        .push((self.storage.join(&rel_key), target));
                }
            }
        }
        Ok(())
    }
}

/// Compare a remote file against local state: same size → unchanged (the
/// documented lazy default); otherwise, when the parser provided `modified`
/// and the local mtime matches it, treat as unchanged too. A local symlink
/// counts as "in place" (spec §103 / tsumugu: symlinks are ignored during
/// syncing) — never downloaded into, never replaced.
async fn local_matches(dest: &Path, entry: &parser::Entry) -> bool {
    let Ok(meta) = tokio::fs::symlink_metadata(dest).await else {
        return false;
    };
    if !meta.file_type().is_file() {
        // Symlink (or other non-regular file): left alone, treated as a
        // match so the planner never plans a download for it.
        return true;
    }
    // Size is authoritative when the listing carries it. Same mtime with a
    // different size is a change (OR-ing the two skipped real updates).
    match entry.size {
        Some(remote_size) => remote_size == meta.len(),
        None => {
            let Some(remote) = entry.modified else {
                return false;
            };
            let Ok(local_sys) = meta.modified() else {
                return false;
            };
            let local = time::OffsetDateTime::from(local_sys);
            time::PrimitiveDateTime::new(local.date(), local.time()) == remote
        }
    }
}

/// Walk the local mirror; anything not present in the remote index is
/// planned for deletion — regular files and symlinks alike (a stale
/// symlink is unlinked, its target untouched). Leftover dirs are ignored,
/// not deleted. DirEntry::file_type does not follow symlinks, so the walk
/// never recurses through one.
async fn local_extras(
    storage: &Path,
    remote: &HashSet<String>,
) -> Result<Vec<PathBuf>, FetchError> {
    let mut deletes = Vec::new();
    let mut stack = vec![storage.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut rd = tokio::fs::read_dir(&dir).await?;
        while let Some(entry) = rd.next_entry().await? {
            let ft = entry.file_type().await?;
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() || ft.is_symlink() {
                let path = entry.path();
                let rel = path
                    .strip_prefix(storage)
                    .map_err(|_| FetchError::Io("storage walk escaped its root".to_string()))?;
                if !remote.contains(rel.to_str().unwrap_or_default()) {
                    deletes.push(path);
                }
            }
        }
    }
    Ok(deletes)
}

/// Download one file to `dest.partial`, then rename over `dest`. Any
/// failure — including cancellation — removes the partial. Logs every
/// outcome (debug on success with url/dest/size, warn with the full error
/// chain on failure).
async fn download_one(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    cancel: &CancellationToken,
) -> Result<u64, FetchError> {
    let result = fetch_to_partial(client, url, dest, cancel).await;
    match &result {
        Ok(n) => tracing::debug!("downloaded {url} -> {} ({n} bytes)", dest.display()),
        Err(e) => tracing::warn!(
            "download failed (url={url}, dest={}): {}",
            dest.display(),
            error_chain(e)
        ),
    }
    result
}

async fn fetch_to_partial(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    cancel: &CancellationToken,
) -> Result<u64, FetchError> {
    if cancel.is_cancelled() {
        return Err(FetchError::Cancelled);
    }
    let resp = tokio::select! {
        r = client.get(url).send() => r?,
        _ = cancel.cancelled() => return Err(FetchError::Cancelled),
    };
    let mut resp = resp.error_for_status()?;
    if has_symlink_ancestor(dest) {
        return Err(FetchError::Io(format!(
            "refusing to write through a symlink: {}",
            dest.display()
        )));
    }
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let partial = partial_path(dest);
    // Drop any stale (or hostile) leftover partial first — remove_file
    // unlinks a symlink rather than following it.
    let _ = tokio::fs::remove_file(&partial).await;
    let mut out = tokio::fs::File::create(&partial).await?;
    let mut n = 0u64;
    let result = loop {
        let chunk = tokio::select! {
            r = resp.chunk() => match r {
                Ok(chunk) => chunk,
                Err(e) => break Err(FetchError::Http(e.to_string())),
            },
            _ = cancel.cancelled() => break Err(FetchError::Cancelled),
        };
        let Some(chunk) = chunk else { break Ok(()) };
        if let Err(e) = out.write_all(&chunk).await {
            break Err(FetchError::Io(e.to_string()));
        }
        n += chunk.len() as u64;
    };
    drop(out);
    match result {
        Ok(()) => {
            if cancel.is_cancelled() {
                let _ = tokio::fs::remove_file(&partial).await;
                return Err(FetchError::Cancelled);
            }
            tokio::fs::rename(&partial, dest).await.inspect_err(|_| {
                let _ = std::fs::remove_file(&partial);
            })?;
            Ok(n)
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&partial).await;
            Err(e)
        }
    }
}

/// `dest.partial`: the in-flight file lives next to its final destination so
/// the rename is atomic on the same filesystem.
fn partial_path(dest: &Path) -> PathBuf {
    PathBuf::from(format!("{}.partial", dest.display()))
}

/// Create a symlink at `dest` pointing to `target` (unix). Idempotent:
/// when `dest` is already a symlink with the same target, it is left as-is
/// (returns false). Anything else already at `dest` is left alone — never
/// clobber existing data (tsumugu's "skip symlink creation when the path
/// exists" behavior).
#[cfg(unix)]
async fn ensure_symlink(dest: &Path, target: &str) -> Result<bool, FetchError> {
    if let Ok(meta) = tokio::fs::symlink_metadata(dest).await {
        if meta.file_type().is_symlink()
            && tokio::fs::read_link(dest)
                .await
                .map(|cur| cur == Path::new(target))
                .unwrap_or(false)
        {
            return Ok(false);
        }
        tracing::warn!(
            "skipping symlink creation at {}: path exists and is not the expected symlink -> {target}",
            dest.display()
        );
        return Ok(false);
    }
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    std::os::unix::fs::symlink(target, dest)?;
    Ok(true)
}

/// True when any component of `dest`'s parent chain is a symlink. Writing
/// through a symlink would escape the mirror root, so such files are
/// refused (and skipped by the caller).
fn has_symlink_ancestor(dest: &Path) -> bool {
    let mut p = dest.parent();
    while let Some(dir) = p {
        if std::fs::symlink_metadata(dir)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return true;
        }
        p = dir.parent();
    }
    false
}

/// Display the error plus its full source chain ("a: b: c").
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut msg = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        msg.push_str(&format!(": {s}"));
        src = s.source();
    }
    msg
}

/// Join a URL and a listing-relative path with exactly one slash.
fn join(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

/// Like [`join`], but guarantees a trailing slash (directory indexes).
fn join_slash(base: &str, path: &str) -> String {
    let mut url = join(base, path);
    if !url.ends_with('/') {
        url.push('/');
    }
    url
}

/// Spec §103: entry paths must stay inside the mirror — relative only, no
/// `..` segments, no absolute paths. (Symlinks are additionally never
/// followed at compare/download time.)
fn is_safe_rel(path: &str) -> bool {
    !path.is_empty()
        && path != "."
        && !path.starts_with('/')
        && !path.split('/').any(|seg| seg == "..")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::extract::{OriginalUri, State};
    use axum::http::StatusCode;
    use axum::routing::get;
    use axum::Router;
    use std::collections::{BTreeMap, HashMap};

    // --- local test server -------------------------------------------

    /// In-memory mirror served by the test axum listener: rel path → bytes,
    /// dirs implicit, listing HTML generated on the fly.
    #[derive(Clone)]
    struct TestMirror {
        files: BTreeMap<String, Vec<u8>>,
        /// Pre-baked HTML served verbatim for a directory (hostile listings).
        raw: HashMap<String, String>,
        /// Per-file-request delay in ms (cancellation test).
        delay_ms: u64,
    }

    impl TestMirror {
        fn new(files: &[(&str, &str)]) -> Self {
            TestMirror {
                files: files
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.as_bytes().to_vec()))
                    .collect(),
                raw: HashMap::new(),
                delay_ms: 0,
            }
        }
    }

    fn listing_html(mirror: &TestMirror, dir: &str) -> String {
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{dir}/")
        };
        let mut html = format!(
            "<html><head><title>Index of /{dir}</title></head><body>\n\
             <h1>Index of /{dir}</h1><hr><pre>\n"
        );
        let mut dirs: Vec<String> = Vec::new();
        for (key, bytes) in &mirror.files {
            let Some(rest) = key.strip_prefix(&prefix) else {
                continue;
            };
            let rest = rest.trim_end_matches('/');
            if rest.is_empty() {
                continue;
            }
            if let Some((sub, _)) = rest.split_once('/') {
                if !dirs.iter().any(|d| d == sub) {
                    dirs.push(sub.to_string());
                }
            } else {
                html.push_str(&format!(
                    "<a href=\"{rest}\">{rest}</a>  16-Aug-2026 10:00  {}\n",
                    bytes.len()
                ));
            }
        }
        for d in dirs {
            html.push_str(&format!(
                "<a href=\"{d}/\">{d}/</a>  16-Aug-2026 10:00  -\n"
            ));
        }
        html.push_str("</pre><hr></body></html>");
        html
    }

    fn spawn_server(mirror: TestMirror) -> String {
        let app = Router::new()
            .route("/{*path}", get(handler))
            .fallback(get(handler))
            .with_state(mirror);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("test server");
        });
        format!("http://{addr}")
    }

    async fn handler(uri: OriginalUri, State(mirror): State<TestMirror>) -> (StatusCode, Body) {
        let path = uri.path().trim_matches('/').to_string();
        if mirror.delay_ms > 0 && mirror.files.contains_key(&path) {
            tokio::time::sleep(std::time::Duration::from_millis(mirror.delay_ms)).await;
        }
        if let Some(bytes) = mirror.files.get(&path) {
            return (StatusCode::OK, Body::from(bytes.clone()));
        }
        if let Some(html) = mirror.raw.get(&path) {
            return (StatusCode::OK, Body::from(html.clone()));
        }
        let prefix = if path.is_empty() {
            String::new()
        } else {
            format!("{path}/")
        };
        if mirror.files.keys().any(|k| k.starts_with(&prefix)) {
            return (StatusCode::OK, Body::from(listing_html(&mirror, &path)));
        }
        (StatusCode::NOT_FOUND, Body::from("not found"))
    }

    // --- helpers ------------------------------------------------------

    fn unique_dir() -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("synora-httpfetch-test-{}", std::process::id()));
        let dir = root.join(format!(
            "{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn listing_date() -> time::PrimitiveDateTime {
        time::PrimitiveDateTime::parse(
            "2026-08-16 10:00:00",
            &time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second]"),
        )
        .unwrap()
    }

    fn set_mtime(p: &Path, dt: time::PrimitiveDateTime) {
        let sys: std::time::SystemTime = dt.assume_utc().into();
        std::fs::File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(sys)
            .unwrap();
    }

    // --- tests --------------------------------------------------------

    #[test]
    fn with_proxy_rejects_socks() {
        let err = match Fetcher::with_proxy(Some("socks5h://127.0.0.1:40000")) {
            Ok(_) => panic!("expected SOCKS proxy to be rejected"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(msg.contains("SOCKS"), "{msg}");
        assert!(msg.contains("HTTP CONNECT"), "{msg}");
    }

    #[test]
    fn listing_error_classifies_proxy_blips() {
        assert!(listing_error_transient(&FetchError::Http(
            "error sending request for url (https://example/)".into(),
        )));
        assert!(listing_error_transient(&FetchError::Http(
            "http error: connection refused".into(),
        )));
        assert!(!listing_error_transient(&FetchError::Http(
            "404 Not Found".into()
        )));
    }

    #[tokio::test]
    async fn sync_downloads_full_tree() {
        let mirror = TestMirror::new(&[
            ("hello.txt", "hello"),
            ("sub/world.txt", "world"),
            ("sub/deep/notes.md", "note!"),
        ]);
        let base = spawn_server(mirror);
        let storage = unique_dir();
        let fetcher = Fetcher::new().unwrap();
        let cancel = CancellationToken::new();
        let stats = fetcher
            .sync(&base, "nginx", &storage, false, 3, &cancel, None)
            .await
            .unwrap();
        assert_eq!(stats.files_downloaded, 3);
        assert_eq!(stats.downloaded_bytes, 15);
        assert_eq!(stats.files_deleted, 0);
        assert_eq!(stats.files_skipped, 0);
        assert_eq!(stats.total_size_hint, Some(15));
        for (rel, want) in [
            ("hello.txt", "hello"),
            ("sub/world.txt", "world"),
            ("sub/deep/notes.md", "note!"),
        ] {
            let got = tokio::fs::read_to_string(storage.join(rel)).await.unwrap();
            assert_eq!(got, want, "{rel}");
        }
        // downloaded via dest.partial → rename; nothing left behind
        assert!(!storage.join("hello.txt.partial").exists());
        assert!(!storage.join("sub/world.txt.partial").exists());
    }

    #[tokio::test]
    async fn plan_skips_unchanged_by_size_and_mtime() {
        let mirror = TestMirror::new(&[
            ("hello.txt", "hello"),
            ("sub/world.txt", "world"),
            ("sub/deep/notes.md", "note!"),
        ]);
        let base = spawn_server(mirror);
        let storage = unique_dir();
        std::fs::create_dir_all(storage.join("sub/deep")).unwrap();
        // Same size, different content, different mtime → skipped by size.
        std::fs::write(storage.join("hello.txt"), "HOLA!").unwrap();
        // Different size, different mtime → downloaded.
        std::fs::write(storage.join("sub/world.txt"), "world!").unwrap();
        // Different size, even if mtime matches the listing date → download.
        std::fs::write(storage.join("sub/deep/notes.md"), "LOCAL-NOTES").unwrap();
        set_mtime(&storage.join("sub/deep/notes.md"), listing_date());
        let fetcher = Fetcher::new().unwrap();
        let cancel = CancellationToken::new();
        let stats = fetcher
            .sync(&base, "nginx", &storage, false, 3, &cancel, None)
            .await
            .unwrap();
        assert_eq!(stats.files_downloaded, 2);
        assert_eq!(stats.total_size_hint, Some(15));
        assert_eq!(
            tokio::fs::read_to_string(storage.join("hello.txt"))
                .await
                .unwrap(),
            "HOLA!"
        );
        assert_eq!(
            tokio::fs::read_to_string(storage.join("sub/world.txt"))
                .await
                .unwrap(),
            "world"
        );
        assert_eq!(
            tokio::fs::read_to_string(storage.join("sub/deep/notes.md"))
                .await
                .unwrap(),
            "note!"
        );
    }

    #[tokio::test]
    async fn delete_removes_local_extras() {
        let mirror = TestMirror::new(&[("hello.txt", "hello")]);
        let base = spawn_server(mirror);
        let storage = unique_dir();
        std::fs::write(storage.join("hello.txt"), "hello").unwrap();
        std::fs::write(storage.join("stale.txt"), "stale").unwrap();
        let fetcher = Fetcher::new().unwrap();
        let cancel = CancellationToken::new();
        // delete=false: extras stay.
        let stats = fetcher
            .sync(&base, "nginx", &storage, false, 2, &cancel, None)
            .await
            .unwrap();
        assert_eq!(stats.files_deleted, 0);
        assert!(storage.join("stale.txt").exists());
        // delete=true: stale.txt is gone, hello.txt (still upstream) stays.
        let stats = fetcher
            .sync(&base, "nginx", &storage, true, 2, &cancel, None)
            .await
            .unwrap();
        assert_eq!(stats.files_deleted, 1);
        assert!(!storage.join("stale.txt").exists());
        assert!(storage.join("hello.txt").exists());
    }

    #[tokio::test]
    async fn failed_single_file_never_aborts_sync() {
        // The listing advertises broken.bin, but the server 404s it:
        // that file is skipped, the rest of the mirror still syncs.
        let mut mirror = TestMirror::new(&[("ok.txt", "ok!")]);
        mirror.raw.insert(
            String::new(),
            "<pre>\n<a href=\"ok.txt\">ok.txt</a>  16-Aug-2026 10:00  3\n\
             <a href=\"broken.bin\">broken.bin</a>  16-Aug-2026 10:00  10\n</pre>"
                .to_string(),
        );
        let base = spawn_server(mirror);
        let storage = unique_dir();
        let fetcher = Fetcher::new().unwrap();
        let cancel = CancellationToken::new();
        let stats = fetcher
            .sync(&base, "nginx", &storage, false, 2, &cancel, None)
            .await
            .unwrap();
        assert_eq!(stats.files_downloaded, 1);
        assert_eq!(stats.files_skipped, 1);
        assert_eq!(
            tokio::fs::read_to_string(storage.join("ok.txt"))
                .await
                .unwrap(),
            "ok!"
        );
        assert!(!storage.join("broken.bin").exists());
        assert!(!storage.join("broken.bin.partial").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_symlinks_are_left_alone_and_strays_deleted() {
        let mirror = TestMirror::new(&[("lnk.txt", "remote-content")]);
        let base = spawn_server(mirror);
        let storage = unique_dir();
        let outside = unique_dir();
        let target = outside.join("target.txt");
        std::fs::write(&target, "TARGET-CONTENT").unwrap();
        // lnk.txt is a symlink pointing outside the mirror: it must NOT be
        // downloaded into or replaced (tsumugu: symlinks are ignored).
        std::os::unix::fs::symlink(&target, storage.join("lnk.txt")).unwrap();
        // stray.lnk is a symlink absent from the index: with delete=true it
        // is unlinked (its target is never touched).
        std::os::unix::fs::symlink(&target, storage.join("stray.lnk")).unwrap();
        let fetcher = Fetcher::new().unwrap();
        let cancel = CancellationToken::new();
        let stats = fetcher
            .sync(&base, "nginx", &storage, true, 2, &cancel, None)
            .await
            .unwrap();
        assert_eq!(stats.files_downloaded, 0);
        assert_eq!(stats.files_deleted, 1);
        // lnk.txt is still the symlink (same target, untouched).
        assert_eq!(std::fs::read_link(storage.join("lnk.txt")).unwrap(), target);
        // The symlink target was never touched; the stray symlink is gone.
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "TARGET-CONTENT");
        assert!(std::fs::symlink_metadata(storage.join("stray.lnk")).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_entries_are_planned_and_created() {
        // fancyindex listing: `latest@/` is a symlink (dir target unknown).
        let mut mirror = TestMirror::new(&[("v18.6/readme.txt", "18.6")]);
        mirror.raw.insert(
            String::new(),
            "<table class=\"fancy\">\n\
             <tr><td class=\"n\"><a href=\"v18.6/\">v18.6/</a></td><td class=\"m\">2026-08-11 18:42</td><td class=\"s\">-</td></tr>\n\
             <tr><td class=\"n\"><a href=\"latest\">latest@/</a></td><td class=\"m\">2026-08-13 12:53</td><td class=\"s\">-</td></tr>\n\
             <tr><td class=\"n\"><a href=\"README\">README</a></td><td class=\"m\">2026-08-13 12:53</td><td class=\"s\">1731</td></tr>\n\
             </table>"
                .to_string(),
        );
        let base = spawn_server(mirror);
        let storage = unique_dir();
        let fetcher = Fetcher::new().unwrap();
        let cancel = CancellationToken::new();
        // Symlink entry goes into plan.symlinks, never into downloads.
        let plan = fetcher
            .plan(&base, "nginx", &storage, false, 2, None)
            .await
            .unwrap();
        assert_eq!(
            plan.symlinks,
            vec![(storage.join("latest"), "latest".to_string())]
        );
        assert!(plan
            .downloads
            .iter()
            .all(|(url, _)| !url.contains("latest")));
        let stats = fetcher.execute(&plan, &cancel, None).await.unwrap();
        assert_eq!(stats.files_symlinked, 1);
        assert_eq!(
            std::fs::read_link(storage.join("latest")).unwrap(),
            PathBuf::from("latest")
        );
        // Second run: same-target link is left in place (idempotent).
        let plan = fetcher
            .plan(&base, "nginx", &storage, false, 2, None)
            .await
            .unwrap();
        let stats = fetcher.execute(&plan, &cancel, None).await.unwrap();
        assert_eq!(stats.files_symlinked, 0);
        assert_eq!(
            std::fs::read_link(storage.join("latest")).unwrap(),
            PathBuf::from("latest")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_entry_never_clobbers_existing_path() {
        let mut mirror = TestMirror::new(&[("v18.6/readme.txt", "18.6")]);
        mirror.raw.insert(
            String::new(),
            "<table class=\"fancy\">\n\
             <tr><td class=\"n\"><a href=\"latest\">latest@/</a></td><td class=\"m\">2026-08-13 12:53</td><td class=\"s\">-</td></tr>\n\
             </table>"
                .to_string(),
        );
        let base = spawn_server(mirror);
        let storage = unique_dir();
        // A regular file squatting on the symlink's path is kept (warned
        // about), never replaced — data safety over mirror fidelity.
        std::fs::write(storage.join("latest"), "keep me").unwrap();
        let fetcher = Fetcher::new().unwrap();
        let cancel = CancellationToken::new();
        let stats = fetcher
            .sync(&base, "nginx", &storage, false, 2, &cancel, None)
            .await
            .unwrap();
        assert_eq!(stats.files_symlinked, 0);
        assert_eq!(
            tokio::fs::read_to_string(storage.join("latest"))
                .await
                .unwrap(),
            "keep me"
        );
    }

    #[tokio::test]
    async fn same_size_local_file_is_not_planned_for_download() {
        let mirror = TestMirror::new(&[("hello.txt", "hello"), ("sub/world.txt", "world")]);
        let base = spawn_server(mirror);
        let storage = unique_dir();
        std::fs::create_dir_all(storage.join("sub")).unwrap();
        // Identical size (different content is irrelevant — size is the
        // documented lazy compare).
        std::fs::write(storage.join("hello.txt"), "HELLO").unwrap();
        std::fs::write(storage.join("sub/world.txt"), "WORLD").unwrap();
        let fetcher = Fetcher::new().unwrap();
        let plan = fetcher
            .plan(&base, "nginx", &storage, false, 2, None)
            .await
            .unwrap();
        assert!(
            plan.downloads.is_empty(),
            "size-identical files must not enter the downloads list: {:?}",
            plan.downloads
        );
    }

    #[tokio::test]
    async fn threads_zero_is_clamped_and_sync_still_works() {
        let mirror = TestMirror::new(&[("a.txt", "a"), ("b.txt", "b")]);
        let base = spawn_server(mirror);
        let storage = unique_dir();
        // A 0-permit semaphore would deadlock; with_threads clamps to 1.
        let fetcher = Fetcher::new().unwrap().with_threads(0);
        assert_eq!(fetcher.threads, 1);
        let cancel = CancellationToken::new();
        let stats = fetcher
            .sync(&base, "nginx", &storage, false, 2, &cancel, None)
            .await
            .unwrap();
        assert_eq!(stats.files_downloaded, 2);
        // threads = 1 works too.
        let storage2 = unique_dir();
        let stats = Fetcher::new()
            .unwrap()
            .with_threads(1)
            .sync(&base, "nginx", &storage2, false, 2, &cancel, None)
            .await
            .unwrap();
        assert_eq!(stats.files_downloaded, 2);
        // Default stays DEFAULT_THREADS.
        assert_eq!(Fetcher::new().unwrap().threads, DEFAULT_THREADS);
    }

    #[tokio::test]
    async fn unsafe_entry_paths_are_skipped() {
        let mut mirror = TestMirror::new(&[("fine.txt", "fine")]);
        mirror.raw.insert(
            String::new(),
            "<pre>\n<a href=\"/etc/passwd\">/etc/passwd</a>  16-Aug-2026 10:00  1\n\
             <a href=\"../../evil\">../../evil</a>  16-Aug-2026 10:00  1\n\
             <a href=\"fine.txt\">fine.txt</a>  16-Aug-2026 10:00  4\n</pre>"
                .to_string(),
        );
        let base = spawn_server(mirror);
        let storage = unique_dir();
        let fetcher = Fetcher::new().unwrap();
        let plan = fetcher
            .plan(&base, "nginx", &storage, false, 2, None)
            .await
            .unwrap();
        assert_eq!(plan.downloads.len(), 1);
        assert_eq!(plan.downloads[0].0, format!("{base}/fine.txt"));
        assert_eq!(plan.downloads[0].1, storage.join("fine.txt"));
    }

    #[tokio::test]
    async fn missing_subdirectory_listing_is_nonfatal() {
        let mut mirror = TestMirror::new(&[("ok.txt", "ok!")]);
        mirror.raw.insert(
            String::new(),
            "<pre>\n<a href=\"ok.txt\">ok.txt</a>  16-Aug-2026 10:00  3\n\
             <a href=\"gone/\">gone/</a>  16-Aug-2026 10:00  -\n</pre>"
                .to_string(),
        );
        let base = spawn_server(mirror);
        let storage = unique_dir();
        let fetcher = Fetcher::new().unwrap();
        let plan = fetcher
            .plan(&base, "nginx", &storage, false, 2, None)
            .await
            .unwrap();
        assert_eq!(plan.downloads.len(), 1);
        assert_eq!(plan.downloads[0].0, format!("{base}/ok.txt"));
        // delete=true must refuse rather than treat missing children as extras.
        let err = fetcher
            .plan(&base, "nginx", &storage, true, 2, None)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("refusing delete") || msg.contains("listing failed"),
            "unexpected error: {msg}"
        );
    }

    #[tokio::test]
    async fn depth_zero_does_not_recurse() {
        let mirror = TestMirror::new(&[("hello.txt", "hello"), ("sub/world.txt", "world")]);
        let base = spawn_server(mirror);
        let storage = unique_dir();
        let fetcher = Fetcher::new().unwrap();
        let plan = fetcher
            .plan(&base, "nginx", &storage, false, 0, None)
            .await
            .unwrap();
        assert_eq!(plan.downloads.len(), 1);
        assert_eq!(plan.downloads[0].0, format!("{base}/hello.txt"));
    }

    #[tokio::test]
    async fn cancel_mid_download_returns_cancelled() {
        let mut mirror = TestMirror::new(&[("big.bin", &"x".repeat(8 * 1024 * 1024))]);
        mirror.delay_ms = 1000;
        let base = spawn_server(mirror);
        let storage = unique_dir();
        let fetcher = Fetcher::new().unwrap();
        let cancel = CancellationToken::new();
        let plan = fetcher
            .plan(&base, "nginx", &storage, false, 2, None)
            .await
            .unwrap();
        assert_eq!(plan.downloads.len(), 1);
        let cancel2 = cancel.clone();
        let handle =
            tokio::spawn(
                async move { Fetcher::new().unwrap().execute(&plan, &cancel2, None).await },
            );
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        cancel.cancel();
        let result = handle.await.unwrap();
        assert!(matches!(result, Err(FetchError::Cancelled)), "{result:?}");
        // partial is cleaned up, nothing half-renamed in place
        assert!(!storage.join("big.bin.partial").exists());
        assert!(!storage.join("big.bin").exists());
    }

    #[tokio::test]
    async fn unknown_parser_is_an_error() {
        let fetcher = Fetcher::new().unwrap();
        let cancel = CancellationToken::new();
        let err = fetcher
            .sync(
                "http://127.0.0.1:1",
                "bogus",
                &unique_dir(),
                false,
                1,
                &cancel,
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, FetchError::Http(_)), "{err:?}");
    }
}
