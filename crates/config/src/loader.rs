//! Config loading pipeline (spec §42–§44):
//! defaults → main TOML → included TOMLs (in listed order, later wins)
//! → env `SYNORA_*` → CLI overrides → validate → `ResolvedConfig`.
//!
//! Includes support globs, relative paths (resolved against the including
//! file's dir), absolute paths, nesting, and cycle detection. A file may
//! contain a bare job table (`name` + `provider` at root) — the whole file
//! is then one job — or `[[jobs]]` entries.

use crate::error::ConfigError;
use crate::schema::{DbDoc, JobDoc, RootDoc, TomlDuration};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use synora_core::job::{
    Hooks, JobSpec, MisfirePolicy, OnWorkerLost, ProviderConfig, Safety, StatisticsMode,
};
use synora_core::schedule::{self, Schedule, ScheduleKind};
use time::Duration;

const MAX_INCLUDE_DEPTH: usize = 32;

#[derive(Debug)]
pub struct ResolvedConfig {
    pub version: u64,
    pub daemon: DaemonConfig,
    pub api: ApiConfig,
    pub jobs: Vec<JobSpec>,
    /// Typed P2+ sections.
    pub proxies: HashMap<String, ProxyConfig>,
    pub proxy_groups: HashMap<String, ProxyGroupConfig>,
    pub egresses: Vec<EgressConfig>,
    pub egress_groups: HashMap<String, EgressGroupConfig>,
    pub storages: HashMap<String, StorageConfig>,
    pub cgroup: Option<CgroupConfig>,
    pub snapshot_retention: synora_core::RetentionPolicy,
    pub notifications: NotificationConfig,
    pub groups: HashMap<String, Vec<String>>,
    pub min_free_bytes: Option<u64>,
    /// Remaining untyped sections.
    pub extras: HashMap<String, toml::Value>,
}

