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
use synora_core::job::{
    Hooks, JobSpec, MisfirePolicy, OnWorkerLost, ProviderConfig, Safety, StatisticsMode,
};
use synora_core::schedule::{self, Schedule, ScheduleKind};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use time::Duration;

const MAX_INCLUDE_DEPTH: usize = 32;

#[derive(Debug)]
pub struct ResolvedConfig {
    pub version: u64,
    pub daemon: DaemonConfig,
    pub api: ApiConfig,
    pub jobs: Vec<JobSpec>,
    /// Parsed-but-inert sections (proxy/proxy_groups/egress/storage/worker).
    pub extras: HashMap<String, toml::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonConfig {
    pub max_concurrency: u32,
    pub db: DbConfig,
    pub log_dir: PathBuf,
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
    pub fn load(config_path: &Path, overrides: &CliOverrides) -> Result<ResolvedConfig, ConfigError> {
        let root = config_path
            .canonicalize()
            .map_err(|e| ConfigError::new(config_path.display().to_string(), 0, format!("cannot read config: {e}")))?;
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
        return Err(ConfigError::new(file, 0, "include nesting too deep (cycle?)"));
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
        ConfigError::new(&file, line_of(&text, e.span()), format!("TOML syntax error: {e}"))
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
        ConfigError::new(&file, line_of(&text, e.span()), format!("TOML syntax error: {e}"))
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
    if let Some(arr) = im.as_table().get("jobs").and_then(|i| i.as_array_of_tables()) {
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

fn parse_job_table(
    t: &toml_edit::Table,
    file: &str,
    line: usize,
) -> Result<JobDoc, ConfigError> {
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
            ConfigError::new(file, 0, format!("include pattern is not valid UTF-8: {pattern}"))
        })?;
        let mut out = Vec::new();
        for entry in glob::glob(glob_pattern)
            .map_err(|e| ConfigError::new(file, 0, format!("bad include pattern `{pattern}`: {e}")))?
        {
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
    span.map(|s| text[..s.start.min(text.len())].bytes().filter(|&b| b == b'\n').count() + 1)
        .unwrap_or(0)
}

fn resolve(root: &RootDoc, jobs: Vec<JobEntry>) -> Result<ResolvedConfig, ConfigError> {
    // daemon
    let db = resolve_db(&root.daemon.db)?;
    let daemon = DaemonConfig {
        max_concurrency: root.daemon.max_concurrency.max(1),
        db,
        log_dir: PathBuf::from(&root.daemon.log_dir),
    };
    // api
    let listen: SocketAddr = root
        .api
        .listen
        .parse()
        .map_err(|_| ConfigError::new("<config>", 0, format!("invalid api.listen `{}`", root.api.listen)))?;
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
    let api = ApiConfig { listen, tls, tokens };

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
        let spec = resolve_job(&entry.doc, &entry.file, entry.line)?;
        seen.insert(spec.name.clone(), (entry.file.clone(), entry.line));
        resolved.push(spec);
    }

    Ok(ResolvedConfig {
        version: root.version.unwrap_or(1),
        daemon,
        api,
        jobs: resolved,
        extras: root.extras.clone(),
    })
}

fn resolve_db(db: &DbDoc) -> Result<DbConfig, ConfigError> {
    match db.kind.as_str() {
        "sqlite" => Ok(DbConfig {
            kind: DbKind::Sqlite,
            path: db.path.clone(),
            url: None,
        }),
        "postgres" => {
            let url = db
                .url
                .clone()
                .ok_or_else(|| ConfigError::new("<config>", 0, "db.kind = \"postgres\" requires db.url"))?;
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
        return Err(err(format!("invalid job name `{}`: must be non-empty, no `/`", doc.name)));
    }
    let schedule = resolve_schedule(doc, &err)?;
    let provider = resolve_provider(doc, &err)?;

    let storage = doc
        .storage
        .as_deref()
        .ok_or_else(|| err("missing required field `storage`".into()))?;
    let storage = PathBuf::from(storage);
    if storage.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err(err(format!(
            "storage path `{}` must not contain `..`",
            storage.display()
        )));
    }

    let timeout = match &doc.timeout {
        TomlDuration::Seconds(s) => Duration::seconds(*s as i64),
        TomlDuration::Human(s) => Duration::seconds(
            schedule::parse_duration_human(s)
                .map_err(|e| err(format!("invalid timeout: {e}")))?
                .whole_seconds(),
        ),
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

    Ok(JobSpec {
        name: doc.name.clone(),
        enabled: doc.enabled,
        worker: doc.worker.clone(),
        provider,
        upstream: doc.upstream.clone(),
        storage,
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
            return Err(err(format!("field `{field}` is not valid for schedule = \"{kind_str}\"")));
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
            })
        }
        other => Err(err(format!(
            "invalid provider `{other}`: expected rsync|script|docker"
        ))),
    }
}

/// Fixed whitelist of env overrides (spec §43).
fn apply_env_overrides(cfg: &mut ResolvedConfig) -> Result<(), ConfigError> {
    if let Ok(v) = std::env::var("SYNORA_MAX_CONCURRENCY") {
        cfg.daemon.max_concurrency = v.parse::<u32>().map_err(|_| {
            ConfigError::new("<environment>", 0, format!("SYNORA_MAX_CONCURRENCY `{v}` is not a number"))
        })?;
    }
    if let Ok(v) = std::env::var("SYNORA_API_LISTEN") {
        cfg.api.listen = v.parse().map_err(|_| {
            ConfigError::new("<environment>", 0, format!("SYNORA_API_LISTEN `{v}` is not a valid address"))
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
        cfg.api.listen = SocketAddr::from_str(listen)
            .map_err(|_| ConfigError::new("<cli>", 0, format!("invalid --api-listen `{listen}`")))?;
    }
    Ok(())
}
