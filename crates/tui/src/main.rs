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
    history: Vec<api::RunDTO>,
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
}

/// Step-by-step input for adding a proxy (name → url → optional expose).
enum Input {
    None,
    ProxyName,
    ProxyUrl { name: String },
    ProxyExpose { name: String, url: String },
}

struct App {
    mode: Mode,
    selected: usize,
    jobs_state: TableState,
    workers_state: TableState,
    input: Input,
    input_buf: String,
    /// Config file proxy registration writes to.
    cfg_path: Option<PathBuf>,
    /// One-shot notice line (operation results).
    notice: Option<String>,
}

impl App {
    fn new(cfg_path: Option<PathBuf>) -> App {
        let mut jobs_state = TableState::default();
        jobs_state.select(Some(0));
        App {
            mode: Mode::Jobs,
            selected: 0,
            jobs_state,
            workers_state: TableState::default(),
            input: Input::None,
            input_buf: String::new(),
            cfg_path,
            notice: None,
        }
    }

    fn selected_job<'a>(&self, snap: &'a Snapshot) -> Option<&'a api::JobDTO> {
        snap.jobs.get(self.selected)
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

fn status_color(status: &str) -> Color {
    match status.to_ascii_lowercase().as_str() {
        "success" => Color::Green,
        "failed" | "lost" => Color::Red,
        "running" | "starting" | "retrying" => Color::Yellow,
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
    let rows: Vec<Row> = snap
        .jobs
        .iter()
        .map(|j| {
            Row::new(vec![
                j.name.clone(),
                j.status.clone(),
                fmt_size(j.size_bytes),
                fmt_ts(j.next_run),
                j.worker.clone().unwrap_or_else(|| "auto".to_string()),
            ])
            .style(Style::default().fg(status_color(&j.status)))
        })
        .collect();
    let table = Table::new(rows, [Constraint::Percentage(25), Constraint::Percentage(15), Constraint::Percentage(15), Constraint::Percentage(25), Constraint::Percentage(20)])
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(" Jobs (F1) "))
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
            format!(" {} — {} ", job.name, job.status),
            Style::default().fg(status_color(&job.status)).add_modifier(Modifier::BOLD),
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
    let header = Row::new(vec!["RUN ID", "STATUS", "DURATION", "EXIT", "BYTES", "MESSAGE"])
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
                r.status.clone(),
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
    let chunks = Layout::vertical([Constraint::Length(lines.len() as u16 + 1), Constraint::Min(6)])
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
    let table = Table::new(rows, [Constraint::Percentage(20), Constraint::Percentage(20), Constraint::Percentage(15), Constraint::Percentage(10), Constraint::Percentage(35)])
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(" Workers (F2) "))
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    f.render_stateful_widget(table, f.area(), &mut app.workers_state);
}

fn render_proxies(f: &mut Frame, _app: &App, snap: &Snapshot) {
    let header = Row::new(vec!["NAME", "TYPE", "LATENCY", "EGRESS IP", "HEALTH", "EXPOSE"])
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
            .style(Style::default().fg(if healthy { Color::Green } else { Color::Red }))
        })
        .collect();
    let table = Table::new(rows, [Constraint::Percentage(16), Constraint::Percentage(10), Constraint::Percentage(12), Constraint::Percentage(18), Constraint::Percentage(10), Constraint::Percentage(34)])
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(" Proxies (F3) — a add, w register CF One/WARP "));
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

/// The input overlay (add-proxy form).
fn render_input(f: &mut Frame, app: &App) {
    let (label, done) = match &app.input {
        Input::None => return,
        Input::ProxyName => ("proxy name (e.g. cf-warp): ", ""),
        Input::ProxyUrl { .. } => ("proxy url (http://… or socks5h://…): ", ""),
        Input::ProxyExpose { .. } => ("expose address (optional, e.g. 0.0.0.0:4000): ", "Enter = done"),
    };
    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).split(f.area());
    let text = format!("{label}{}\n{done}   Esc cancel, Enter confirm", app.input_buf);
    f.render_widget(
        Paragraph::new(text).block(Block::default().borders(Borders::ALL).title(" Add proxy ")),
        chunks[1],
    );
}

fn render_notice(f: &mut Frame, app: &App) {
    if let Some(n) = &app.notice {
        let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(f.area());
        f.render_widget(
            Paragraph::new(n.clone())
                .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            chunks[0],
        );
    }
}

fn render_footer(f: &mut Frame, snap: &Snapshot) {
    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(f.area());
    let hint = "F1 Jobs  F2 Workers  F3 Proxies  F5 Logs  Enter detail  r run  s stop  a add-proxy  w CF-One  q/F10 quit";
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
    }
    render_input(f, app);
    render_notice(f, app);
    render_footer(f, snap);
}

/// Common local proxy ports (CF One / WARP and typical local proxies).
const WARP_PORTS: &[u16] = &[40000, 40001, 1080, 10808, 7890, 7891, 2080, 8899];

fn detect_local_socks() -> Vec<u16> {
    let mut found = Vec::new();
    for port in WARP_PORTS {
        if std::net::TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], *port)),
            Duration::from_millis(300),
        )
        .is_ok()
        {
            found.push(*port);
        }
    }
    found
}

