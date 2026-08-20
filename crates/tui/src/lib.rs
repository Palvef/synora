//! `synora-tui` — terminal console (spec §39–§41): jobs table with status /
//! size / next run, per-job detail (run history), worker panel, proxy panel
//! with in-TUI proxy registration (CF One / WARP auto-detect, manual http/
//! socks5h add), and a log viewer that follows the selected job.
//! Data comes from the manager API; proxy registration appends to the local
//! config file and hot-reloads the manager.

use api::Client;
use clap::Parser;
use config::{CliOverrides, ConfigLoader};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Row, Table, TableState};
use ratatui::Frame;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

#[derive(Parser)]
#[command(name = "synora-tui", version, about = "Synora terminal console")]
struct Cli {
    /// Main config file (also the file proxy registration writes to)
    #[arg(short, long)]
    config: Option<PathBuf>,
    /// Manager URL override (default: config api.listen)
    #[arg(long)]
    manager: Option<String>,
    /// API token override (default: first configured token)
    #[arg(long)]
    token: Option<String>,
}

/// Snapshot of the world for one render pass.
#[derive(Default, Clone)]
struct Snapshot {
    jobs: Vec<api::JobDTO>,
    workers: Vec<api::WorkerDTO>,
    log_lines: Vec<String>,
    log_job: Option<String>,
    /// Job whose spec is currently loaded (spec re-fetch gate).
    spec_job: Option<String>,
    history: Vec<api::RunDTO>,
    /// Full job spec (structured editor) + its editable field view.
    spec_json: Option<serde_json::Value>,
    spec_fields: Vec<(String, String)>,
    proxies: Vec<serde_json::Value>,
    error: Option<String>,
}

#[derive(PartialEq)]
enum Mode {
    Jobs,
    Workers,
    Proxies,
    Logs,
    JobDetail,
    /// Structured job editor: all fields of the selected job.
    SpecEdit,
    Config,
}

/// Step-by-step input for adding a proxy (name → url → optional expose),
/// creating a job (name → upstream → storage → schedule), or searching.
enum Input {
    None,
    Search,
    ProxyName,
    ProxyUrl {
        name: String,
    },
    ProxyExpose {
        name: String,
        url: String,
    },
    JobName,
    /// Pick which worker a job runs on (TUI: job detail → w).
    WorkerSelect {
        job: String,
        candidates: Vec<String>,
    },
    /// Edit one field of a job's spec (structured editor).
    SpecValue {
        field: String,
    },
    JobUpstream {
        name: String,
        provider: String,
    },
    JobStorage {
        name: String,
        provider: String,
        upstream: String,
    },
    /// Arrow-key pick list used for add/edit (provider, schedule, enums).
    Pick {
        title: String,
        options: Vec<(String, String)>,
        idx: usize,
        kind: PickKind,
    },
    /// Edit a string-array field (rsync `options` / `exclude`) as a list.
    ListEdit {
        field: String,
        items: Vec<String>,
        idx: usize,
    },
}

enum PickKind {
    NewProvider {
        name: String,
    },
    NewSchedule {
        name: String,
        provider: String,
        upstream: String,
        storage: String,
    },
    SpecField {
        field: String,
    },
    /// Append one extra rsync option (or a custom value).
    ListAppend {
        field: String,
    },
}

struct App {
    mode: Mode,
    selected: usize,
    /// Field cursor inside SpecEdit (must NOT reuse `selected`, which is the job index).
    spec_field: usize,
    /// Job frozen when entering SpecEdit so arrows cannot switch jobs.
    edit_job: Option<String>,
    jobs_state: TableState,
    workers_state: TableState,
    input: Input,
    input_buf: String,
    /// Config file proxy registration writes to.
    cfg_path: Option<PathBuf>,
    /// One-shot notice line (operation results).
    notice: Option<String>,
    /// Search filter for the jobs table (entered with `/`).
    search: String,
    /// Config editor state (F6): file list + editable lines of the current
    /// file + cursor.
    config_files: Vec<PathBuf>,
    config_idx: usize,
    file_lines: Vec<String>,
    cur_row: usize,
    cur_col: usize,
    dirty: bool,
}

impl App {
    fn new(cfg_path: Option<PathBuf>) -> App {
        let mut jobs_state = TableState::default();
        jobs_state.select(Some(0));
        App {
            mode: Mode::Jobs,
            selected: 0,
            spec_field: 0,
            edit_job: None,
            jobs_state,
            workers_state: TableState::default(),
            input: Input::None,
            input_buf: String::new(),
            cfg_path,
            notice: None,
            search: String::new(),
            config_files: Vec::new(),
            config_idx: 0,
            file_lines: Vec::new(),
            cur_row: 0,
            cur_col: 0,
            dirty: false,
        }
    }

    fn selected_job<'a>(&self, snap: &'a Snapshot) -> Option<&'a api::JobDTO> {
        snap.jobs.get(self.selected)
    }

    /// Jobs matching the search filter (all when the filter is empty).
    fn visible_jobs<'a>(&self, snap: &'a Snapshot) -> Vec<&'a api::JobDTO> {
        let needle = self.search.to_lowercase();
        snap.jobs
            .iter()
            .filter(|j| needle.is_empty() || j.name.to_lowercase().contains(&needle))
            .collect()
    }
}

async fn fetch(client: &Client, snap: &mut Snapshot) {
    snap.error = None;
    match (client.list_jobs().await, client.list_workers().await) {
        (Ok(jobs), Ok(workers)) => {
            snap.jobs = jobs;
            snap.workers = workers;
        }
        (Err(e), _) | (_, Err(e)) => snap.error = Some(e.to_string()),
    }
    if let Ok(p) = client.list_proxies().await {
        snap.proxies = p
            .get("proxies")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
    }
}

fn status_label(status: &str) -> String {
    match status.to_ascii_lowercase().as_str() {
        "starting" | "syncing" | "running" => "syncing".into(),
        other => other.to_string(),
    }
}

fn status_color(status: &str) -> Color {
    match status.to_ascii_lowercase().as_str() {
        "success" => Color::Green,
        "failed" | "lost" => Color::Red,
        "running" | "starting" | "syncing" | "retrying" => Color::Yellow,
        "queued" => Color::Blue,
        "cancelled" | "cancelling" => Color::Magenta,
        _ => Color::Gray,
    }
}

fn fmt_ts(ts: Option<i64>) -> String {
    match ts {
        Some(t) if t > 0 => time::OffsetDateTime::from_unix_timestamp(t)
            .map(|t| {
                t.format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_else(|_| "-".into())
            })
            .unwrap_or_else(|_| "-".into()),
        _ => "-".into(),
    }
}

fn fmt_size(bytes: Option<i64>) -> String {
    match bytes {
        Some(b) if b > 0 => synora_core::human_size(b as u64),
        _ => "-".into(),
    }
}

