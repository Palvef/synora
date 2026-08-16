//! `synora-tui` — terminal console (spec §39–§41): jobs table with status /
//! size / next run, worker panel, log viewer. Data comes from the manager
//! API (or falls back to the local SQLite DB for standalone setups).

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
    error: Option<String>,
}

enum Mode {
    Jobs,
    Workers,
    Logs,
}

struct App {
    mode: Mode,
    selected: usize,
    jobs_state: TableState,
    workers_state: TableState,
}

impl App {
    fn new() -> App {
        let mut jobs_state = TableState::default();
        jobs_state.select(Some(0));
        App {
            mode: Mode::Jobs,
            selected: 0,
            jobs_state,
            workers_state: TableState::default(),
        }
    }

    fn selected_job<'a>(&self, snap: &'a Snapshot) -> Option<&'a api::JobDTO> {
        snap.jobs.get(self.selected)
    }
}

async fn fetch(client: &Client) -> Snapshot {
    let mut snap = Snapshot::default();
    match (client.list_jobs().await, client.list_workers().await) {
        (Ok(jobs), Ok(workers)) => {
            snap.jobs = jobs;
            snap.workers = workers;
        }
        (Err(e), _) | (_, Err(e)) => snap.error = Some(e.to_string()),
    }
    if let Some(job) = snap.jobs.first() {
        if let Ok(log) = client.job_logs(&job.name, 30).await {
            snap.log_lines = log.lines().map(|l| l.to_string()).collect();
        }
    }
    snap
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

fn render_logs(f: &mut Frame, app: &App, snap: &Snapshot) {
    let job = app.selected_job(snap);
    let title = job
        .map(|j| format!(" Logs: {} (F5) ", j.name))
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

fn render_footer(f: &mut Frame, snap: &Snapshot) {
    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(f.area());
    f.render_widget(
        Paragraph::new("F1 Jobs  F2 Workers  F5 Logs  ↑/↓ select  q/F10 quit")
            .style(Style::default().fg(Color::DarkGray)),
        chunks[1],
    );
    if let Some(err) = &snap.error {
        f.render_widget(
            Paragraph::new(Line::from(Span::raw(format!("⚠ {err}"))))
                .style(Style::default().fg(Color::Red)),
            chunks[0],
        );
    }
}

fn render(f: &mut Frame, app: &mut App, snap: &Snapshot) {
    match app.mode {
        Mode::Jobs => render_jobs(f, app, snap),
        Mode::Workers => render_workers(f, app, snap),
        Mode::Logs => render_logs(f, app, snap),
    }
    render_footer(f, snap);
}

fn main() -> Result<(), String> {
    let cli = Cli::parse();
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
                let s = fetch(&client).await;
                *snap.lock().await = s;
            }
        });
    }

    let mut terminal = ratatui::init();
    let mut app = App::new();
    let result = loop {
        let snap = snap.blocking_lock().clone();
        let _ = terminal.draw(|f| render(f, &mut app, &snap));
        if !event::poll(Duration::from_millis(250)).map_err(|e| e.to_string())? {
            continue;
        }
        match event::read().map_err(|e| e.to_string())? {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Char('q') | KeyCode::F(10) => break Ok(()),
                KeyCode::F(1) => app.mode = Mode::Jobs,
                KeyCode::F(2) => app.mode = Mode::Workers,
                KeyCode::F(5) => app.mode = Mode::Logs,
                KeyCode::Up => {
                    app.selected = app.selected.saturating_sub(1);
                    app.jobs_state.select(Some(app.selected));
                }
                KeyCode::Down => {
                    app.selected += 1;
                    app.jobs_state.select(Some(app.selected));
                }
                _ => {}
            },
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
