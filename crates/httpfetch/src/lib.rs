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
use std::sync::Arc;

use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

/// Upper bound on index entries visited while planning, so a hostile or
/// runaway index can't spin the planner forever.
const MAX_ENTRIES: usize = 200_000;
/// Hard cap on recursion depth even when the caller asks for more.
const MAX_DEPTH: u32 = 16;
/// Max concurrent downloads during [`Fetcher::execute`].
const MAX_CONCURRENT: usize = 8;
/// Per-request timeout (listings and downloads alike); a timed-out file is
/// skipped like any other single-file failure.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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
    /// Files skipped after a per-file failure (download error, timeout,
    /// failed delete) — a single bad file never aborts a sync.
    pub files_skipped: u32,
    /// Sum of remote sizes of everything planned for download; `None` when
    /// any planned file's size was unknown.
    pub total_size_hint: Option<u64>,
}

/// A planned sync: what to download (url → destination) and what to delete.
#[derive(Debug, Default)]
pub struct Plan {
    pub downloads: Vec<(String, PathBuf)>,
    pub deletes: Vec<PathBuf>,
    // Planning internals feeding `total_size_hint`; not part of the public
    // shape so users can't fake a hint.
    size_sum: u64,
    size_unknown: bool,
    total_size_hint: Option<u64>,
}

/// HTTP fetcher: a reqwest client with rustls, redirects followed (max 10),
/// no proxy by default, 30 s per-request timeout.
pub struct Fetcher {
    client: reqwest::Client,
}

impl Fetcher {
    pub fn new() -> Result<Self, FetchError> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::limited(10))
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        Ok(Self { client })
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
    pub async fn plan(
        &self,
        base_url: &str,
        parser_name: &str,
        storage: &Path,
        delete: bool,
        depth: u32,
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
        };
        planner.dir(base_url, "", depth.min(MAX_DEPTH)).await?;
        if delete {
            plan.deletes = local_extras(storage, &remote).await?;
        }
        plan.total_size_hint = if plan.size_unknown {
            None
        } else {
            Some(plan.size_sum)
        };
        Ok(plan)
    }

    /// Execute a plan concurrently (max [`MAX_CONCURRENT`] in-flight, via a
    /// tokio semaphore) with a cancel token. Each file downloads to
    /// `dest.partial` then renames into place; a failed file is warned about
    /// and skipped, never fatal — only cancellation aborts the run. Summed
    /// stats are returned.
    pub async fn execute(
        &self,
        plan: &Plan,
        cancel: &CancellationToken,
    ) -> Result<FetchStats, FetchError> {
        if cancel.is_cancelled() {
            return Err(FetchError::Cancelled);
        }
        let mut stats = FetchStats {
            total_size_hint: plan.total_size_hint,
            ..FetchStats::default()
        };
        let sem = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT));
        let mut tasks = Vec::with_capacity(plan.downloads.len());
        for (url, dest) in &plan.downloads {
            let client = self.client.clone();
            let sem = Arc::clone(&sem);
            let cancel = cancel.clone();
            let url = url.clone();
            let dest = dest.clone();
            tasks.push(tokio::spawn(async move {
                let _permit = sem
                    .acquire_owned()
                    .await
                    .map_err(|_| FetchError::Cancelled)?;
                download_one(&client, &url, &dest, &cancel).await
            }));
        }
        // Single-file failures must not sink the whole sync: warn (done in
        // download_one) and carry on; only cancellation aborts.
        let mut cancelled = false;
        for task in tasks {
            match task.await {
                Ok(Ok(bytes)) => {
                    stats.downloaded_bytes += bytes;
                    stats.files_downloaded += 1;
                }
                Ok(Err(FetchError::Cancelled)) => cancelled = true,
                Ok(Err(_e)) => stats.files_skipped += 1,
                Err(_) => {
                    tracing::warn!("download task panicked");
                    stats.files_skipped += 1;
                }
            }
        }
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
                continue;
            }
            stats.files_deleted += 1;
        }
        Ok(stats)
    }

    /// Convenience: [`Fetcher::plan`] then [`Fetcher::execute`].
    pub async fn sync(
        &self,
        base_url: &str,
        parser_name: &str,
        storage: &Path,
        delete: bool,
        depth: u32,
        cancel: &CancellationToken,
    ) -> Result<FetchStats, FetchError> {
        let plan = self
            .plan(base_url, parser_name, storage, delete, depth)
            .await?;
        self.execute(&plan, cancel).await
    }
}

impl Default for Fetcher {
    fn default() -> Self {
        Self::new().expect("reqwest client with default TLS setup cannot fail to build")
    }
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
}

impl Planner<'_> {
    async fn dir(&mut self, url: &str, dir_rel: &str, depth: u32) -> Result<(), FetchError> {
        let body = self
            .fetcher
            .client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        for entry in self.parser.parse(&body) {
            if self.seen >= MAX_ENTRIES {
                tracing::warn!("index entry cap ({MAX_ENTRIES}) reached at {url}");
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
            self.remote.insert(rel.trim_end_matches('/').to_string());
            match entry.kind {
                parser::EntryKind::Dir => {
                    if depth == 0 {
                        continue;
                    }
                    let child_url = join_slash(url, &entry.path);
                    let child_rel = rel.trim_end_matches('/').to_string();
                    let result = Box::pin(self.dir(&child_url, &child_rel, depth - 1)).await;
                    if let Err(e) = result {
                        // A broken subdirectory must not sink the whole plan.
                        tracing::warn!("listing {child_url} failed: {e}");
                    }
                }
                parser::EntryKind::File => {
                    let dest = self.storage.join(&rel);
                    if local_matches(&dest, &entry).await {
                        continue;
                    }
                    match entry.size {
                        Some(s) => self.plan.size_sum += s,
                        None => self.plan.size_unknown = true,
                    }
                    self.plan.downloads.push((join(url, &entry.path), dest));
                }
            }
        }
        Ok(())
    }
}