fn render_jobs(f: &mut Frame, app: &mut App, snap: &Snapshot) {
    let header = Row::new(vec!["JOB", "STATUS", "SIZE", "NEXT RUN", "WORKER"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let visible = app.visible_jobs(snap);
    let rows: Vec<Row> = visible
        .iter()
        .map(|j| {
            // The worker column shows who ACTUALLY ran it last (a
            // configured worker id / group, else the last run's worker).
            let actual_worker = j
                .last_run
                .as_ref()
                .and_then(|r| r.worker_id.clone())
                .unwrap_or_else(|| j.worker.clone().unwrap_or_else(|| "auto".to_string()));
            Row::new(vec![
                j.name.clone(),
                status_label(&j.status),
                fmt_size(j.size_bytes),
                fmt_ts(j.next_run),
                actual_worker,
            ])
            .style(Style::default().fg(status_color(&j.status)))
        })
        .collect();
    let title = if app.search.is_empty() {
        " Jobs (F1) ".to_string()
    } else {
        format!(
            " Jobs (F1) — filter: \"{}\" ({}/{}) ",
            app.search,
            visible.len(),
            snap.jobs.len()
        )
    };
    let table = Table::new(
        rows,
        [
            Constraint::Percentage(25),
            Constraint::Percentage(15),
            Constraint::Percentage(15),
            Constraint::Percentage(25),
            Constraint::Percentage(20),
        ],
    )
    .header(header)
    .block(Block::default().borders(Borders::ALL).title(title))
    .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
    .highlight_symbol("> ");
    f.render_stateful_widget(table, f.area(), &mut app.jobs_state);
}

/// Detail panel: selected job's last runs (Enter from the jobs table).
fn render_job_detail(f: &mut Frame, app: &App, snap: &Snapshot) {
    let Some(job) = app.selected_job(snap) else {
        f.render_widget(
            Paragraph::new("no job selected").block(Block::default().borders(Borders::ALL)),
            f.area(),
        );
        return;
    };
    let lines = vec![
        Line::from(Span::styled(
            format!(" {} — {} ", job.name, status_label(&job.status)),
            Style::default()
                .fg(status_color(&job.status))
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::raw(format!(
            "  upstream:  {}",
            job.upstream.clone().unwrap_or_else(|| "-".into())
        ))),
        Line::from(Span::raw(format!(
            "  storage:   {}   (size {})",
            job.storage_path,
            fmt_size(job.size_bytes)
        ))),
        Line::from(Span::raw(format!(
            "  next run:  {}   provider: {}   worker: {}",
            fmt_ts(job.next_run),
            job.provider,
            job.worker.clone().unwrap_or_else(|| "auto".into())
        ))),
        Line::from(Span::raw("")),
    ];
    let header = Row::new(vec![
        "RUN ID", "STATUS", "DURATION", "EXIT", "BYTES", "MESSAGE",
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = snap
        .history
        .iter()
        .take(30)
        .map(|r| {
            let msg = r
                .message
                .clone()
                .unwrap_or_default()
                .chars()
                .take(60)
                .collect::<String>();
            Row::new(vec![
                r.id.chars().take(8).collect(),
                status_label(&r.status),
                r.duration_secs
                    .map(|d| format!("{d}s"))
                    .unwrap_or_else(|| "-".into()),
                r.exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "-".into()),
                fmt_size(r.bytes_transferred),
                msg,
            ])
            .style(Style::default().fg(status_color(&r.status)))
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Percentage(12),
            Constraint::Percentage(12),
            Constraint::Percentage(10),
            Constraint::Percentage(8),
            Constraint::Percentage(12),
            Constraint::Percentage(46),
        ],
    )
    .header(header)
    .block(Block::default().borders(Borders::ALL).title(" Last runs "));
    let chunks = Layout::vertical([
        Constraint::Length(lines.len() as u16 + 1),
        Constraint::Min(6),
    ])
    .split(f.area());
    f.render_widget(Paragraph::new(lines), chunks[0]);
    f.render_widget(table, chunks[1]);
}

fn render_workers(f: &mut Frame, app: &mut App, snap: &Snapshot) {
    let header = Row::new(vec!["ID", "HOST", "STATUS", "RUNNING", "LABELS"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = snap
        .workers
        .iter()
        .map(|w| {
            Row::new(vec![
                w.id.clone(),
                w.hostname.clone(),
                w.status.clone(),
                w.jobs_running.to_string(),
                w.labels.join(","),
            ])
            .style(Style::default().fg(status_color(&w.status)))
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Percentage(20),
            Constraint::Percentage(20),
            Constraint::Percentage(15),
            Constraint::Percentage(10),
            Constraint::Percentage(35),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Workers (F2) "),
    )
    .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
    .highlight_symbol("> ");
    f.render_stateful_widget(table, f.area(), &mut app.workers_state);
}

fn render_proxies(f: &mut Frame, _app: &App, snap: &Snapshot) {
    let header = Row::new(vec![
        "NAME",
        "TYPE",
        "LATENCY",
        "EGRESS IP",
        "HEALTH",
        "EXPOSE",
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = snap
        .proxies
        .iter()
        .map(|p| {
            let get = |k: &str| p.get(k).and_then(|v| v.as_str()).unwrap_or("-").to_string();
            let latency = p
                .get("latency_ms")
                .and_then(|v| v.as_u64())
                .map(|v| format!("{v}ms"))
                .unwrap_or_else(|| "-".into());
            let healthy = p.get("healthy").and_then(|v| v.as_bool()).unwrap_or(false);
            Row::new(vec![
                get("name"),
                get("type"),
                latency,
                get("egress_ip"),
                if healthy { "UP".into() } else { "DOWN".into() },
                get("expose"),
            ])
            .style(Style::default().fg(if healthy {
                Color::Green
            } else {
                Color::Red
            }))
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Percentage(16),
            Constraint::Percentage(10),
            Constraint::Percentage(12),
            Constraint::Percentage(18),
            Constraint::Percentage(10),
            Constraint::Percentage(34),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Proxies (F3) — a add, w register CF One/WARP "),
    );
    f.render_widget(table, f.area());
}

fn render_logs(f: &mut Frame, app: &App, snap: &Snapshot) {
    let job = app.selected_job(snap);
    let title = job
        .map(|j| format!(" Logs: {} (F5) — follows selected job ", j.name))
        .unwrap_or_else(|| " Logs (F5) ".to_string());
    let items: Vec<ListItem> = snap
        .log_lines
        .iter()
        .rev()
        .map(|l| ListItem::new(Line::from(Span::raw(l.clone()))))
        .collect();
    let list = List::new(items).block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(list, f.area());
}

/// Structured job editor: every field with its current value (unset
/// fields show their default), arrow keys select, Enter edits the value,
/// S writes the whole job back and reloads.
fn render_spec_edit(f: &mut Frame, app: &App, snap: &Snapshot) {
    let job = app
        .edit_job
        .clone()
        .or_else(|| snap.jobs.get(app.selected).map(|j| j.name.clone()))
        .unwrap_or_default();
    let items: Vec<ListItem> = snap
        .spec_fields
        .iter()
        .enumerate()
        .map(|(i, (k, v))| {
            let mark = if i == app.spec_field { "> " } else { "  " };
            ListItem::new(format!("{mark}{k:<22} {v}"))
        })
        .collect();
    let title = format!(" Edit job: {job} — Enter change value, S save+reload, Esc back ");
    let list = List::new(items).block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(list, f.area());
}

/// Config editor: file list on the left, editable text on the right.
fn render_config(f: &mut Frame, app: &App) {
    let file_items: Vec<ListItem> = app
        .config_files
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let mark = if i == app.config_idx { "> " } else { "  " };
            ListItem::new(format!("{mark}{name}"))
        })
        .collect();
    let files =
        List::new(file_items).block(Block::default().borders(Borders::ALL).title(" Files "));
    let lines: Vec<Line> = app
        .file_lines
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let prefix = if i == app.cur_row { "▌" } else { " " };
            Line::from(Span::raw(format!("{prefix} {l}")))
        })
        .collect();
    let body = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Editor (F6) — arrows move, type to edit, S save, Tab switch file, Esc back "),
    );
    let chunks = Layout::horizontal([Constraint::Percentage(22), Constraint::Percentage(78)])
        .split(f.area());
    f.render_widget(files, chunks[0]);
    f.render_widget(body, chunks[1]);
}

/// The input overlay (add-proxy form).
fn render_input(f: &mut Frame, app: &App) {
    let (title, label, done) = match &app.input {
        Input::None => return,
        Input::Search => (" Search ", "keyword (empty = clear filter): ", ""),
        Input::ProxyName => (" Add proxy ", "proxy name (e.g. cf-warp): ", ""),
        Input::ProxyUrl { .. } => (" Add proxy ", "proxy url (http://… or socks5h://…): ", ""),
        Input::ProxyExpose { .. } => (
            " Add proxy ",
            "expose address (optional, e.g. 0.0.0.0:4000): ",
            "Enter = done",
        ),
        Input::JobName => (" New job ", "job name: ", ""),
        Input::WorkerSelect { .. } => (
            " Assign worker ",
            "number of the worker above (Enter confirms): ",
            "",
        ),
        Input::SpecValue { field, .. } => (" Edit field ", field.as_str(), ""),
        Input::JobUpstream { .. } => (
            " New job ",
            "upstream (rsync://… / http://… / git url / path): ",
            "",
        ),
        Input::JobStorage { .. } => (" New job ", "storage path: ", ""),
        Input::Pick { title, .. } => (title.as_str(), "↑/↓ select, Enter confirm: ", ""),
        Input::ListEdit { field, .. } => (
            field.as_str(),
            "↑/↓ item  a add  d delete  Enter add  Esc done: ",
            "",
        ),
    };
    let mut body = format!(
        "{label}{}\n{done}   Esc cancel, Enter confirm",
        app.input_buf
    );
    if let Input::WorkerSelect { candidates, .. } = &app.input {
        for (i, w) in candidates.iter().enumerate() {
            let mark = if i.to_string() == app.input_buf.trim() {
                "> "
            } else {
                "  "
            };
            body.push_str(&format!("\n{mark}{i}: {w}"));
        }
    }
    if let Input::Pick { options, idx, .. } = &app.input {
        body.clear();
        body.push_str("↑/↓ select, Enter confirm   Esc cancel\n");
        for (i, (label, _)) in options.iter().enumerate() {
            let mark = if i == *idx { "> " } else { "  " };
            body.push_str(&format!("{mark}{label}\n"));
        }
    }
    if let Input::ListEdit { items, idx, field } = &app.input {
        body.clear();
        body.push_str(&format!(
            "{field}: extra flags (built-in rsync defaults stay). a/Enter add  d delete  Esc save\n"
        ));
        if items.is_empty() {
            body.push_str("  (none — press a to add)\n");
        }
        for (i, item) in items.iter().enumerate() {
            let mark = if i == *idx { "> " } else { "  " };
            body.push_str(&format!("{mark}{item}\n"));
        }
    }
    let height = 3 + body.lines().count() as u16;
    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(height)]).split(f.area());
    f.render_widget(
        Paragraph::new(body).block(Block::default().borders(Borders::ALL).title(title)),
        chunks[1],
    );
}

fn render_notice(f: &mut Frame, app: &App) {
    if let Some(n) = &app.notice {
        let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(f.area());
        f.render_widget(
            Paragraph::new(n.clone()).style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            chunks[0],
        );
    }
}

fn render_footer(f: &mut Frame, app: &App, snap: &Snapshot) {
    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(f.area());
    // Mode-specific key hints (user: keys only show in the panel they apply to).
    let mode_hint = match app.mode {
        Mode::Jobs => "Enter detail  r run  s stop  D delete  / search  n new-job",
        Mode::JobDetail => "Esc back  r run  s stop  w assign worker  e edit config  D delete",
        Mode::SpecEdit => "Enter edit value  S save+reload  Esc back",
        Mode::Workers => "↑/↓ select  d remove worker",
        Mode::Proxies => "a add-proxy  w register CF-One/WARP  e edit config",
        Mode::Config => "S save  Tab switch file  Esc back",
        Mode::Logs => "↑/↓ select job  (logs follow selection)",
    };
    let hint = format!("F1 Jobs  F2 Workers  F3 Proxies  F5 Logs  q/F10 quit   |   {mode_hint}");
    f.render_widget(
        Paragraph::new(hint).style(Style::default().fg(Color::DarkGray)),
        chunks[1],
    );
    if let Some(err) = &snap.error {
        f.render_widget(
            Paragraph::new(Line::from(Span::raw(format!("error: {err}"))))
                .style(Style::default().fg(Color::Red)),
            chunks[0],
        );
    }
}

fn render(f: &mut Frame, app: &mut App, snap: &Snapshot) {
    match app.mode {
        Mode::Jobs => render_jobs(f, app, snap),
        Mode::Workers => render_workers(f, app, snap),
        Mode::Proxies => render_proxies(f, app, snap),
        Mode::Logs => render_logs(f, app, snap),
        Mode::JobDetail => render_job_detail(f, app, snap),
        Mode::SpecEdit => render_spec_edit(f, app, snap),
        Mode::Config => render_config(f, app),
    }
    render_input(f, app);
    render_notice(f, app);
    render_footer(f, app, snap);
}

fn detect_local_socks() -> Vec<u16> {
    match netroute::local_warp_url() {
        Some(url) => url
            .rsplit_once(':')
            .and_then(|(_, p)| p.parse::<u16>().ok())
            .map(|p| vec![p])
            .unwrap_or_default(),
        None => Vec::new(),
    }
}

fn toml_section_field(section: &str, key: &str) -> Option<String> {
    for line in section.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix(key) else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let value = rest.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(value);
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

/// Append/refresh a `[proxy.<name>]` table in the config file.
/// SOCKS (cf-warp) defaults to an HTTP CONNECT expose so workers do not
/// receive `socks5h://127.0.0.1`. Existing expose/auth lines are kept
/// when the caller does not pass a new expose; auth is never generated
/// automatically because rsync `RSYNC_PROXY=host:port` cannot send it.
fn upsert_proxy_section(
    path: &PathBuf,
    name: &str,
    kind: &str,
    url: &str,
    expose: Option<&str>,
) -> Result<(), String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let header = format!("[proxy.{name}]");
    let old_section = text.find(&header).map(|start| {
        let end = text[start..]
            .find("\n[")
            .map(|i| start + i)
            .unwrap_or(text.len());
        text[start..end].to_string()
    });
    let expose = expose
        .map(str::to_string)
        .or_else(|| {
            old_section
                .as_deref()
                .and_then(|section| toml_section_field(section, "expose"))
        })
        .or_else(|| {
            if kind == "socks5h" {
                Some("0.0.0.0:14000".into())
            } else {
                None
            }
        });
    let expose_auth = old_section
        .as_deref()
        .and_then(|section| toml_section_field(section, "expose_auth"));
    let mut section = format!("\n[proxy.{name}]\ntype = \"{kind}\"\nurl = \"{url}\"\n");
    if let Some(e) = expose {
        section.push_str(&format!("expose = \"{e}\"\n"));
    }
    if let Some(a) = expose_auth {
        section.push_str(&format!("expose_auth = \"{a}\"\n"));
    }

    let new_text = if let Some(start) = text.find(&format!("[proxy.{name}]")) {
        // Replace the existing section: from its header to the next
        // `[section]` header (keep that newline) or EOF. The leading
        // newline of `section` is only the ADD-branch separator —
        // `text[..start]` already ends with the newline before the
        // header, so both ends must be trimmed or every replace
        // inserts one more blank line (idempotency: run N+1 == run N).
        let end = text[start..]
            .find("\n[")
            .map(|i| start + i)
            .unwrap_or(text.len());
        format!(
            "{}{}{}",
            &text[..start],
            section.trim_matches('\n'),
            // Mid-file the next `[` header keeps one newline; at EOF keep
            // exactly one trailing newline (else add-then-replace would
            // differ by one byte and never stabilize).
            if end == text.len() {
                "\n"
            } else {
                &text[end..]
            }
        )
    } else {
        // Append with exactly one blank-line separator, even when the
        // file does not end with a newline.
        let sep = if text.ends_with('\n') { "" } else { "\n" };
        format!("{text}{sep}{section}")
    };
    std::fs::write(path, new_text).map_err(|e| format!("write {}: {e}", path.display()))
}

/// Append a `[[jobs]]` entry to the config file (TUI "new job" form).
fn upsert_job_section(
    path: &PathBuf,
    name: &str,
    provider: &str,
    upstream: &str,
    storage: &str,
    schedule: &str,
) -> Result<(), String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let needle = format!("name = \"{name}\"");
    if text.contains(&needle) {
        return Err(format!("job `{name}` already exists in the config"));
    }
    // Schedule → config fields: cron / daily HH:MM / interval Nh / manual.
    let schedule_line = if schedule.contains('*') || schedule.split_whitespace().count() == 5 {
        format!("schedule = \"cron\"\ncron = \"{schedule}\"")
    } else if schedule.starts_with("daily") {
        let at = schedule.split_whitespace().nth(1).unwrap_or("03:00");
        format!("schedule = \"daily\"\nat = \"{at}\"")
    } else if schedule.starts_with("interval") {
        let every = schedule.split_whitespace().nth(1).unwrap_or("6h");
        format!("schedule = \"interval\"\nevery = \"{every}\"")
    } else if schedule.starts_with("manual") {
        "schedule = \"manual\"".to_string()
    } else {
        format!("schedule = \"cron\"\ncron = \"{schedule}\"")
    };
    let entry = format!(
        "\n[[jobs]]\nname = \"{name}\"\n{schedule_line}\nprovider = \"{provider}\"\nupstream = \"{upstream}\"\nstorage = \"{storage}\"\n"
    );
    std::fs::write(path, format!("{text}{entry}"))
        .map_err(|e| format!("write {}: {e}", path.display()))
}

/// Build the config editor file list: the main config, its include globs,
/// and the worker config when present next to it.
fn load_config_files(app: &mut App) {
    let mut files: Vec<PathBuf> = Vec::new();
    if let Some(main) = &app.cfg_path {
        files.push(main.clone());
        if let Ok(text) = std::fs::read_to_string(main) {
            for line in text.lines() {
                let t = line.trim();
                let Some(rest) = t.strip_prefix("include").and_then(|r| {
                    r.trim_start_matches('=')
                        .trim()
                        .strip_prefix('[')
                        .map(|_| r.trim_start_matches('=').trim())
                        .or(Some(r.trim_start_matches('=').trim()))
                }) else {
                    continue;
                };
                // Both `include = "glob"` and `include = ["a", "b"]` forms.
                let pats: Vec<&str> =
                    if let Some(inner) = rest.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
                        inner
                            .split(',')
                            .filter_map(|p| {
                                let p = p.trim();
                                p.strip_prefix('"').and_then(|s| s.strip_suffix('"'))
                            })
                            .collect()
                    } else {
                        rest.strip_prefix('"')
                            .and_then(|s| s.strip_suffix('"'))
                            .into_iter()
                            .collect()
                    };
                let base = main.parent().unwrap_or(std::path::Path::new("."));
                for pat in pats {
                    let full = base.join(pat);
                    if let Ok(paths) = glob::glob(&full.to_string_lossy()) {
                        for p in paths.flatten() {
                            files.push(p);
                        }
                    }
                }
            }
        }
        if let Some(dir) = main.parent() {
            let worker = dir.join("worker.toml");
            if worker.exists() {
                files.push(worker);
            }
        }
    }
    files.sort();
    files.dedup();
    app.config_files = files;
    app.config_idx = 0;
    load_config_file(app);
}

fn load_config_file(app: &mut App) {
    app.file_lines = app
        .config_files
        .get(app.config_idx)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| t.lines().map(String::from).collect())
        .unwrap_or_default();
    if app.file_lines.is_empty() {
        app.file_lines.push(String::new());
    }
    app.cur_row = 0;
    app.cur_col = 0;
    app.dirty = false;
}