/// Append/refresh a `[proxy.<name>]` table in the config file.
fn upsert_proxy_section(path: &PathBuf, name: &str, kind: &str, url: &str, expose: Option<&str>) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let marker = format!("\n[proxy.{name}]\n");
    let section = format!(
        "{marker}type = \"{kind}\"\nurl = \"{url}\"{}\n",
        expose
            .map(|e| format!("\nexpose = \"{e}\""))
            .unwrap_or_default()
    );
    let new_text = if let Some(start) = text.find(&format!("[proxy.{name}]")) {
        // Replace the existing section: from its header to the next
        // `[section]` header or EOF.
        let end = text[start..]
            .find("\n[")
            .map(|i| start + i + 1)
            .unwrap_or(text.len());
        format!("{}{}{}", &text[..start], section.trim_end_matches('\n'), &text[end..])
    } else {
        format!("{text}{section}")
    };
    std::fs::write(path, new_text).map_err(|e| format!("write {}: {e}", path.display()))
}

/// Register CF One / WARP: detect the local proxy port and add it to config.
async fn register_cf_warp(client: &Client, cfg_path: &Option<PathBuf>) -> Result<String, String> {
    let ports = detect_local_socks();
    if ports.is_empty() {
        return Err("no local proxy port found (checked 40000/1080/7890/…); is CF One/WARP running?".into());
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
    Ok(format!("registered cf-warp at socks5h://127.0.0.1:{port} (reload applied {applied} job(s))"))
}

fn main() -> Result<(), String> {
    let cli = Cli::parse();
    let cfg_path = cli.config.clone();
    let (url, token) = resolve_creds(cli.config, cli.manager, cli.token)?;
    let client = Client::new(&url, &token).map_err(|e| e.to_string())?;

    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    let snap = Arc::new(Mutex::new(Snapshot::default()));

    // Background refresh every 2s.
    {
        let snap = snap.clone();
        let client = client.clone();
        rt.spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(2));
            loop {
                tick.tick().await;
                let mut s = snap.lock().await;
                fetch(&client, &mut s).await;
            }
        });
    }

    let mut terminal = ratatui::init();
    let mut app = App::new(cfg_path);
    let result = loop {
        // Selected-job data (logs + history) is fetched here, not in the
        // background loop — it depends on the current selection.
        {
            let mut s = snap.blocking_lock();
            let log_job = s.jobs.get(app.selected).map(|j| j.name.clone());
            if let Some(name) = &log_job {
                if s.log_job.as_deref() != Some(name.as_str()) || s.log_lines.is_empty() {
                    s.log_job = Some(name.clone());
                    let fetched = rt.block_on(client.job_logs(name, 200));
                    if let Ok(log) = fetched {
                        s.log_lines = log.lines().map(|l| l.to_string()).collect();
                    }
                }
            }
            if app.mode == Mode::JobDetail {
                if let Some(job) = s.jobs.get(app.selected) {
                    let fetched = rt.block_on(client.job_history(&job.name));
                    if let Ok(h) = fetched {
                        s.history = h;
                    }
                }
            }
        }
        let snap = snap.blocking_lock().clone();
        let _ = terminal.draw(|f| render(f, &mut app, &snap));
        if !event::poll(Duration::from_millis(250)).map_err(|e| e.to_string())? {
            continue;
        }
        let ev = event::read().map_err(|e| e.to_string())?;
        let Event::Key(key) = ev else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        // Input overlay first: characters fill the buffer, Enter advances.
        match &mut app.input {
            Input::ProxyName | Input::ProxyUrl { .. } | Input::ProxyExpose { .. } => {
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
                            Input::ProxyName => {
                                app.input = Input::ProxyUrl { name: buf };
                            }
                            Input::ProxyUrl { name } => {
                                app.input = Input::ProxyExpose { name, url: buf };
                            }
                            Input::ProxyExpose { name, url } => {
                                // Commit: write config + reload.
                                let kind = if url.starts_with("socks5h://") { "socks5h" } else { "http" };
                                let expose = if buf.trim().is_empty() { None } else { Some(buf.trim().to_string()) };
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
                            Input::None => {}
                        }
                    }
                    KeyCode::Char(c) => app.input_buf.push(c),
                    _ => {}
                }
                continue;
            }
            Input::None => {}
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::F(10) => break Ok(()),
            KeyCode::F(1) => app.mode = Mode::Jobs,
            KeyCode::F(2) => app.mode = Mode::Workers,
            KeyCode::F(3) => app.mode = Mode::Proxies,
            KeyCode::F(5) => app.mode = Mode::Logs,
            KeyCode::Up => {
                app.selected = app.selected.saturating_sub(1);
                app.jobs_state.select(Some(app.selected));
            }
            KeyCode::Down => {
                app.selected += 1;
                app.jobs_state.select(Some(app.selected));
            }
            KeyCode::Enter if app.mode == Mode::Jobs => app.mode = Mode::JobDetail,
            KeyCode::Esc if app.mode == Mode::JobDetail => app.mode = Mode::Jobs,
            KeyCode::Char('r') => {
                if let Some(job) = snap.jobs.get(app.selected) {
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
                if let Some(job) = snap.jobs.get(app.selected) {
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
fn resolve_creds(
    config: Option<PathBuf>,
    manager: Option<String>,
    token: Option<String>,
) -> Result<(String, String), String> {
    // The TUI reads through the manager API; config supplies defaults.
    let cfg = find_config(config)
        .ok()
        .and_then(|p| ConfigLoader::load(&p, &CliOverrides::default()).ok());
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
    Ok((url, token))
}

fn find_config(explicit: Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(p) = explicit {
        if p.exists() {
            return Ok(p);
        }
        return Err(format!("config file not found: {}", p.display()));
    }
    for candidate in ["synora.toml", "config/synora.toml"] {
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
        upsert_proxy_section(&path, "cf-warp", "socks5h", "socks5h://127.0.0.1:40000", None).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[proxy.cf-warp]"));
        assert!(text.contains("socks5h://127.0.0.1:40000"));

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
}