/// One proxy definition (spec §20/§22): HTTP forward proxy, or a command
/// that reports liveness, or plain direct.
#[derive(Debug, Clone, PartialEq)]
pub struct ProxyConfig {
    pub kind: ProxyKind,
    /// Liveness probe URL (HTTP proxies): GET through the proxy.
    pub healthcheck: Option<String>,
    pub timeout: u64,
    /// Local listener to expose when "exposed" from the TUI
    /// (e.g. "127.0.0.1:4000" for a local CF One / WARP endpoint).
    pub expose: Option<String>,
    /// Optional "user:pass" for the exposed HTTP CONNECT port. Empty means
    /// unauthenticated CONNECT (required for rsync `RSYNC_PROXY=host:port`).
    pub expose_auth: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProxyKind {
    Direct,
    /// http:// or socks5h:// forward proxy.
    Forward {
        url: String,
        env: Vec<(String, String)>,
    },
    Command {
        check: String,
        env: Vec<(String, String)>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProxyGroupConfig {
    pub proxies: Vec<String>,
    /// fixed | failover | round-robin | random
    pub strategy: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EgressConfig {
    pub name: String,
    pub address: std::net::IpAddr,
    /// Optional TCP probe target (spec §63), e.g. "1.1.1.1:443".
    pub probe: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EgressGroupConfig {
    pub addresses: Vec<String>,
    pub strategy: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StorageConfig {
    pub kind: StorageKind,
    pub mountpoint: Option<PathBuf>,
    pub auto_create: bool,
    pub require_empty: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StorageKind {
    Dir,
    Zfs {
        pool: String,
        dataset: String,
        /// Extra `zfs create -o key=value` options (user-requested).
        options: Vec<(String, String)>,
    },
    Btrfs {
        subvol: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct CgroupConfig {
    /// cgroup v2 base path (default /sys/fs/cgroup/synora).
    pub base_path: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct NotificationConfig {
    pub webhook_url: Option<String>,
    /// Consecutive failures before the first alert (spec §91).
    pub alert_after_failures: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonConfig {
    pub max_concurrency: u32,
    pub db: DbConfig,
    pub log_dir: PathBuf,
    pub default_proxy: Option<String>,
    pub default_worker: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbConfig {
    pub kind: DbKind,
    /// SQLite file path.
    pub path: String,
    /// PostgreSQL URL; required when kind = postgres.
    pub url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbKind {
    Sqlite,
    Postgres,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiConfig {
    pub listen: SocketAddr,
    pub tls: TlsConfig,
    pub tokens: Vec<ApiToken>,
    pub synora_json_path: String,
    pub tunasync_json_path: String,

    /// Which status JSON shape to expose: "synora", "tunasync", or "both".
    pub status_format: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TlsConfig {
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
    pub client_ca: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiToken {
    pub name: String,
    pub token: String,
    pub role: String,
    pub permissions: Vec<String>,
}

/// Last-resort overrides from the CLI layer.
#[derive(Debug, Default, Clone)]
pub struct CliOverrides {
    pub db_kind: Option<String>,
    pub db_path: Option<String>,
    pub db_url: Option<String>,
    pub api_listen: Option<String>,
}

/// One job extracted from a file, with its provenance for `file:line` errors.
struct JobEntry {
    doc: JobDoc,
    file: String,
    line: usize,
}

struct LoadState {
    /// Files on the current include path (for cycle detection).
    stack: HashSet<PathBuf>,
    jobs: Vec<JobEntry>,
}

pub struct ConfigLoader;

impl ConfigLoader {
    pub fn load(
        config_path: &Path,
        overrides: &CliOverrides,
    ) -> Result<ResolvedConfig, ConfigError> {
        let root = config_path.canonicalize().map_err(|e| {
            ConfigError::new(
                config_path.display().to_string(),
                0,
                format!("cannot read config: {e}"),
            )
        })?;
        let mut state = LoadState {
            stack: HashSet::new(),
            jobs: Vec::new(),
        };
        let mut merged: Option<toml_edit::DocumentMut> = None;
        load_file(&root, &mut state, &mut merged, 0)?;

        let root: RootDoc = deserialize_root(merged.as_ref())?;
        let mut cfg = resolve(&root, state.jobs)?;
        apply_env_overrides(&mut cfg)?;
        apply_cli_overrides(&mut cfg, overrides)?;
        Ok(cfg)
    }
}

fn deserialize_root(doc: Option<&toml_edit::DocumentMut>) -> Result<RootDoc, ConfigError> {
    let text = doc.map(|d| d.to_string()).unwrap_or_default();
    // serde errors here lack file:line (the doc is a merge of many files);
    // the message names the offending field, which is actionable.
    toml::from_str(&text).map_err(|e| ConfigError::new("<config>", 0, e.message().to_string()))
}

/// Load one file: env-expand → parse → extract include + jobs → merge own
/// content → recurse into includes (children merge on top ⇒ included wins,
/// per the layering order "main → included").
fn load_file(
    path: &Path,
    state: &mut LoadState,
    merged: &mut Option<toml_edit::DocumentMut>,
    depth: usize,
) -> Result<(), ConfigError> {
    let file = path.display().to_string();
    if depth > MAX_INCLUDE_DEPTH {
        return Err(ConfigError::new(
            file,
            0,
            "include nesting too deep (cycle?)",
        ));
    }
    if state.stack.contains(path) {
        return Err(ConfigError::new(
            file,
            0,
            format!("include cycle detected at {}", path.display()),
        ));
    }
    state.stack.insert(path.to_path_buf());

    let text = std::fs::read_to_string(path)
        .map_err(|e| ConfigError::new(&file, 0, format!("cannot read file: {e}")))?;
    let text = expand_env(&text, &file)?;

    // First parse: immutable doc — spans survive, giving us file:line for jobs.
    let im_parsed: Result<toml_edit::ImDocument<String>, toml_edit::TomlError> = text.parse();
    let im = im_parsed.map_err(|e| {
        ConfigError::new(
            &file,
            line_of(&text, e.span()),
            format!("TOML syntax error: {e}"),
        )
    })?;
    let job_lines = job_line_numbers(&im, &text);
    let include_line = im
        .as_table()
        .get("include")
        .and_then(|i| i.span())
        .map(|s| line_of(&text, Some(s)))
        .unwrap_or(0);
    // Second parse: mutable doc for include/job extraction and merging.
    let doc_parsed: Result<toml_edit::DocumentMut, toml_edit::TomlError> = text.parse();
    let mut doc = doc_parsed.map_err(|e| {
        ConfigError::new(
            &file,
            line_of(&text, e.span()),
            format!("TOML syntax error: {e}"),
        )
    })?;

    let include = extract_include(&mut doc, &file, include_line)?;
    extract_jobs(&mut doc, &file, &job_lines, &mut state.jobs)?;

    // Own content first (lowest precedence), then includes.
    merge_doc(merged, doc);

    let base_dir = path.parent().unwrap_or(Path::new("."));
    for pattern in &include {
        let files = resolve_include(pattern, base_dir, &file)?;
        for f in files {
            load_file(&f, state, merged, depth + 1)?;
        }
    }

    state.stack.remove(path);
    Ok(())
}

/// Expand `${VAR}` before parsing; `$${` escapes to a literal `${`, `$$` to `$`.
/// Comments are skipped (no expansion inside `# ...`). Missing variables fail
/// with the file:line of the occurrence (spec §65).
///
/// The scanner tracks basic/literal strings so `#` inside a quoted value is
/// not mistaken for a comment. TOML multiline strings ("""/''') are treated as
/// their single-line forms — expansion inside them may be skipped; configs in
/// practice use `${VAR}` in simple strings.
fn expand_env(text: &str, file: &str) -> Result<String, ConfigError> {
    #[derive(PartialEq)]
    enum State {
        Code,
        BasicString,
        LiteralString,
    }
    let mut out = String::with_capacity(text.len());
    let mut line = 1usize;
    let mut state = State::Code;
    let mut escaped = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match state {
            State::BasicString => {
                if escaped {
                    out.push(c);
                    escaped = false;
                    continue;
                }
                if c == '\\' {
                    out.push(c);
                    escaped = true;
                    continue;
                }
                if c == '"' {
                    out.push(c);
                    state = State::Code;
                    continue;
                }
                if c == '$' {
                    try_expand(&mut chars, &mut out, &mut line, file)?;
                } else {
                    out.push(c);
                }
            }
            State::LiteralString => {
                out.push(c);
                if c == '\'' {
                    state = State::Code;
                }
            }
            State::Code => match c {
                '#' => {
                    // Comment: copy the rest of the line verbatim.
                    out.push(c);
                    for c2 in chars.by_ref() {
                        out.push(c2);
                        if c2 == '\n' {
                            line += 1;
                            break;
                        }
                    }
                }
                '"' => {
                    out.push(c);
                    state = State::BasicString;
                }
                '\'' => {
                    out.push(c);
                    state = State::LiteralString;
                }
                '$' => try_expand(&mut chars, &mut out, &mut line, file)?,
                '\n' => {
                    line += 1;
                    out.push(c);
                }
                _ => out.push(c),
            },
        }
    }
    Ok(out)
}

/// Handle `$` at the current position (caller consumed it): `$${` → literal
/// `${`, `$$` → `$`, `${NAME}` → value of NAME (error if unset).
fn try_expand(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    out: &mut String,
    line: &mut usize,
    file: &str,
) -> Result<(), ConfigError> {
    let literal = chars.peek() == Some(&'$');
    if literal {
        chars.next();
        if chars.peek() == Some(&'{') {
            chars.next();
            out.push_str("${");
        } else {
            out.push('$');
        }
        return Ok(());
    }
    let opening = chars.peek() == Some(&'{');
    if !opening {
        out.push('$');
        return Ok(());
    }
    chars.next(); // consume '{'
    let mut name = String::new();
    let mut closed = false;
    for c2 in chars.by_ref() {
        if c2 == '}' {
            closed = true;
            break;
        }
        name.push(c2);
    }
    if !closed {
        return Err(ConfigError::new(file, *line, "unterminated ${...}"));
    }
    if name.is_empty() {
        return Err(ConfigError::new(file, *line, "empty variable name in ${}"));
    }
    match std::env::var(&name) {
        Ok(v) => out.push_str(&v),
        Err(_) => {
            return Err(ConfigError::new(
                file,
                *line,
                format!("environment variable `{name}` is not set"),
            ))
        }
    }
    Ok(())
}

/// Read and remove the `include` key; must be an array of strings.
fn extract_include(
    doc: &mut toml_edit::DocumentMut,
    file: &str,
    line: usize,
) -> Result<Vec<String>, ConfigError> {
    let Some(item) = doc.remove("include") else {
        return Ok(Vec::new());
    };
    let arr = item
        .as_array()
        .ok_or_else(|| ConfigError::new(file, line, "`include` must be an array of paths"))?;
    let mut out = Vec::new();
    for v in arr.iter() {
        let s = v
            .as_str()
            .ok_or_else(|| ConfigError::new(file, line, "`include` entries must be strings"))?;
        out.push(s.to_string());
    }
    Ok(out)
}

/// Line numbers of the `[[jobs]]` entries plus the bare job (if any), taken
/// from the span-preserving immutable doc — same order as `extract_jobs`.
fn job_line_numbers(im: &toml_edit::ImDocument<String>, text: &str) -> Vec<usize> {
    let mut lines = Vec::new();
    if let Some(arr) = im
        .as_table()
        .get("jobs")
        .and_then(|i| i.as_array_of_tables())
    {
        for t in arr.iter() {
            lines.push(line_of(text, t.span()));
        }
    }
    if im.as_table().contains_key("provider") {
        lines.push(
            im.as_table()
                .get("name")
                .and_then(|i| i.span())
                .map(|s| line_of(text, Some(s)))
                .unwrap_or(1),
        );
    }
    lines
}

/// Extract `[[jobs]]` entries and bare job tables, removing them from the doc.
fn extract_jobs(
    doc: &mut toml_edit::DocumentMut,
    file: &str,
    job_lines: &[usize],
    jobs: &mut Vec<JobEntry>,
) -> Result<(), ConfigError> {
    if let Some(item) = doc.remove("jobs") {
        let line = job_lines.first().copied().unwrap_or(0);
        let arr = item
            .as_array_of_tables()
            .ok_or_else(|| ConfigError::new(file, line, "`jobs` must be an array of tables"))?;
        for (idx, t) in arr.iter().enumerate() {
            let job_line = job_lines.get(idx).copied().unwrap_or(line);
            jobs.push(JobEntry {
                doc: parse_job_table(t, file, job_line)?,
                file: file.to_string(),
                line: job_line,
            });
        }
    }
    // Bare job table: the root itself is a job (spec §78 single-job file).
    // Any unexpected extra section in the same file surfaces via
    // `deny_unknown_fields` when deserializing the JobDoc.
    if doc.as_table().contains_key("provider") {
        let line = job_lines.last().copied().unwrap_or(1);
        // Serialize the whole doc back and deserialize as a job.
        let s = doc.to_string();
        let entry = JobEntry {
            doc: toml::from_str::<JobDoc>(&s).map_err(|e| {
                ConfigError::new(file, line, format!("invalid job: {}", e.message()))
            })?,
            file: file.to_string(),
            line,
        };
        jobs.push(entry);
        doc.as_table_mut().clear();
    }
    Ok(())
}

fn parse_job_table(t: &toml_edit::Table, file: &str, line: usize) -> Result<JobDoc, ConfigError> {
    // `Table::to_string()` does not render sub-tables ([jobs.hooks] would be
    // silently dropped) — round-trip through a DocumentMut instead.
    let mut doc = toml_edit::DocumentMut::new();
    *doc.as_table_mut() = t.clone();
    toml_edit::de::from_document(doc)
        .map_err(|e| ConfigError::new(file, line, format!("invalid job: {e}")))
}

/// Expand an include entry: glob or plain path, relative to `base`.
fn resolve_include(pattern: &str, base: &Path, file: &str) -> Result<Vec<PathBuf>, ConfigError> {
    let full = if Path::new(pattern).is_absolute() {
        PathBuf::from(pattern)
    } else {
        base.join(pattern)
    };
    let has_magic = pattern.contains(['*', '?', '[']);
    if has_magic {
        let glob_pattern = full.to_str().ok_or_else(|| {
            ConfigError::new(
                file,
                0,
                format!("include pattern is not valid UTF-8: {pattern}"),
            )
        })?;
        let mut out = Vec::new();
        for entry in glob::glob(glob_pattern).map_err(|e| {
            ConfigError::new(file, 0, format!("bad include pattern `{pattern}`: {e}"))
        })? {
            let p = entry.map_err(|e| {
                ConfigError::new(file, 0, format!("include pattern `{pattern}`: {e}"))
            })?;
            out.push(p);
        }
        if out.is_empty() {
            return Err(ConfigError::new(
                file,
                0,
                format!("include pattern `{pattern}` matched no files"),
            ));
        }
        out.sort();
        Ok(out)
    } else {
        if !full.exists() {
            return Err(ConfigError::new(
                file,
                0,
                format!("included file not found: {}", full.display()),
            ));
        }
        Ok(vec![full])
    }
}

/// Deep merge: tables merge recursively, everything else (scalars, arrays,
/// array-of-tables) is replaced by the later file.
fn merge_doc(dst: &mut Option<toml_edit::DocumentMut>, src: toml_edit::DocumentMut) {
    match dst {
        None => *dst = Some(src),
        Some(d) => merge_table(d.as_table_mut(), src.as_table()),
    }
}

fn merge_table(dst: &mut toml_edit::Table, src: &toml_edit::Table) {
    for (k, v) in src.iter() {
        let both_tables = dst.get(k).and_then(|i| i.as_table()).is_some() && v.as_table().is_some();
        if both_tables {
            let d = dst.get_mut(k).unwrap().as_table_mut().unwrap();
            merge_table(d, v.as_table().unwrap());
        } else {
            dst.insert(k, v.clone());
        }
    }
}

/// Byte offset → 1-based line number in `text`.
fn line_of(text: &str, span: Option<std::ops::Range<usize>>) -> usize {
    span.map(|s| {
        text[..s.start.min(text.len())]
            .bytes()
            .filter(|&b| b == b'\n')
            .count()
            + 1
    })
    .unwrap_or(0)
}

fn resolve(root: &RootDoc, jobs: Vec<JobEntry>) -> Result<ResolvedConfig, ConfigError> {
    // daemon
    let db = resolve_db(&root.daemon.db)?;
    let daemon = DaemonConfig {
        max_concurrency: root.daemon.max_concurrency.max(1),
        db,
        log_dir: PathBuf::from(&root.daemon.log_dir),
        default_proxy: root.daemon.default_proxy.clone(),
        default_worker: root
            .daemon
            .default_worker
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    };
    // api
    let listen: SocketAddr = root.api.listen.parse().map_err(|_| {
        ConfigError::new(
            "<config>",
            0,
            format!("invalid api.listen `{}`", root.api.listen),
        )
    })?;
    let tls = TlsConfig {
        cert: root.api.tls.cert.as_ref().map(PathBuf::from),
        key: root.api.tls.key.as_ref().map(PathBuf::from),
        client_ca: root.api.tls.client_ca.as_ref().map(PathBuf::from),
    };
    if tls.cert.is_some() != tls.key.is_some() {
        return Err(ConfigError::new(
            "<config>",
            0,
            "api.tls: `cert` and `key` must both be set",
        ));
    }
    let mut tokens = Vec::new();
    for t in &root.api.tokens {
        // A weak or duplicated token makes bearer auth meaningless. Enforce
        // a floor here so `synora check` catches misconfig before boot.
        if t.token.len() < 32 {
            return Err(ConfigError::new(
                "<config>",
                0,
                format!(
                    "api token `{}`: token must be at least 32 bytes (e.g. `openssl rand -hex 32`)",
                    t.name
                ),
            ));
        }
        if t.token.to_ascii_lowercase().contains("change-me") {
            return Err(ConfigError::new(
                "<config>",
                0,
                format!("api token `{}`: placeholder token is not allowed", t.name),
            ));
        }
        if tokens.iter().any(|e: &ApiToken| e.token == t.token) {
            return Err(ConfigError::new(
                "<config>",
                0,
                format!(
                    "api token `{}`: token value must be unique per token",
                    t.name
                ),
            ));
        }
        if tokens.iter().any(|e: &ApiToken| e.name == t.name) {
            return Err(ConfigError::new(
                "<config>",
                0,
                format!(
                    "api token `{}`: token name must be unique (worker identity binds to name)",
                    t.name
                ),
            ));
        }
        if !["admin", "operator", "viewer"].contains(&t.role.as_str()) {
            return Err(ConfigError::new(
                "<config>",
                0,
                format!("api token `{}`: role must be admin|operator|viewer", t.name),
            ));
        }
        tokens.push(ApiToken {
            name: t.name.clone(),
            token: t.token.clone(),
            role: t.role.clone(),
            permissions: t.permissions.clone(),
        });
    }
    let api = ApiConfig {
        listen,
        tls,
        tokens,
        synora_json_path: root.api.synora_json_path.clone(),
        tunasync_json_path: root.api.tunasync_json_path.clone(),
        status_format: root.api.status_format.clone(),
    };

    // jobs: resolve each, reject duplicate names (spec §44)
    let mut seen: HashMap<String, (String, usize)> = HashMap::new();
    let mut resolved = Vec::new();
    for entry in jobs {
        if let Some((prev_file, prev_line)) = seen.get(&entry.doc.name) {
            return Err(ConfigError::new(
                &entry.file,
                entry.line,
                format!(
                    "duplicate job `{}` (first defined at {prev_file}:{prev_line})",
                    entry.doc.name
                ),
            ));
        }
        let mut spec = resolve_job(&entry.doc, &entry.file, entry.line)?;
        if spec
            .worker
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
        {
            spec.worker = daemon.default_worker.clone();
        }
        seen.insert(spec.name.clone(), (entry.file.clone(), entry.line));
        resolved.push(spec);
    }

    let (proxies, proxy_groups) = parse_proxies(root)?;
    let (egresses, egress_groups) = parse_egress(root)?;
    let storages = parse_storage(root)?;
    let cgroup = parse_cgroup(root)?;
    let (snapshot_retention, notifications, groups, min_free_bytes) = parse_misc(root)?;

    Ok(ResolvedConfig {
        version: root.version.unwrap_or(1),
        daemon,
        api,
        jobs: resolved,
        proxies,
        proxy_groups,
        egresses,
        egress_groups,
        storages,
        cgroup,
        snapshot_retention,
        notifications,
        groups,
        min_free_bytes,
        extras: root.extras.clone(),
    })
}

/// Read a string-keyed section from the untyped extras.
fn extra_table<'a>(root: &'a RootDoc, key: &str) -> Option<&'a toml::value::Table> {
    root.extras.get(key).and_then(|v| v.as_table())
}

type ProxyMap = HashMap<String, ProxyConfig>;
type ProxyGroupMap = HashMap<String, ProxyGroupConfig>;
type EgressMap = HashMap<String, EgressGroupConfig>;

/// `zfs_options` accepts either a table (`{ recordsize = "1M", ... }`)
/// or a string of `-o key=value` pairs (e.g. "-o recordsize=1M -o
/// xattr=off"). Both produce the same key/value list for `zfs create`.
fn parse_zfs_options(v: Option<&toml::Value>) -> Option<Vec<(String, String)>> {
    match v {
        Some(toml::Value::Table(t)) => Some(
            t.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect(),
        ),
        Some(toml::Value::String(s)) => {
            let mut out = Vec::new();
            for tok in s.split_whitespace() {
                let rest = tok.strip_prefix("-o").unwrap_or(tok);
                let (k, v) = rest.split_once('=')?;
                out.push((k.to_string(), v.to_string()));
            }
            Some(out)
        }
        _ => None,
    }
}

fn parse_proxies(root: &RootDoc) -> Result<(ProxyMap, ProxyGroupMap), ConfigError> {
    let mut proxies = HashMap::new();
    if let Some(table) = extra_table(root, "proxy") {
        for (name, value) in table {
            let t = value.as_table().ok_or_else(|| {
                ConfigError::new("<config>", 0, format!("[proxy.{name}] must be a table"))
            })?;
            let kind = match t.get("type").and_then(|v| v.as_str()).unwrap_or("http") {
                "http" | "socks5h" => ProxyKind::Forward {
                    url: t
                        .get("url")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    env: parse_env_table(t),
                },
                "command" => ProxyKind::Command {
                    check: t
                        .get("check")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    env: parse_env_table(t),
                },
                "direct" => ProxyKind::Direct,
                other => {
                    return Err(ConfigError::new(
                        "<config>",
                        0,
                        format!("[proxy.{name}]: invalid type `{other}` (http|command|direct)"),
                    ))
                }
            };
            let mut expose = t.get("expose").and_then(|v| v.as_str()).map(String::from);
            if expose.is_none() {
                if let ProxyKind::Forward { url, .. } = &kind {
                    let lower = url.to_ascii_lowercase();
                    if (lower.starts_with("socks5h://") || lower.starts_with("socks5://"))
                        && (lower.contains("127.0.0.1")
                            || lower.contains("localhost")
                            || lower.contains("[::1]")
                            || lower.contains("0.0.0.0"))
                    {
                        // Workers cannot use manager-local SOCKS. Default an
                        // HTTP CONNECT expose the manager will serve.
                        expose = Some("0.0.0.0:14000".into());
                    }
                }
            }
            proxies.insert(
                name.clone(),
                ProxyConfig {
                    kind,
                    healthcheck: t
                        .get("healthcheck")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    expose,
                    expose_auth: t
                        .get("expose_auth")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    timeout: t
                        .get("timeout")
                        .and_then(|v| v.as_str())
                        .and_then(|v| synora_core::parse_duration_human(v).ok())
                        .map(|d| d.whole_seconds().max(1) as u64)
                        .unwrap_or(10),
                },
            );
        }
    }
    let mut groups = HashMap::new();
    if let Some(table) = extra_table(root, "proxy_groups") {
        for (name, value) in table {
            let t = value.as_table().ok_or_else(|| {
                ConfigError::new(
                    "<config>",
                    0,
                    format!("[proxy_groups.{name}] must be a table"),
                )
            })?;
            let proxies: Vec<String> = t
                .get("proxies")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let strategy = t
                .get("strategy")
                .and_then(|v| v.as_str())
                .unwrap_or("failover")
                .to_string();
            if !["fixed", "failover", "round-robin", "random"].contains(&strategy.as_str()) {
                return Err(ConfigError::new(
                    "<config>",
                    0,
                    format!("[proxy_groups.{name}]: invalid strategy `{strategy}`"),
                ));
            }
            groups.insert(name.clone(), ProxyGroupConfig { proxies, strategy });
        }
    }
    Ok((proxies, groups))
}

fn parse_env_table(t: &toml::value::Table) -> Vec<(String, String)> {
    t.get("env")
        .and_then(|v| v.as_table())
        .map(|env| {
            env.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

fn parse_egress(root: &RootDoc) -> Result<(Vec<EgressConfig>, EgressMap), ConfigError> {
    let mut egresses = Vec::new();
    if let Some(arr) = root.extras.get("egress").and_then(|v| v.as_array()) {
        for item in arr {
            let t = item.as_table().ok_or_else(|| {
                ConfigError::new("<config>", 0, "[[egress]] entries must be tables")
            })?;
            let name = t
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let address: std::net::IpAddr = t
                .get("address")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    ConfigError::new(
                        "<config>",
                        0,
                        format!("[[egress]] `{name}`: missing address"),
                    )
                })?
                .parse()
                .map_err(|_| {
                    ConfigError::new(
                        "<config>",
                        0,
                        format!("[[egress]] `{name}`: invalid address"),
                    )
                })?;
            egresses.push(EgressConfig {
                name,
                address,
                probe: t.get("probe").and_then(|v| v.as_str()).map(String::from),
            });
        }
    }
    let mut groups = HashMap::new();
    for key in ["egress-groups", "egress_groups"] {
        if let Some(table) = extra_table(root, key) {
            for (name, value) in table {
                let t = value.as_table().ok_or_else(|| {
                    ConfigError::new("<config>", 0, format!("[{key}.{name}] must be a table"))
                })?;
                let addresses: Vec<String> = t
                    .get("addresses")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let strategy = t
                    .get("strategy")
                    .and_then(|v| v.as_str())
                    .unwrap_or("failover")
                    .to_string();
                groups.insert(
                    name.clone(),
                    EgressGroupConfig {
                        addresses,
                        strategy,
                    },
                );
            }
        }
    }
    Ok((egresses, groups))
}

fn parse_storage(root: &RootDoc) -> Result<HashMap<String, StorageConfig>, ConfigError> {
    let mut storages = HashMap::new();
    if let Some(table) = extra_table(root, "storage") {
        for (name, value) in table {
            let t = value.as_table().ok_or_else(|| {
                ConfigError::new("<config>", 0, format!("[storage.{name}] must be a table"))
            })?;
            let kind = match t.get("type").and_then(|v| v.as_str()).unwrap_or("dir") {
                "dir" => StorageKind::Dir,
                "zfs" => StorageKind::Zfs {
                    pool: t
                        .get("pool")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    dataset: t
                        .get("dataset")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    options: parse_zfs_options(t.get("zfs_options")).unwrap_or_default(),
                },
                "btrfs" => StorageKind::Btrfs {
                    subvol: t
                        .get("subvol")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                },
                other => {
                    return Err(ConfigError::new(
                        "<config>",
                        0,
                        format!("[storage.{name}]: invalid type `{other}` (dir|zfs|btrfs)"),
                    ))
                }
            };
            storages.insert(
                name.clone(),
                StorageConfig {
                    kind,
                    mountpoint: t
                        .get("mountpoint")
                        .and_then(|v| v.as_str())
                        .map(PathBuf::from),
                    auto_create: t
                        .get("auto_create")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    require_empty: t
                        .get("require_empty")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                },
            );
        }
    }
    Ok(storages)
}

fn parse_cgroup(root: &RootDoc) -> Result<Option<CgroupConfig>, ConfigError> {
    match extra_table(root, "cgroup") {
        None => Ok(None),
        Some(t) => Ok(Some(CgroupConfig {
            base_path: t
                .get("base_path")
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/sys/fs/cgroup/synora")),
        })),
    }
}

type MiscSections = (
    synora_core::RetentionPolicy,
    NotificationConfig,
    HashMap<String, Vec<String>>,
    Option<u64>,
);

fn parse_misc(root: &RootDoc) -> Result<MiscSections, ConfigError> {
    let retention = extra_table(root, "snapshot")
        .and_then(|t| t.get("policy").and_then(|v| v.as_table()))
        .map(|t| synora_core::RetentionPolicy {
            keep_last: t
                .get("keep_last")
                .and_then(|v| v.as_integer())
                .map(|v| v as u32),
            keep_daily: t
                .get("keep_daily")
                .and_then(|v| v.as_integer())
                .map(|v| v as u32),
            keep_weekly: t
                .get("keep_weekly")
                .and_then(|v| v.as_integer())
                .map(|v| v as u32),
            keep_monthly: t
                .get("keep_monthly")
                .and_then(|v| v.as_integer())
                .map(|v| v as u32),
        })
        .unwrap_or_default();
    let notifications = extra_table(root, "notifications")
        .and_then(|t| t.get("webhook").and_then(|v| v.as_table()))
        .map(|t| NotificationConfig {
            webhook_url: t.get("url").and_then(|v| v.as_str()).map(String::from),
            alert_after_failures: t
                .get("alert_after_failures")
                .and_then(|v| v.as_integer())
                .unwrap_or(1) as u32,
        })
        .unwrap_or_default();
    let groups = extra_table(root, "groups")
        .map(|t| {
            t.iter()
                .filter_map(|(name, v)| {
                    v.as_table()
                        .and_then(|gt| gt.get("jobs"))
                        .and_then(|j| j.as_array())
                        .map(|a| {
                            (
                                name.clone(),
                                a.iter()
                                    .filter_map(|s| s.as_str().map(String::from))
                                    .collect(),
                            )
                        })
                })
                .collect()
        })
        .unwrap_or_default();
    let min_free = extra_table(root, "min_storage")
        .and_then(|t| t.get("free_bytes"))
        .and_then(|v| v.as_str())
        .and_then(|s| synora_core::parse_duration_human(s).ok())
        .map(|d| d.whole_seconds().max(0) as u64);
    Ok((retention, notifications, groups, min_free))
}

fn resolve_db(db: &DbDoc) -> Result<DbConfig, ConfigError> {
    match db.kind.as_str() {
        "sqlite" => Ok(DbConfig {
            kind: DbKind::Sqlite,
            path: db.path.clone(),
            url: None,
        }),
        "postgres" => {
            let url = db.url.clone().ok_or_else(|| {
                ConfigError::new("<config>", 0, "db.kind = \"postgres\" requires db.url")
            })?;
            Ok(DbConfig {
                kind: DbKind::Postgres,
                path: String::new(),
                url: Some(url),
            })
        }
        other => Err(ConfigError::new(
            "<config>",
            0,
            format!("invalid db.kind `{other}`: expected sqlite|postgres"),
        )),
    }
}

fn resolve_job(doc: &JobDoc, file: &str, line: usize) -> Result<JobSpec, ConfigError> {
    let err = |m: String| ConfigError::new(file, line, m);

    if doc.name.is_empty() || doc.name.contains('/') {
        return Err(err(format!(
            "invalid job name `{}`: must be non-empty, no `/`",
            doc.name
        )));
    }
    let schedule = resolve_schedule(doc, &err)?;
    let provider = resolve_provider(doc, &err)?;

    // Template variables (tunasync-style): `{{.Name}}` / `{{name}}` inside
    // job fields expand to the job name.
    let expand = |v: String| -> String {
        v.replace("{{.Name}}", &doc.name)
            .replace("{{name}}", &doc.name)
    };
    let storage_raw = doc
        .storage
        .as_deref()
        .ok_or_else(|| err("missing required field `storage`".into()))?;
    // tunasync `mirror_subdir`: the mirror lives under <storage>/<sub_dir>.
    let storage = match &doc.mirror_subdir {
        Some(sub) => {
            if sub
                .split('/')
                .any(|c| c.is_empty() || c == ".." || c == ".")
            {
                return Err(err(format!(
                    "invalid mirror_subdir `{sub}` (no .. or empty segments)"
                )));
            }
            format!("{}/{}", storage_raw.trim_end_matches('/'), sub)
        }
        None => storage_raw.to_string(),
    };
    let storage = PathBuf::from(expand(storage));
    if storage
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(err(format!(
            "storage path `{}` must not contain `..`",
            storage.display()
        )));
    }

    let timeout = match &doc.timeout {
        Some(TomlDuration::Seconds(s)) => Duration::seconds(*s as i64),
        Some(TomlDuration::Human(s)) => Duration::seconds(
            schedule::parse_duration_human(s)
                .map_err(|e| err(format!("invalid timeout: {e}")))?
                .whole_seconds(),
        ),
        // No timeout configured = unlimited.
        None => Duration::seconds(i64::MAX / 4),
    };
    let retry_delay = schedule::parse_duration_human(&doc.retry_delay)
        .map_err(|e| err(format!("invalid retry_delay: {e}")))?;
    if doc.retry_backoff < 1.0 {
        return Err(err(format!(
            "retry_backoff must be >= 1.0, got {}",
            doc.retry_backoff
        )));
    }
    if let Some(re) = &doc.fail_on_match {
        regex::Regex::new(re).map_err(|e| err(format!("invalid fail_on_match regex: {e}")))?;
    }
    let misfire_policy = match doc.misfire_policy.as_str() {
        "skip" => MisfirePolicy::Skip,
        "run-immediately" => MisfirePolicy::RunImmediately,
        "run-next" => MisfirePolicy::RunNext,
        other => {
            return Err(err(format!(
                "invalid misfire_policy `{other}`: expected skip|run-immediately|run-next"
            )))
        }
    };
    let on_worker_lost = match doc.on_worker_lost.as_str() {
        "retry" => OnWorkerLost::Retry,
        "fail" => OnWorkerLost::Fail,
        other => {
            return Err(err(format!(
                "invalid on_worker_lost `{other}`: expected retry|fail"
            )))
        }
    };
    let timezone = doc.timezone.clone();
    if time_tz::timezones::get_by_name(&timezone).is_none() {
        return Err(err(format!("unknown timezone `{timezone}`")));
    }
    let statistics = match doc.statistics.as_str() {
        "provider" => StatisticsMode::Provider,
        "filesystem" => StatisticsMode::Filesystem,
        other => {
            return Err(err(format!(
                "invalid statistics `{other}`: expected provider|filesystem"
            )))
        }
    };

    // memory_limit: "4G"/"512M"/"1T" or a bare number in MB (tunasync parity).
    let parse_mem = |m: &str| -> Result<u64, String> {
        let (num, mult) = match m.chars().last() {
            Some('K') => (&m[..m.len() - 1], 1024u64),
            Some('M') => (&m[..m.len() - 1], 1024 * 1024),
            Some('G') => (&m[..m.len() - 1], 1024u64.pow(3)),
            Some('T') => (&m[..m.len() - 1], 1024u64.pow(4)),
            _ => (m, 1024 * 1024),
        };
        num.parse::<u64>()
            .map(|n| n.saturating_mul(mult))
            .map_err(|_| format!("invalid memory_limit `{m}`: expected like `4G` or `512M`"))
    };
    let memory_limit = doc
        .memory_limit
        .as_deref()
        .map(|m| parse_mem(m).map_err(&err))
        .transpose()?;
    let family = match doc.family.as_str() {
        "ipv4" | "ipv6" | "any" => doc.family.clone(),
        other => {
            return Err(err(format!(
                "invalid family `{other}`: expected ipv4|ipv6|any"
            )))
        }
    };
    let snapshot_policy = match doc.snapshot.policy.as_str() {
        "never" => synora_core::SnapshotPolicy::Never,
        "after-success" => synora_core::SnapshotPolicy::AfterSuccess,
        "before-sync" => synora_core::SnapshotPolicy::BeforeSync,
        "before-and-after" => synora_core::SnapshotPolicy::BeforeAndAfter,
        "manual" => synora_core::SnapshotPolicy::Manual,
        other => {
            return Err(err(format!(
                "invalid snapshot policy `{other}`: expected never|after-success|before-sync|before-and-after|manual"
            )))
        }
    };
    for dep in &doc.depends_on {
        if dep == &doc.name {
            return Err(err(format!("job `{}` cannot depend on itself", doc.name)));
        }
    }

    // Template variables (tunasync-style): `{{.Name}}` / `{{name}}` inside
    // job fields expand to the job name (mirrors tunasync's `{{.Name}}`
    // template in worker.conf, e.g. log_dir/storage composition).
    let expand = |v: String| -> String {
        v.replace("{{.Name}}", &doc.name)
            .replace("{{name}}", &doc.name)
    };

    // Job names must be safe path segments (they become log dirs and
    // control-file names).
    if doc
        .name
        .split('/')
        .any(|c| c.is_empty() || c == ".." || c == ".")
        || doc.name.starts_with('.')
    {
        return Err(err(format!(
            "invalid job name `{}` (no `..`, `.`, or leading-dot segments)",
            doc.name
        )));
    }
    Ok(JobSpec {
        name: doc.name.clone(),
        enabled: doc.enabled,
        worker: doc.worker.clone(),
        provider,
        upstream: doc.upstream.clone().map(expand),
        storage,
        mirror_subdir: doc.mirror_subdir.clone(),
        storage_name: doc.storage_name.clone(),
        proxy: doc.proxy.clone(),
        egress: doc.egress.clone(),
        timeout,
        retry: doc.retry,
        retry_delay,
        retry_backoff: doc.retry_backoff,
        success_exit_codes: doc.success_exit_codes.clone(),
        fail_on_match: doc.fail_on_match.clone(),
        max_concurrency: doc.max_concurrency.max(1),
        misfire_policy,
        on_worker_lost,
        timezone,
        statistics,
        resources: doc.resources.clone(),
        priority: doc.priority,
        schedule,
        hooks: Hooks {
            before_sync: doc.hooks.before_sync.clone(),
            after_sync: doc.hooks.after_sync.clone(),
            on_success: doc.hooks.on_success.clone(),
            on_failure: doc.hooks.on_failure.clone(),
        },
        safety: Safety {
            max_delete_files: doc.safety.max_delete_files,
            max_delete_ratio: doc.safety.max_delete_ratio,
            max_size_drop_ratio: doc.safety.max_size_drop_ratio,
        },
        family,
        memory_limit,
        cpu_limit: doc.cpu_limit,
        depends_on: doc.depends_on.clone(),
        snapshot_policy,
        verify: synora_core::VerifyConfig {
            enabled: doc.verify.enabled,
            checks: doc.verify.checks.clone(),
            command: doc.verify.command.clone(),
        },
    })
}

fn resolve_schedule(
    doc: &JobDoc,
    err: &impl Fn(String) -> ConfigError,
) -> Result<Schedule, ConfigError> {
    let kind_str = doc
        .schedule
        .as_deref()
        .ok_or_else(|| err("missing required field `schedule`".into()))?;
    let kind = match kind_str {
        "cron" => {
            let expr = doc
                .cron
                .as_deref()
                .ok_or_else(|| err("schedule = \"cron\" requires `cron`".into()))?;
            let expr = schedule::parse_cron_expr(expr).map_err(err)?;
            ScheduleKind::Cron { expr }
        }
        "daily" => {
            let at = doc
                .at
                .as_deref()
                .ok_or_else(|| err("schedule = \"daily\" requires `at`".into()))?;
            ScheduleKind::Daily {
                at: schedule::parse_time_at(at).map_err(err)?,
            }
        }
        "weekly" => {
            let weekday = doc
                .weekday
                .as_deref()
                .ok_or_else(|| err("schedule = \"weekly\" requires `weekday`".into()))?;
            let at = doc
                .at
                .as_deref()
                .ok_or_else(|| err("schedule = \"weekly\" requires `at`".into()))?;
            ScheduleKind::Weekly {
                weekday: schedule::parse_weekday(weekday).map_err(err)?,
                at: schedule::parse_time_at(at).map_err(err)?,
            }
        }
        "interval" => {
            let every = doc
                .every
                .as_deref()
                .ok_or_else(|| err("schedule = \"interval\" requires `every`".into()))?;
            ScheduleKind::Interval {
                every: schedule::parse_duration_human(every).map_err(err)?,
            }
        }
        "manual" | "startup" => {
            if kind_str == "manual" {
                ScheduleKind::Manual
            } else {
                ScheduleKind::Startup
            }
        }
        other => {
            return Err(err(format!(
                "invalid schedule `{other}`: expected cron|daily|weekly|interval|manual|startup"
            )))
        }
    };
    // Schedule-kind-specific fields are mutually exclusive.
    for (field, present) in [
        ("cron", doc.cron.is_some()),
        ("at", doc.at.is_some()),
        ("weekday", doc.weekday.is_some()),
        ("every", doc.every.is_some()),
    ] {
        let expected = match kind_str {
            "cron" => field == "cron",
            "daily" => field == "at",
            "weekly" => field == "weekday" || field == "at",
            "interval" => field == "every",
            _ => false,
        };
        if present && !expected {
            return Err(err(format!(
                "field `{field}` is not valid for schedule = \"{kind_str}\""
            )));
        }
    }
    Ok(Schedule { kind })
}

fn resolve_provider(
    doc: &JobDoc,
    err: &impl Fn(String) -> ConfigError,
) -> Result<ProviderConfig, ConfigError> {
    let provider = doc
        .provider
        .as_deref()
        .ok_or_else(|| err("missing required field `provider`".into()))?;
    match provider {
        "rsync" => {
            if doc.upstream.is_none() {
                return Err(err("provider = \"rsync\" requires `upstream`".into()));
            }
            Ok(ProviderConfig::Rsync {
                options: doc.options.clone(),
                exclude: doc.exclude.clone(),
            })
        }
        "two-stage-rsync" => {
            if doc.upstream.is_none() {
                return Err(err(
                    "provider = \"two-stage-rsync\" requires `upstream`".into()
                ));
            }
            let stage1_profile = doc
                .stage1_profile
                .clone()
                .unwrap_or_else(|| "debian".to_string());
            if !["debian", "debian-oldstyle"].contains(&stage1_profile.as_str()) {
                return Err(err(format!(
                    "invalid stage1_profile `{stage1_profile}`: expected debian|debian-oldstyle"
                )));
            }
            Ok(ProviderConfig::TwoStageRsync {
                options: doc.options.clone(),
                exclude: doc.exclude.clone(),
                stage1_profile,
            })
        }
        "git" => {
            if doc.upstream.is_none() {
                return Err(err(
                    "provider = \"git\" requires `upstream` (repository URL)".into(),
                ));
            }
            Ok(ProviderConfig::Git {
                branch: doc.branch.clone(),
            })
        }
        "script" => {
            let command = doc
                .command
                .as_ref()
                .ok_or_else(|| err("provider = \"script\" requires `command`".into()))?;
            Ok(ProviderConfig::Script {
                command: command.clone(),
            })
        }
        "docker" => {
            let image = doc
                .image
                .as_ref()
                .ok_or_else(|| err("provider = \"docker\" requires `image`".into()))?;
            Ok(ProviderConfig::Docker {
                image: image.clone(),
                env: doc.env.clone(),
                volumes: doc.volumes.clone(),
                keep_container: doc.keep_container,
                network: doc.docker_network.clone(),
                command: doc.docker_command.clone(),
            })
        }
        "http" => {
            let parser = doc
                .parser
                .as_deref()
                .ok_or_else(|| err("provider = \"http\" requires `parser`".into()))?;
            if parser::parser_for(parser).is_none() {
                return Err(err(format!("unknown parser `{parser}`")));
            }
            Ok(ProviderConfig::Http {
                parser: parser.to_string(),
                delete: doc.delete,
                threads: doc.threads,
            })
        }
        other => Err(err(format!(
            "invalid provider `{other}`: expected rsync|script|docker|git|http"
        ))),
    }
}

/// Fixed whitelist of env overrides (spec §43).
fn apply_env_overrides(cfg: &mut ResolvedConfig) -> Result<(), ConfigError> {
    if let Ok(v) = std::env::var("SYNORA_MAX_CONCURRENCY") {
        cfg.daemon.max_concurrency = v.parse::<u32>().map_err(|_| {
            ConfigError::new(
                "<environment>",
                0,
                format!("SYNORA_MAX_CONCURRENCY `{v}` is not a number"),
            )
        })?;
    }
    if let Ok(v) = std::env::var("SYNORA_API_LISTEN") {
        cfg.api.listen = v.parse().map_err(|_| {
            ConfigError::new(
                "<environment>",
                0,
                format!("SYNORA_API_LISTEN `{v}` is not a valid address"),
            )
        })?;
    }
    if let Ok(v) = std::env::var("SYNORA_DB_URL") {
        cfg.daemon.db.kind = DbKind::Postgres;
        cfg.daemon.db.url = Some(v);
    }
    if let Ok(v) = std::env::var("SYNORA_TOKEN") {
        cfg.api.tokens.push(ApiToken {
            name: "env".into(),
            token: v,
            role: "admin".into(),
            permissions: Vec::new(),
        });
    }
    Ok(())
}

fn apply_cli_overrides(
    cfg: &mut ResolvedConfig,
    overrides: &CliOverrides,
) -> Result<(), ConfigError> {
    if let Some(kind) = &overrides.db_kind {
        cfg.daemon.db.kind = match kind.as_str() {
            "sqlite" => DbKind::Sqlite,
            "postgres" => DbKind::Postgres,
            other => {
                return Err(ConfigError::new(
                    "<cli>",
                    0,
                    format!("invalid --db-kind `{other}`: expected sqlite|postgres"),
                ))
            }
        };
    }
    if let Some(path) = &overrides.db_path {
        cfg.daemon.db.path = path.clone();
    }
    if let Some(url) = &overrides.db_url {
        cfg.daemon.db.kind = DbKind::Postgres;
        cfg.daemon.db.url = Some(url.clone());
    }
    if let Some(listen) = &overrides.api_listen {
        cfg.api.listen = SocketAddr::from_str(listen).map_err(|_| {
            ConfigError::new("<cli>", 0, format!("invalid --api-listen `{listen}`"))
        })?;
    }
    Ok(())
}