/// Set the `worker` field of an existing job in its config file.
fn upsert_job_worker(
    config_path: &std::path::Path,
    job_name: &str,
    worker: &str,
) -> Result<(), String> {
    let dir = config_path.parent().unwrap_or(std::path::Path::new("."));
    let mut done = false;
    for entry in glob::glob(&format!("{}/**/*.toml", dir.display()))
        .map_err(|e| e.to_string())?
        .flatten()
    {
        let text = std::fs::read_to_string(&entry).map_err(|e| e.to_string())?;
        if !text.contains(&format!("name = \"{job_name}\"")) {
            continue;
        }
        let mut out = String::new();
        let mut in_job = false;
        let mut handled = false;
        for line in text.lines() {
            if line.trim() == "[[jobs]]" {
                in_job = true;
            } else if in_job && line.trim().starts_with("name =") {
                in_job = line.contains(job_name);
            }
            if in_job
                && !handled
                && (line.trim().starts_with("worker =") || line.trim() == "enabled")
            {
                // Replace an existing worker line; keep enabled as anchor.
            }
            if in_job && !handled && line.trim().starts_with("worker =") {
                out.push_str(&format!("worker = \"{worker}\"\n"));
                handled = true;
                continue;
            }
            out.push_str(line);
            out.push('\n');
        }
        if !handled {
            // No worker line: insert right after the name line.
            let name_line = format!("name = \"{job_name}\"\n");
            let pos = out.find(&name_line).map(|i| i + name_line.len());
            if let Some(pos) = pos {
                out.insert_str(pos, &format!("worker = \"{worker}\"\n"));
                handled = true;
            }
        }
        if handled {
            std::fs::write(&entry, out).map_err(|e| e.to_string())?;
            done = true;
        }
    }
    if done {
        Ok(())
    } else {
        Err(format!("job `{job_name}` not found in any config file"))
    }
}