/// Compare a remote file against local state: same size → unchanged (the
/// documented lazy default); otherwise, when the parser provided `modified`
/// and the local mtime matches it, treat as unchanged too. Symlinks are
/// never followed (spec §103 / tsumugu): a symlink is not a matching copy,
/// so it gets replaced by a regular file on the next sync.
async fn local_matches(dest: &Path, entry: &parser::Entry) -> bool {
    let Ok(meta) = tokio::fs::symlink_metadata(dest).await else {
        return false;
    };
    if !meta.file_type().is_file() {
        // Symlink (or other non-regular file): never treated as a copy.
        return false;
    }
    if entry.size == Some(meta.len()) {
        return true;
    }
    let Some(remote) = entry.modified else {
        return false;
    };
    let Ok(local_sys) = meta.modified() else {
        return false;
    };
    let local = time::OffsetDateTime::from(local_sys);
    time::PrimitiveDateTime::new(local.date(), local.time()) == remote
}

/// Walk the local mirror; anything not present in the remote index is
/// planned for deletion. Symlinks and leftover dirs are ignored, not
/// deleted (tsumugu-style).
async fn local_extras(
    storage: &Path,
    remote: &HashSet<String>,
) -> Result<Vec<PathBuf>, FetchError> {
    let mut deletes = Vec::new();
    let mut stack = vec![storage.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut rd = tokio::fs::read_dir(&dir).await?;
        while let Some(entry) = rd.next_entry().await? {
            // DirEntry::file_type does not follow symlinks.
            let ft = entry.file_type().await?;
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() {
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
            .sync(&base, "nginx", &storage, false, 3, &cancel)
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
        // Different size, but mtime matches the listing date → skipped.
        std::fs::write(storage.join("sub/deep/notes.md"), "LOCAL-NOTES").unwrap();
        set_mtime(&storage.join("sub/deep/notes.md"), listing_date());
        let fetcher = Fetcher::new().unwrap();
        let cancel = CancellationToken::new();
        let stats = fetcher
            .sync(&base, "nginx", &storage, false, 3, &cancel)
            .await
            .unwrap();
        assert_eq!(stats.files_downloaded, 1);
        assert_eq!(stats.downloaded_bytes, 5);
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
            "LOCAL-NOTES"
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
            .sync(&base, "nginx", &storage, false, 2, &cancel)
            .await
            .unwrap();
        assert_eq!(stats.files_deleted, 0);
        assert!(storage.join("stale.txt").exists());
        // delete=true: stale.txt is gone, hello.txt (still upstream) stays.
        let stats = fetcher
            .sync(&base, "nginx", &storage, true, 2, &cancel)
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
            .sync(&base, "nginx", &storage, false, 2, &cancel)
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
    async fn symlinks_are_ignored_and_replaced_by_regular_files() {
        let mirror = TestMirror::new(&[("lnk.txt", "remote-content")]);
        let base = spawn_server(mirror);
        let storage = unique_dir();
        let outside = unique_dir();
        let target = outside.join("target.txt");
        std::fs::write(&target, "TARGET-CONTENT").unwrap();
        // lnk.txt is a symlink pointing outside the mirror: it must be
        // replaced by a regular file, not written through.
        std::os::unix::fs::symlink(&target, storage.join("lnk.txt")).unwrap();
        // stray.lnk is a symlink absent from the index: never deleted.
        std::os::unix::fs::symlink(&target, storage.join("stray.lnk")).unwrap();
        let fetcher = Fetcher::new().unwrap();
        let cancel = CancellationToken::new();
        let stats = fetcher
            .sync(&base, "nginx", &storage, true, 2, &cancel)
            .await
            .unwrap();
        assert_eq!(stats.files_downloaded, 1);
        assert_eq!(stats.files_deleted, 0);
        // lnk.txt is now a regular file with remote content.
        let meta = std::fs::symlink_metadata(storage.join("lnk.txt")).unwrap();
        assert!(meta.file_type().is_file(), "symlink must be replaced");
        assert_eq!(
            tokio::fs::read_to_string(storage.join("lnk.txt"))
                .await
                .unwrap(),
            "remote-content"
        );
        // The symlink target was never touched; the stray symlink survived.
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "TARGET-CONTENT");
        assert!(std::fs::symlink_metadata(storage.join("stray.lnk"))
            .unwrap()
            .file_type()
            .is_symlink());
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
            .plan(&base, "nginx", &storage, false, 2)
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
            .plan(&base, "nginx", &storage, false, 2)
            .await
            .unwrap();
        assert_eq!(plan.downloads.len(), 1);
        assert_eq!(plan.downloads[0].0, format!("{base}/ok.txt"));
    }

    #[tokio::test]
    async fn depth_zero_does_not_recurse() {
        let mirror = TestMirror::new(&[("hello.txt", "hello"), ("sub/world.txt", "world")]);
        let base = spawn_server(mirror);
        let storage = unique_dir();
        let fetcher = Fetcher::new().unwrap();
        let plan = fetcher
            .plan(&base, "nginx", &storage, false, 0)
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
            .plan(&base, "nginx", &storage, false, 2)
            .await
            .unwrap();
        assert_eq!(plan.downloads.len(), 1);
        let cancel2 = cancel.clone();
        let handle =
            tokio::spawn(async move { Fetcher::new().unwrap().execute(&plan, &cancel2).await });
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
            )
            .await
            .unwrap_err();
        assert!(matches!(err, FetchError::Http(_)), "{err:?}");
    }
}