/// Flatten a JobSpec JSON into (field, value) rows for the structured
/// editor; nested tables (hooks/safety/snapshot/verify) are rendered as
/// their own rows. Fields not set show their default.
fn spec_to_editor(json: &serde_json::Value) -> serde_json::Value {
    let Some(obj) = json.as_object() else {
        return json.clone();
    };
    let mut out = serde_json::Map::new();
    let copy_keys = [
        "name",
        "enabled",
        "worker",
        "upstream",
        "storage",
        "storage_name",
        "mirror_subdir",
        "proxy",
        "egress",
        "family",
        "retry",
        "retry_backoff",
        "fail_on_match",
        "max_concurrency",
        "misfire_policy",
        "on_worker_lost",
        "timezone",
        "statistics",
        "priority",
        "cpu_limit",
        "image",
        "keep_container",
        "docker_network",
        "parser",
        "delete",
        "threads",
        "stage1_profile",
        "command",
        "branch",
        "every",
        "at",
        "cron",
        "weekday",
    ];
    for key in copy_keys {
        if let Some(v) = obj.get(key) {
            if !v.is_object() {
                out.insert(key.to_string(), v.clone());
            }
        }
    }
    for key in [
        "options",
        "exclude",
        "env",
        "volumes",
        "docker_command",
        "success_exit_codes",
        "depends_on",
        "resources",
    ] {
        if let Some(v) = obj.get(key) {
            out.insert(key.to_string(), v.clone());
        }
    }
    if let Some(p) = obj.get("provider") {
        if let Some(s) = p.as_str() {
            out.insert("provider".into(), serde_json::Value::String(s.to_string()));
        } else if let Some(po) = p.as_object() {
            if let Some(ty) = po.get("type").and_then(|v| v.as_str()) {
                out.insert("provider".into(), serde_json::Value::String(ty.to_string()));
            }
            for (k, v) in po {
                if k == "type" {
                    continue;
                }
                let dest = if k == "command" {
                    "docker_command"
                } else {
                    k.as_str()
                };
                out.insert(dest.to_string(), v.clone());
            }
        }
    }
    if let Some(s) = obj.get("schedule") {
        if s.is_string() {
            out.insert("schedule".into(), s.clone());
        } else {
            for (k, v) in schedule_to_editor(s) {
                out.insert(k, v);
            }
        }
    }
    if let Some(v) = obj.get("memory_limit") {
        out.insert("memory_limit".into(), format_memory_field(v));
    }
    if let Some(v) = obj.get("timeout") {
        out.insert("timeout".into(), format_duration_field(v));
    }
    if let Some(v) = obj.get("retry_delay") {
        out.insert("retry_delay".into(), format_duration_field(v));
    }
    let provider = out
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if matches!(provider.as_str(), "rsync" | "two-stage-rsync") {
        out.entry("options".to_string())
            .or_insert_with(|| serde_json::Value::Array(vec![]));
        out.entry("exclude".to_string())
            .or_insert_with(|| serde_json::Value::Array(vec![]));
    }
    if provider == "two-stage-rsync" {
        out.entry("stage1_profile".to_string())
            .or_insert_with(|| serde_json::Value::String("debian".into()));
    }
    serde_json::Value::Object(out)
}

fn schedule_to_editor(v: &serde_json::Value) -> Vec<(String, serde_json::Value)> {
    let kind = v
        .get("kind")
        .and_then(|k| k.get("kind").or(Some(k)))
        .and_then(|k| k.as_str())
        .unwrap_or("manual");
    let mut out = vec![(
        "schedule".into(),
        serde_json::Value::String(kind.to_string()),
    )];
    match kind {
        "interval" => {
            if let Some(every) = v.pointer("/kind/every").or_else(|| v.get("every")) {
                out.push(("every".into(), format_duration_field(every)));
            }
        }
        "daily" => {
            if let Some(at) = v.pointer("/kind/at").or_else(|| v.get("at")) {
                out.push(("at".into(), serde_json::Value::String(json_scalar(at))));
            }
        }
        "cron" => {
            if let Some(expr) = v.pointer("/kind/expr").or_else(|| v.get("cron")) {
                out.push(("cron".into(), serde_json::Value::String(json_scalar(expr))));
            }
        }
        "weekly" => {
            if let Some(w) = v.pointer("/kind/weekday").or_else(|| v.get("weekday")) {
                out.push(("weekday".into(), serde_json::Value::String(json_scalar(w))));
            }
            if let Some(at) = v.pointer("/kind/at").or_else(|| v.get("at")) {
                out.push(("at".into(), serde_json::Value::String(json_scalar(at))));
            }
        }
        _ => {}
    }
    out
}

fn duration_secs(v: &serde_json::Value) -> Option<i64> {
    if let Some(n) = v.as_i64() {
        return Some(n);
    }
    if let Some(n) = v.as_u64() {
        return Some(n as i64);
    }
    if let Some(arr) = v.as_array() {
        return arr
            .first()
            .and_then(|x| x.as_i64().or_else(|| x.as_u64().map(|n| n as i64)));
    }
    if let Some(obj) = v.as_object() {
        return obj
            .get("secs")
            .or_else(|| obj.get("whole_seconds"))
            .and_then(|x| x.as_i64().or_else(|| x.as_u64().map(|n| n as i64)));
    }
    None
}

fn format_duration_field(v: &serde_json::Value) -> serde_json::Value {
    if let Some(s) = v.as_str() {
        return serde_json::Value::String(s.to_string());
    }
    let Some(secs) = duration_secs(v) else {
        return serde_json::Value::String(json_scalar(v));
    };
    if secs <= 0 || secs > 3600 * 24 * 365 * 10 {
        return serde_json::Value::String(String::new());
    }
    let text = if secs % 86400 == 0 {
        format!("{}d", secs / 86400)
    } else if secs % 3600 == 0 {
        format!("{}h", secs / 3600)
    } else if secs % 60 == 0 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    };
    serde_json::Value::String(text)
}

fn format_memory_field(v: &serde_json::Value) -> serde_json::Value {
    if let Some(s) = v.as_str() {
        return serde_json::Value::String(s.to_string());
    }
    let Some(bytes) = v.as_u64().or_else(|| v.as_i64().map(|n| n as u64)) else {
        return serde_json::Value::String(json_scalar(v));
    };
    if bytes == 0 {
        return serde_json::Value::String(String::new());
    }
    const G: u64 = 1024 * 1024 * 1024;
    const M: u64 = 1024 * 1024;
    let text = if bytes % G == 0 {
        format!("{}G", bytes / G)
    } else if bytes % M == 0 {
        format!("{}M", bytes / M)
    } else {
        bytes.to_string()
    };
    serde_json::Value::String(text)
}

fn json_array_strings(v: &serde_json::Value) -> Vec<String> {
    match v {
        serde_json::Value::Array(a) => a
            .iter()
            .map(json_scalar)
            .filter(|s| !s.is_empty())
            .collect(),
        serde_json::Value::String(s) if !s.is_empty() => vec![s.clone()],
        _ => Vec::new(),
    }
}

fn set_json_array(json: &mut serde_json::Value, field: &str, items: Vec<String>) {
    if let Some(obj) = json.as_object_mut() {
        obj.insert(
            field.to_string(),
            serde_json::Value::Array(items.into_iter().map(serde_json::Value::String).collect()),
        );
    }
}

fn rsync_extra_options() -> Vec<(String, String)> {
    [
        "--delete-excluded",
        "--timeout=1800",
        "--timeout=3600",
        "--contimeout=120",
        "--bwlimit=10240",
        "--bwlimit=20480",
        "--password-file=/etc/synora/syncpassword/",
        "--exclude-from=/etc/synora/excludes/",
        "--exclude=",
        "--include=",
        "--chmod=o+r,Do+x,Fa-x",
        "--partial",
        "--inplace",
        "--checksum",
        "--no-motd",
        "--ignore-existing",
        "--max-delete=10000",
    ]
    .into_iter()
    .map(|s| (s.to_string(), s.to_string()))
    .chain(std::iter::once(("自定义…".into(), String::new())))
    .collect()
}

fn flatten_spec(json: &serde_json::Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(obj) = json.as_object() {
        for (k, v) in obj {
            let val = match v {
                serde_json::Value::Null => "".to_string(),
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Array(a) => {
                    let items: Vec<String> = a.iter().map(json_scalar).collect();
                    format!("[{}]", items.join(", "))
                }
                serde_json::Value::Object(_) => continue, // nested tables handled below
                other => other.to_string(),
            };
            out.push((k.clone(), val));
        }
    }
    out.sort();
    out
}

/// Rewrite a job's whole [[jobs]] block from its spec JSON (the
/// structured editor's save path). Nested tables (hooks/safety/snapshot/
/// verify) render as [jobs.xxx] subtables.
fn upsert_job_block(
    config_path: &std::path::Path,
    job_name: &str,
    json: &serde_json::Value,
) -> Result<(), String> {
    let dir = config_path.parent().unwrap_or(std::path::Path::new("."));
    let mut toml_lines = vec!["[[jobs]]".to_string(), format!("name = \"{job_name}\"")];
    let mut nested: Vec<(String, String)> = Vec::new();
    if let Some(obj) = json.as_object() {
        let mut keys: Vec<&String> = obj.keys().filter(|k| k.as_str() != "name").collect();
        keys.sort();
        for k in keys {
            let v = &obj[k];
            if v.is_object() {
                for (nk, nv) in v.as_object().unwrap() {
                    nested.push((format!("{k}.{nk}"), json_scalar(nv)));
                }
                continue;
            }
            let val = match v {
                serde_json::Value::String(s) => format!("\"{}\"", s.replace('\n', "\\n")),
                serde_json::Value::Array(a) => {
                    let items: Vec<String> = a
                        .iter()
                        .map(json_scalar)
                        .map(|x| format!("\"{x}\"").replace('\n', "\\n"))
                        .collect();
                    format!("[{}]", items.join(", "))
                }
                serde_json::Value::Null => continue,
                other => other.to_string(),
            };
            toml_lines.push(format!("{k} = {val}"));
        }
    }
    let block = toml_lines.join("\n");
    let mut done = false;
    for entry in glob::glob(&format!("{}/**/*.toml", dir.display()))
        .map_err(|e| e.to_string())?
        .flatten()
    {
        let text = std::fs::read_to_string(&entry).map_err(|e| e.to_string())?;
        if !text.contains(&format!("name = \"{job_name}\"")) {
            continue;
        }
        // Split on the section marker and rebuild the matching block.
        let mut parts: Vec<&str> = text.split("[[jobs]]").collect();
        let mut rebuilt = String::new();
        let mut replaced = false;
        for part in parts.drain(..) {
            if !replaced && part.contains(&format!("name = \"{job_name}\"")) {
                // Drop the old block (up to the next section header).
                let cut = part.find("\n[[").unwrap_or(part.len());
                rebuilt.push_str("[[jobs]]");
                rebuilt.push_str(&block);
                rebuilt.push_str(part[cut..].trim_start_matches('\n'));
                rebuilt.push('\n');
                replaced = true;
            } else {
                if !rebuilt.is_empty() || !part.is_empty() {
                    rebuilt.push_str("[[jobs]]");
                    rebuilt.push_str(part);
                }
            }
        }
        if replaced {
            std::fs::write(&entry, rebuilt).map_err(|e| e.to_string())?;
            done = true;
        }
    }
    if done {
        Ok(())
    } else {
        Err(format!("job `{job_name}` not found in any config file"))
    }
}

/// Set a top-level field of the spec JSON, guessing the value type from
/// the current value (numbers stay numbers, arrays parse as arrays of
/// strings, booleans stay booleans; everything else is a string).
fn set_json_field(json: &mut serde_json::Value, field: &str, value: &str) {
    let Some(obj) = json.as_object_mut() else {
        return;
    };
    if value.is_empty() && !matches!(obj.get(field), Some(serde_json::Value::Array(_))) {
        obj.insert(field.to_string(), serde_json::Value::Null);
        return;
    }
    let new_value = match obj.get(field) {
        Some(serde_json::Value::Number(_)) => value
            .parse::<i64>()
            .map(serde_json::Value::from)
            .unwrap_or_else(|_| {
                value
                    .parse::<f64>()
                    .map(serde_json::Value::from)
                    .unwrap_or_else(|_| serde_json::Value::String(value.to_string()))
            }),
        Some(serde_json::Value::Bool(_)) => serde_json::Value::Bool(value == "true"),
        Some(serde_json::Value::Array(_)) => serde_json::Value::Array(
            value
                .split(',')
                .map(|s| serde_json::Value::String(s.trim().to_string()))
                .collect(),
        ),
        _ => serde_json::Value::String(value.to_string()),
    };
    obj.insert(field.to_string(), new_value);
}

fn json_scalar(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn provider_options() -> Vec<(String, String)> {
    [
        "rsync",
        "two-stage-rsync",
        "http",
        "git",
        "docker",
        "script",
    ]
    .into_iter()
    .map(|s| (s.to_string(), s.to_string()))
    .collect()
}

fn schedule_options() -> Vec<(String, String)> {
    [
        ("manual", "manual"),
        ("interval 6h", "interval 6h"),
        ("interval 12h", "interval 12h"),
        ("daily 03:00", "daily 03:00"),
        ("interval 1d", "interval 1d"),
    ]
    .into_iter()
    .map(|(l, v)| (l.to_string(), v.to_string()))
    .collect()
}

fn pick_options_for_field(field: &str, snap: &Snapshot) -> Option<Vec<(String, String)>> {
    match field {
        "enabled" => Some(vec![
            ("true".into(), "true".into()),
            ("false".into(), "false".into()),
        ]),
        "family" => Some(vec![
            ("ipv4".into(), "ipv4".into()),
            ("ipv6".into(), "ipv6".into()),
            ("any".into(), "any".into()),
        ]),
        "parser" => Some(
            ["nginx", "apache", "caddy", "s3", "debian"]
                .into_iter()
                .map(|s| (s.to_string(), s.to_string()))
                .collect(),
        ),
        "provider" => Some(provider_options()),
        "schedule" => Some(schedule_options()),
        "memory_limit" => Some(
            ["(none)", "1G", "2G", "4G", "8G", "16G"]
                .into_iter()
                .map(|s| {
                    let v = if s == "(none)" { "" } else { s };
                    (s.to_string(), v.to_string())
                })
                .collect(),
        ),
        "proxy" => {
            let mut opts = vec![("(none)".into(), "".into())];
            for p in &snap.proxies {
                if let Some(name) = p.get("name").and_then(|v| v.as_str()) {
                    if !opts.iter().any(|(_, v)| v == name) {
                        opts.push((name.to_string(), name.to_string()));
                    }
                }
            }
            Some(opts)
        }
        "worker" => {
            let mut opts = vec![("auto".into(), "".into())];
            for w in &snap.workers {
                opts.push((w.id.clone(), w.id.clone()));
            }
            Some(opts)
        }
        "stage1_profile" => Some(
            ["debian", "debian-oldstyle"]
                .into_iter()
                .map(|s| (s.to_string(), s.to_string()))
                .collect(),
        ),
        "statistics" => Some(vec![
            ("provider".into(), "provider".into()),
            ("filesystem".into(), "filesystem".into()),
        ]),
        "misfire_policy" => Some(vec![
            ("skip".into(), "skip".into()),
            ("run-immediately".into(), "run-immediately".into()),
            ("run-next".into(), "run-next".into()),
        ]),
        "on_worker_lost" => Some(vec![
            ("retry".into(), "retry".into()),
            ("fail".into(), "fail".into()),
        ]),
        "keep_container" | "delete" => Some(vec![
            ("true".into(), "true".into()),
            ("false".into(), "false".into()),
        ]),
        "docker_network" => Some(vec![
            ("bridge".into(), "bridge".into()),
            ("host".into(), "host".into()),
            ("none".into(), "none".into()),
        ]),
        _ => None,
    }
}

/// Register CF One / WARP: detect the local proxy port and add it to config.
async fn register_cf_warp(client: &Client, cfg_path: &Option<PathBuf>) -> Result<String, String> {
    let ports = detect_local_socks();
    if ports.is_empty() {
        return Err(
            "no local proxy port found (checked 40000/1080/7890/…); is CF One/WARP running?".into(),
        );
    }
    let port = ports[0];
    let path = cfg_path
        .clone()
        .ok_or("TUI needs -c <config> to register proxies".to_string())?;
    upsert_proxy_section(
        &path,
        "cf-warp",
        "socks5h",
        &format!("socks5h://127.0.0.1:{port}"),
        None,
    )?;
    let applied = client.reload().await.map_err(|e| e.to_string())?;
    Ok(format!(
        "registered cf-warp at socks5h://127.0.0.1:{port} (reload applied {applied} job(s))"
    ))
}

/// Entry point used by `synora tui` and the standalone `synora-tui` binary.
pub fn run(
    config: Option<PathBuf>,
    manager: Option<String>,
    token: Option<String>,
) -> Result<(), String> {
    // The config path used for writes is the one that resolved (defaults to
    // /etc/synora/synora.toml when no -c is given).
    let (resolved_path, url, token) = resolve_creds(config.clone(), manager, token)?;
    let cfg_path = Some(resolved_path);
    let client = Client::new(&url, &token).map_err(|e| e.to_string())?;

    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    let snap = Arc::new(Mutex::new(Snapshot::default()));

    // Background refresh every 2s: world data + the selected job's
    // logs/history/spec. The render loop never blocks on the network
    // (a slow manager must not freeze the UI).
    let selected_shared = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let detail_shared = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let logs_shared = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let edit_job_shared = std::sync::Arc::new(tokio::sync::Mutex::new(None::<String>));
    {
        let snap = snap.clone();
        let client = client.clone();
        let selected_shared = selected_shared.clone();
        let detail_shared = detail_shared.clone();
        let logs_shared = logs_shared.clone();
        let edit_job_shared = edit_job_shared.clone();
        rt.spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(2));
            loop {
                tick.tick().await;
                // Fetch into a local snapshot; the mutex is only held for
                // the brief clone and the final swap. Holding it across
                // the awaits below froze the render loop (which blocks on
                // the same mutex) for every HTTP round-trip.
                let mut s = snap.lock().await.clone();
                fetch(&client, &mut s).await;
                let frozen = edit_job_shared.lock().await.clone();
                let idx = selected_shared.load(Ordering::Relaxed);
                let name = frozen.or_else(|| s.jobs.get(idx).map(|j| j.name.clone()));
                if let Some(name) = name {
                    // Only pull logs while the Logs panel is open.
                    if logs_shared.load(Ordering::Relaxed)
                        && (s.log_job.as_deref() != Some(name.as_str()) || s.log_lines.is_empty())
                    {
                        s.log_job = Some(name.clone());
                        if let Ok(log) = client.job_logs(&name, 200).await {
                            s.log_lines = log.lines().map(String::from).collect();
                        }
                    }
                    if detail_shared.load(Ordering::Relaxed) {
                        if let Ok(h) = client.job_history(&name).await {
                            s.history = h;
                        }
                        // Spec: fetch when the selection changed or there is
                        // none yet. Never re-fetch every tick — that would
                        // clobber edits made in the spec editor with the
                        // stale clone from before the fetch.
                        if s.spec_job.as_deref() != Some(name.as_str()) || s.spec_json.is_none() {
                            if let Ok(spec) = client.job_spec(&name).await {
                                let json = spec_to_editor(
                                    &serde_json::to_value(&spec).unwrap_or(serde_json::Value::Null),
                                );
                                s.spec_fields = flatten_spec(&json);
                                s.spec_json = Some(json);
                                s.spec_job = Some(name.clone());
                            }
                        }
                    }
                }
                *snap.lock().await = s;
            }
        });
    }

    let mut app = App::new(cfg_path);
    // Auto-register CF One / WARP on startup (self-register; do not add
    // it by hand) — detect the local proxy port, add it to the config,
    // hot-reload. Runs in the background so the console opens immediately;
    // silent when nothing is running locally.
    if let Some(cfg) = app.cfg_path.clone() {
        let client = client.clone();
        rt.spawn(async move {
            if let Ok(_m) = register_cf_warp(&client, &Some(cfg)).await {
                // notice is set on the next render via shared state
            }
        });
    }

    let mut terminal = ratatui::try_init().map_err(|e| {
        format!(
            "cannot initialize the terminal ({e}). Is TERM set? The TUI needs a real terminal — try `export TERM=xterm-256color` and avoid dumb terminals."
        )
    })?;
    let result = loop {
        // Sync the shared selection state to the background fetcher.
        if let Ok(mut g) = edit_job_shared.try_lock() {
            *g = app.edit_job.clone();
        }
        let snap_view = match snap.try_lock() {
            Ok(g) => g.clone(),
            Err(_) => continue,
        };
        if let Some(name) = app.edit_job.as_deref() {
            if let Some((idx, _)) = snap_view
                .jobs
                .iter()
                .enumerate()
                .find(|(_, j)| j.name == name)
            {
                app.selected = idx;
                app.jobs_state.select(Some(idx));
            }
        }
        selected_shared.store(app.selected, Ordering::Relaxed);
        detail_shared.store(
            app.mode == Mode::JobDetail || app.mode == Mode::SpecEdit,
            Ordering::Relaxed,
        );
        logs_shared.store(app.mode == Mode::Logs, Ordering::Relaxed);
        if std::env::var("SYNORA_TUI_DEBUG").is_ok() {
            eprintln!(
                "DEBUG jobs={} workers={} err={:?}",
                snap_view.jobs.len(),
                snap_view.workers.len(),
                snap_view.error
            );
        }
        let _ = terminal.draw(|f| render(f, &mut app, &snap_view));
        if !event::poll(Duration::from_millis(400)).map_err(|e| e.to_string())? {
            continue;
        }
        let ev = event::read().map_err(|e| e.to_string())?;
        let Event::Key(key) = ev else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        // Input overlay first: characters fill the buffer, Enter advances.
        match &mut app.input {
            Input::None => {}
            Input::ListEdit { .. } => {
                let (field, mut items, mut idx) = match &app.input {
                    Input::ListEdit { field, items, idx } => (field.clone(), items.clone(), *idx),
                    _ => unreachable!(),
                };
                let mut reopen = None;
                match key.code {
                    KeyCode::Esc => {
                        let mut s = snap.blocking_lock();
                        if let Some(json) = s.spec_json.as_mut() {
                            set_json_array(json, &field, items);
                            s.spec_fields = flatten_spec(json);
                        }
                        app.input = Input::None;
                        continue;
                    }
                    KeyCode::Up => idx = idx.saturating_sub(1),
                    KeyCode::Down => {
                        if !items.is_empty() {
                            idx = (idx + 1).min(items.len().saturating_sub(1));
                        }
                    }
                    KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace => {
                        if idx < items.len() {
                            items.remove(idx);
                            if idx >= items.len() {
                                idx = items.len().saturating_sub(1);
                            }
                        }
                    }
                    KeyCode::Char('a') | KeyCode::Enter => {
                        let options = if field == "options" {
                            rsync_extra_options()
                        } else if field == "exclude" {
                            vec![
                                ("lost+found".into(), "lost+found".into()),
                                (".~tmp~/".into(), ".~tmp~/".into()),
                                ("自定义…".into(), String::new()),
                            ]
                        } else {
                            vec![("自定义…".into(), String::new())]
                        };
                        reopen = Some(Input::Pick {
                            title: format!(" add {field} "),
                            options,
                            idx: 0,
                            kind: PickKind::ListAppend {
                                field: field.clone(),
                            },
                        });
                    }
                    _ => {}
                }
                app.input = reopen.unwrap_or(Input::ListEdit { field, items, idx });
                continue;
            }
            _ => {
                match key.code {
                    KeyCode::Esc => {
                        app.input = Input::None;
                        app.input_buf.clear();
                    }
                    KeyCode::Backspace => {
                        app.input_buf.pop();
                    }
                    KeyCode::Enter => {
                        let buf = std::mem::take(&mut app.input_buf);
                        match std::mem::replace(&mut app.input, Input::None) {
                            Input::Search => {
                                app.search = buf.trim().to_string();
                            }
                            Input::SpecValue { field } => {
                                // Merge into the snapshot spec JSON (saved
                                // via S in the SpecEdit view).
                                {
                                    let mut s = snap.blocking_lock();
                                    if let Some(json) = s.spec_json.as_mut() {
                                        if let Some(name) = field.strip_suffix("[]") {
                                            let mut items = json
                                                .get(name)
                                                .map(json_array_strings)
                                                .unwrap_or_default();
                                            let item = buf.trim();
                                            if !item.is_empty() && !items.iter().any(|x| x == item)
                                            {
                                                items.push(item.to_string());
                                            }
                                            set_json_array(json, name, items.clone());
                                            s.spec_fields = flatten_spec(json);
                                            app.input = Input::ListEdit {
                                                field: name.to_string(),
                                                items,
                                                idx: 0,
                                            };
                                        } else {
                                            set_json_field(json, &field, buf.trim());
                                            s.spec_fields = flatten_spec(json);
                                        }
                                    }
                                }
                                app.mode = Mode::SpecEdit;
                            }
                            Input::WorkerSelect { job, candidates } => {
                                let sel = buf.trim().parse::<usize>().unwrap_or(0);
                                let worker = candidates.get(sel).cloned().unwrap_or_default();
                                if !worker.is_empty() {
                                    let cfg = app.cfg_path.clone();
                                    let client = client.clone();
                                    let job2 = job.clone();
                                    let result = rt.block_on(async move {
                                        let path = cfg.ok_or("TUI needs -c <config> to assign workers".to_string())?;
                                        upsert_job_worker(&path, &job2, &worker)?;
                                        let applied = client.reload().await.map_err(|e| e.to_string())?;
                                        Ok::<String, String>(format!("job `{job2}` pinned to worker `{worker}`, reload applied {applied} job(s)"))
                                    });
                                    app.notice = Some(match result {
                                        Ok(m) => m,
                                        Err(e) => format!("error: {e}"),
                                    });
                                }
                            }
                            Input::ProxyName => {
                                app.input = Input::ProxyUrl { name: buf };
                            }
                            Input::ProxyUrl { name } => {
                                app.input = Input::ProxyExpose { name, url: buf };
                            }
                            Input::ProxyExpose { name, url } => {
                                // Commit: write config + reload.
                                let kind = if url.starts_with("socks5h://") {
                                    "socks5h"
                                } else {
                                    "http"
                                };
                                let expose = if buf.trim().is_empty() {
                                    None
                                } else {
                                    Some(buf.trim().to_string())
                                };
                                let cfg = app.cfg_path.clone();
                                let client = client.clone();
                                let result = rt.block_on(async move {
                                    let path = cfg.ok_or("TUI needs -c <config> to add proxies".to_string())?;
                                    upsert_proxy_section(&path, &name, kind, &url, expose.as_deref())?;
                                    let applied = client.reload().await.map_err(|e| e.to_string())?;
                                    Ok::<String, String>(format!("proxy `{name}` added ({url}), reload applied {applied} job(s)"))
                                });
                                app.notice = match result {
                                    Ok(m) => Some(m),
                                    Err(e) => Some(format!("error: {e}")),
                                };
                            }
                            Input::JobName => {
                                app.input = Input::Pick {
                                    title: " Provider ".into(),
                                    options: provider_options(),
                                    idx: 0,
                                    kind: PickKind::NewProvider { name: buf },
                                };
                            }
                            Input::JobUpstream { name, provider } => {
                                app.input = Input::JobStorage {
                                    name,
                                    provider,
                                    upstream: buf,
                                };
                            }
                            Input::JobStorage {
                                name,
                                provider,
                                upstream,
                            } => {
                                app.input = Input::Pick {
                                    title: " Schedule ".into(),
                                    options: schedule_options(),
                                    idx: 0,
                                    kind: PickKind::NewSchedule {
                                        name,
                                        provider,
                                        upstream,
                                        storage: buf,
                                    },
                                };
                            }
                            Input::Pick {
                                options, idx, kind, ..
                            } => {
                                let value =
                                    options.get(idx).map(|(_, v)| v.clone()).unwrap_or_default();
                                match kind {
                                    PickKind::NewProvider { name } => {
                                        app.input = Input::JobUpstream {
                                            name,
                                            provider: value,
                                        };
                                    }
                                    PickKind::NewSchedule {
                                        name,
                                        provider,
                                        upstream,
                                        storage,
                                    } => {
                                        let schedule = value;
                                        let cfg = app.cfg_path.clone();
                                        let client = client.clone();
                                        let result = rt.block_on(async move {
                                            let path = cfg.ok_or("TUI needs -c <config> to add jobs".to_string())?;
                                            upsert_job_section(&path, &name, &provider, &upstream, &storage, &schedule)?;
                                            let applied = client.reload().await.map_err(|e| e.to_string())?;
                                            Ok::<String, String>(format!("job `{name}` created ({provider}, schedule {schedule}), reload applied {applied} job(s)"))
                                        });
                                        app.notice = match result {
                                            Ok(m) => Some(m),
                                            Err(e) => Some(format!("error: {e}")),
                                        };
                                    }
                                    PickKind::SpecField { field } => {
                                        {
                                            let mut s = snap.blocking_lock();
                                            if let Some(json) = s.spec_json.as_mut() {
                                                set_json_field(json, &field, value.trim());
                                                s.spec_fields = flatten_spec(json);
                                            }
                                        }
                                        app.mode = Mode::SpecEdit;
                                    }
                                    PickKind::ListAppend { field } => {
                                        if value.is_empty() {
                                            app.input = Input::SpecValue {
                                                field: format!("{field}[]"),
                                            };
                                            app.input_buf.clear();
                                        } else if value.ends_with('=') || value.ends_with('/') {
                                            app.input = Input::SpecValue {
                                                field: format!("{field}[]"),
                                            };
                                            app.input_buf = value;
                                        } else {
                                            let mut s = snap.blocking_lock();
                                            if let Some(json) = s.spec_json.as_mut() {
                                                let mut items = json
                                                    .get(&field)
                                                    .map(json_array_strings)
                                                    .unwrap_or_default();
                                                if !items.contains(&value) {
                                                    items.push(value);
                                                }
                                                set_json_array(json, &field, items.clone());
                                                s.spec_fields = flatten_spec(json);
                                                app.input = Input::ListEdit {
                                                    field,
                                                    items,
                                                    idx: 0,
                                                };
                                            }
                                        }
                                        app.mode = Mode::SpecEdit;
                                    }
                                }
                            }
                            Input::None => {}
                            Input::ListEdit { .. } => {}
                        }
                    }
                    KeyCode::Up | KeyCode::Down => {
                        if let Input::WorkerSelect { candidates, .. } = &app.input {
                            let cur = app.input_buf.trim().parse::<usize>().unwrap_or(0);
                            let next = match key.code {
                                KeyCode::Up => cur.saturating_sub(1),
                                _ => (cur + 1).min(candidates.len().saturating_sub(1)),
                            };
                            app.input_buf = next.to_string();
                        }
                        if let Input::Pick { options, idx, .. } = &mut app.input {
                            *idx = match key.code {
                                KeyCode::Up => idx.saturating_sub(1),
                                _ => (*idx + 1).min(options.len().saturating_sub(1)),
                            };
                        }
                    }
                    KeyCode::Char(c) => app.input_buf.push(c),
                    _ => {}
                }
                continue;
            }
        }

        // Structured job editor: arrows select a field, Enter edits its
        // value, S rewrites the whole job block and reloads.
        if app.mode == Mode::SpecEdit {
            match key.code {
                KeyCode::Esc => {
                    app.mode = Mode::JobDetail;
                    app.edit_job = None;
                }
                KeyCode::Up => app.spec_field = app.spec_field.saturating_sub(1),
                KeyCode::Down => {
                    app.spec_field =
                        (app.spec_field + 1).min(snap_view.spec_fields.len().saturating_sub(1));
                }
                KeyCode::Enter => {
                    if let Some((field, value)) = snap_view.spec_fields.get(app.spec_field) {
                        if matches!(
                            field.as_str(),
                            "options"
                                | "exclude"
                                | "env"
                                | "volumes"
                                | "docker_command"
                                | "depends_on"
                        ) {
                            let items = snap_view
                                .spec_json
                                .as_ref()
                                .and_then(|j| j.get(field))
                                .map(json_array_strings)
                                .unwrap_or_default();
                            app.input = Input::ListEdit {
                                field: field.clone(),
                                items,
                                idx: 0,
                            };
                        } else if let Some(options) = pick_options_for_field(field, &snap_view) {
                            let idx = options.iter().position(|(_, v)| v == value).unwrap_or(0);
                            app.input = Input::Pick {
                                title: format!(" {field} "),
                                options,
                                idx,
                                kind: PickKind::SpecField {
                                    field: field.clone(),
                                },
                            };
                        } else {
                            app.input = Input::SpecValue {
                                field: field.clone(),
                            };
                            app.input_buf = value.clone();
                        }
                    }
                }
                KeyCode::Char('S') => {
                    if let Some(job) = app
                        .edit_job
                        .clone()
                        .or_else(|| snap_view.jobs.get(app.selected).map(|j| j.name.clone()))
                    {
                        let json = snap_view.spec_json.clone();
                        let cfg = app.cfg_path.clone();
                        let client = client.clone();
                        let result = rt.block_on(async move {
                            let path = cfg.ok_or("no config path".to_string())?;
                            let json = json.ok_or("no job spec loaded".to_string())?;
                            upsert_job_block(&path, &job, &json)?;
                            let applied = client.reload().await.map_err(|e| e.to_string())?;
                            Ok::<String, String>(format!(
                                "job `{job}` saved, reload applied {applied} job(s)"
                            ))
                        });
                        app.notice = Some(match result {
                            Ok(m) => m,
                            Err(e) => format!("error: {e}"),
                        });
                        app.mode = Mode::JobDetail;
                    }
                }
                _ => {}
            }
            continue;
        }

        // Config editor mode: full editing keys.
        if app.mode == Mode::Config {
            match key.code {
                KeyCode::Esc => {
                    if app.dirty {
                        app.notice = Some("unsaved changes discarded".to_string());
                    }
                    app.dirty = false;
                    app.mode = Mode::Jobs;
                }
                KeyCode::Tab => {
                    if !app.config_files.is_empty() {
                        app.config_idx = (app.config_idx + 1) % app.config_files.len();
                        load_config_file(&mut app);
                    }
                }
                KeyCode::Char('S') => {
                    if let Some(path) = app.config_files.get(app.config_idx).cloned() {
                        let content = app.file_lines.join("\n") + "\n";
                        let write = std::fs::write(&path, &content);
                        app.notice = Some(match write {
                            Ok(()) => {
                                app.dirty = false;
                                let is_worker = path
                                    .file_name()
                                    .map(|n| n == "worker.toml")
                                    .unwrap_or(false);
                                if is_worker {
                                    format!(
                                        "{} saved (restart the worker to apply)",
                                        path.display()
                                    )
                                } else {
                                    let client = client.clone();
                                    match rt.block_on(client.reload()) {
                                        Ok(n) => format!(
                                            "{} saved, reload applied {n} job(s)",
                                            path.display()
                                        ),
                                        Err(e) => {
                                            format!("{} saved, reload failed: {e}", path.display())
                                        }
                                    }
                                }
                            }
                            Err(e) => format!("error saving {}: {e}", path.display()),
                        });
                    }
                }
                KeyCode::Up => app.cur_row = app.cur_row.saturating_sub(1),
                KeyCode::Down => {
                    app.cur_row = (app.cur_row + 1).min(app.file_lines.len().saturating_sub(1));
                }
                KeyCode::Left => app.cur_col = app.cur_col.saturating_sub(1),
                KeyCode::Right => app.cur_col += 1,
                KeyCode::Enter => {
                    let line = app.file_lines.get(app.cur_row).cloned().unwrap_or_default();
                    let col = app.cur_col.min(line.len());
                    let (head, tail) = line.split_at(col);
                    app.file_lines[app.cur_row] = head.to_string();
                    app.file_lines.insert(app.cur_row + 1, tail.to_string());
                    app.cur_row += 1;
                    app.cur_col = 0;
                    app.dirty = true;
                }
                KeyCode::Backspace => {
                    let line = app.file_lines.get(app.cur_row).cloned().unwrap_or_default();
                    if app.cur_col > 0 {
                        let col = app.cur_col.min(line.len()).saturating_sub(1);
                        let mut l = line;
                        l.remove(col);
                        app.file_lines[app.cur_row] = l;
                        app.cur_col = col;
                    } else if app.cur_row > 0 {
                        let removed = app.file_lines.remove(app.cur_row);
                        app.cur_row -= 1;
                        let cur = &app.file_lines[app.cur_row];
                        app.cur_col = cur.len();
                        app.file_lines[app.cur_row] = format!("{cur}{removed}");
                    }
                    app.dirty = true;
                }
                KeyCode::Delete => {
                    if let Some(line) = app.file_lines.get(app.cur_row).cloned() {
                        if app.cur_col < line.len() && !line.is_empty() {
                            let mut l = line;
                            l.remove(app.cur_col);
                            app.file_lines[app.cur_row] = l;
                            app.dirty = true;
                        }
                    }
                }
                KeyCode::Char(c) => {
                    let line = app.file_lines.get(app.cur_row).cloned().unwrap_or_default();
                    let col = app.cur_col.min(line.len());
                    let mut l = line;
                    l.insert(col, c);
                    app.file_lines[app.cur_row] = l;
                    app.cur_col = col + 1;
                    app.dirty = true;
                }
                _ => {}
            }
            continue;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::F(10) => break Ok(()),
            KeyCode::F(1) => app.mode = Mode::Jobs,
            KeyCode::F(2) => app.mode = Mode::Workers,
            KeyCode::F(3) => app.mode = Mode::Proxies,
            KeyCode::F(5) => app.mode = Mode::Logs,
            KeyCode::F(6) => {
                load_config_files(&mut app);
                app.mode = Mode::Config;
            }
            KeyCode::Char('e') if app.mode == Mode::Proxies => {
                load_config_files(&mut app);
                app.mode = Mode::Config;
            }
            KeyCode::Char('e') if app.mode == Mode::JobDetail => {
                // Structured editor: freeze the job so arrows only move fields.
                app.edit_job = app.selected_job(&snap_view).map(|j| j.name.clone());
                app.spec_field = 0;
                app.mode = Mode::SpecEdit;
            }
            KeyCode::Up => {
                if app.mode == Mode::Workers {
                    let cur = app.workers_state.selected().unwrap_or(0);
                    let next = cur.saturating_sub(1);
                    app.workers_state.select(Some(next));
                } else {
                    app.selected = app.selected.saturating_sub(1);
                    app.jobs_state.select(Some(app.selected));
                }
            }
            KeyCode::Down => {
                if app.mode == Mode::Workers {
                    let cur = app.workers_state.selected().unwrap_or(0);
                    let next = (cur + 1).min(snap_view.workers.len().saturating_sub(1));
                    app.workers_state.select(Some(next));
                } else {
                    app.selected += 1;
                    app.jobs_state.select(Some(app.selected));
                }
            }
            KeyCode::Char('d') if app.mode == Mode::Workers => {
                let idx = app.workers_state.selected().unwrap_or(0);
                if let Some(w) = snap_view.workers.get(idx) {
                    let client = client.clone();
                    let id = w.id.clone();
                    let for_async = id.clone();
                    let result = rt.block_on(async move { client.unregister(&for_async).await });
                    app.notice = Some(match result {
                        Ok(()) => format!("worker `{id}` removed"),
                        Err(e) => format!("error: {e}"),
                    });
                }
            }
            KeyCode::Enter if app.mode == Mode::Jobs => app.mode = Mode::JobDetail,
            KeyCode::Char('w') if app.mode == Mode::JobDetail => {
                if let Some(job) = snap_view.jobs.get(app.selected) {
                    let candidates: Vec<String> = snap_view
                        .workers
                        .iter()
                        .filter(|w| w.status == "ONLINE")
                        .map(|w| w.id.clone())
                        .collect();
                    if candidates.is_empty() {
                        app.notice = Some("no ONLINE workers to choose from".to_string());
                    } else {
                        app.input = Input::WorkerSelect {
                            job: job.name.clone(),
                            candidates,
                        };
                        app.input_buf = "0".to_string();
                    }
                }
            }
            KeyCode::Esc if app.mode == Mode::JobDetail => app.mode = Mode::Jobs,
            KeyCode::Char('r') => {
                if let Some(job) = snap_view.jobs.get(app.selected) {
                    let client = client.clone();
                    let name = job.name.clone();
                    let for_async = name.clone();
                    let result = rt.block_on(async move { client.trigger_run(&for_async).await });
                    app.notice = Some(match result {
                        Ok(id) => format!("run triggered for `{name}`: {}", &id[..id.len().min(8)]),
                        Err(e) => format!("error: {e}"),
                    });
                }
            }
            KeyCode::Char('s') => {
                if let Some(job) = snap_view.jobs.get(app.selected) {
                    let client = client.clone();
                    let name = job.name.clone();
                    let for_async = name.clone();
                    let result = rt.block_on(async move { client.stop_run(&for_async).await });
                    app.notice = Some(match result {
                        Ok(()) => format!("stop requested for `{name}`"),
                        Err(e) => format!("error: {e}"),
                    });
                }
            }
            KeyCode::Char('/') if app.mode == Mode::Jobs => {
                app.input = Input::Search;
                app.input_buf = app.search.clone();
            }
            KeyCode::Char('D') if app.mode == Mode::Jobs || app.mode == Mode::JobDetail => {
                if let Some(job) = app.selected_job(&snap_view).map(|j| j.name.clone()) {
                    let client = client.clone();
                    let name = job.clone();
                    let result = rt.block_on(async move { client.delete_job(&job).await });
                    app.notice = Some(match result {
                        Ok(()) => {
                            app.mode = Mode::Jobs;
                            format!("deleted job `{name}` (config + database)")
                        }
                        Err(e) => format!("delete `{name}` failed: {e}"),
                    });
                    // Force the next tick to refresh jobs; the deleted name
                    // must not stay selected.
                    if app.selected > 0 {
                        app.selected -= 1;
                        app.jobs_state.select(Some(app.selected));
                    }
                }
            }
            KeyCode::Char('n') if app.mode == Mode::Jobs => {
                app.input = Input::JobName;
                app.input_buf.clear();
            }
            KeyCode::Char('a') if app.mode == Mode::Proxies => {
                app.input = Input::ProxyName;
                app.input_buf.clear();
            }
            KeyCode::Char('w') if app.mode == Mode::Proxies => {
                let client = client.clone();
                let cfg = app.cfg_path.clone();
                let result = rt.block_on(async move { register_cf_warp(&client, &cfg).await });
                app.notice = Some(match result {
                    Ok(m) => m,
                    Err(e) => format!("error: {e}"),
                });
            }
            _ => {}
        }
    };
    ratatui::restore();
    result
}

/// Resolve manager URL + token from flags or config's api section
/// (local SQLite path is not used — the TUI reads through the API).
/// Also returns the config path that resolved, for config writes.
fn resolve_creds(
    config: Option<PathBuf>,
    manager: Option<String>,
    token: Option<String>,
) -> Result<(PathBuf, String, String), String> {
    // The TUI reads through the manager API; config supplies defaults.
    let path = find_config(config)?;
    let cfg = ConfigLoader::load(&path, &CliOverrides::default()).ok();
    let url = match manager {
        Some(u) => u,
        None => format!(
            "http://{}",
            cfg.as_ref()
                .map(|c| c.api.listen.to_string())
                .unwrap_or_else(|| "127.0.0.1:8100".to_string())
        ),
    };
    let token = match token {
        Some(t) => t,
        None => cfg
            .and_then(|c| c.api.tokens.first().map(|t| t.token.clone()))
            .ok_or("no api token configured (set [api.tokens] or pass --token)")?,
    };
    Ok((path, url, token))
}

fn find_config(explicit: Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(p) = explicit {
        if p.exists() {
            return Ok(p);
        }
        return Err(format!("config file not found: {}", p.display()));
    }
    for candidate in [
        "synora.toml",
        "config/synora.toml",
        "/etc/synora/synora.toml",
    ] {
        let p = PathBuf::from(candidate);
        if p.exists() {
            return Ok(p);
        }
    }
    Err("no config file found (use -c PATH)".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_section_add_and_replace() {
        let dir = std::env::temp_dir().join(format!("synora-tui-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("synora.toml");
        std::fs::write(&path, "[daemon]\nlog_dir = \"/tmp\"\n").unwrap();

        // Add a new section.
        upsert_proxy_section(
            &path,
            "cf-warp",
            "socks5h",
            "socks5h://127.0.0.1:40000",
            None,
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[proxy.cf-warp]"));
        assert!(text.contains("socks5h://127.0.0.1:40000"));
        assert!(text.contains("expose = \"0.0.0.0:14000\""));
        assert!(!text.contains("expose_auth"));

        // Replace it (re-register with a different port + expose).
        upsert_proxy_section(
            &path,
            "cf-warp",
            "socks5h",
            "socks5h://127.0.0.1:40001",
            Some("0.0.0.0:4000"),
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("socks5h://127.0.0.1:40001"));
        assert!(text.contains("expose = \"0.0.0.0:4000\""));
        assert!(!text.contains("40000"));
        assert_eq!(text.matches("[proxy.cf-warp]").count(), 1);
        // Other sections survive.
        assert!(text.contains("[daemon]"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn proxy_section_upsert_is_idempotent() {
        let dir = std::env::temp_dir().join(format!("synora-tui-idem-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("synora.toml");
        let seed = "[proxy.a]\ntype = \"socks5h\"\nurl = \"u1\"\n\n[proxy.b]\ntype = \"http\"\nurl = \"u2\"\n";
        std::fs::write(&path, seed).unwrap();

        // Existing section: two consecutive upserts must leave the file
        // unchanged (each TUI open re-registers cf-warp).
        upsert_proxy_section(&path, "a", "socks5h", "u1", None).unwrap();
        let once = std::fs::read_to_string(&path).unwrap();
        upsert_proxy_section(&path, "a", "socks5h", "u1", None).unwrap();
        let twice = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            once, twice,
            "replace must be idempotent:\n{once}\nvs\n{twice}"
        );

        // New section: one call adds it with exactly one blank-line
        // separator; a second call (which takes the replace path) must
        // leave the file unchanged.
        upsert_proxy_section(&path, "c", "http", "u3", None).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("\n\n[proxy.c]\n"),
            "new section needs one blank line separator: {text}"
        );
        assert_eq!(text.matches("[proxy.c]").count(), 1);
        upsert_proxy_section(&path, "c", "http", "u3", None).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            text,
            "add-then-replace must be idempotent"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod block_tests {
    use super::*;

    #[test]
    fn job_block_replace() {
        let dir = std::env::temp_dir().join(format!("synora-block-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("jobs.toml");
        std::fs::write(
            &path,
            "[[jobs]]\nname = \"a\"\nschedule = \"manual\"\nprovider = \"rsync\"\nupstream = \"u\"\nstorage = \"/s\"\n\n[[jobs]]\nname = \"b\"\nschedule = \"manual\"\nprovider = \"rsync\"\nupstream = \"u2\"\nstorage = \"/s2\"\n",
        )
        .unwrap();
        let json = serde_json::json!({
            "name": "a",
            "schedule": "interval",
            "every": "6h",
            "provider": "rsync",
            "upstream": "u",
            "storage": "/s",
            "enabled": true
        });
        upsert_job_block(&path, "a", &json).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("every = \"6h\""), "text: {text}");
        assert!(text.contains("name = \"b\""), "job b must survive: {text}");
        assert!(
            !text.contains("name = \"a\"\nschedule = \"manual\""),
            "old block must be gone"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn spec_editor_shows_rsync_options() {
        let spec = serde_json::json!({
            "name": "GXDE",
            "provider": {
                "type": "rsync",
                "options": ["--password-file=/etc/synora/syncpassword/gxde", "--delete-excluded", "--timeout=1800"],
                "exclude": []
            },
            "schedule": {"kind": {"kind": "interval", "every": [14400, 0]}},
            "enabled": true
        });
        let editor = spec_to_editor(&spec);
        assert_eq!(editor["provider"], "rsync");
        let fields = flatten_spec(&editor);
        let map: std::collections::HashMap<_, _> = fields.into_iter().collect();
        assert!(map["options"].contains("--timeout=1800"), "{map:?}");
        assert!(map["options"].contains("--delete-excluded"), "{map:?}");
        assert_eq!(map["schedule"], "interval");
        assert_eq!(map["every"], "4h");
    }
}
