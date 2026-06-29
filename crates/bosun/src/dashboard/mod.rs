//! Real-time terminal dashboard TUI using ratatui.
//!
//! Shows apps, metrics, security status, gateway info, and backups
//! in a 4-panel interactive layout refreshed every second.

use crate::client::BosunClient;
use crate::proto::bosun::v1::{
    App, AppMetric, BackupInfo, GatewayCacheStats, GatewayRoute, SecurityDecision,
    SecurityStatus,
};
use anyhow::Context;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use crossterm::ExecutableCommand;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Row, Table, TableState};
use ratatui::{Frame, Terminal};
use std::io::stdout;
use std::time::{Duration, Instant};

// ── Dashboard state ────────────────────────────────────────────────

/// Which panel is currently focused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Panel {
    Apps,
    Security,
    Gateway,
    Backups,
}

impl Panel {
    fn next(self) -> Self {
        match self {
            Panel::Apps => Panel::Security,
            Panel::Security => Panel::Gateway,
            Panel::Gateway => Panel::Backups,
            Panel::Backups => Panel::Apps,
        }
    }
}

/// Data fetched from the daemon on each tick.
struct DashboardData {
    apps: Vec<App>,
    metrics: Vec<AppMetric>,
    security_status: Option<SecurityStatus>,
    security_decisions: Vec<SecurityDecision>,
    gateway_status: Option<String>, // "Running" or "Disabled" / error string
    gateway_routes: Vec<GatewayRoute>,
    gateway_cache_stats: Vec<GatewayCacheStats>, // per route
    backups: Vec<BackupInfo>,
    last_refresh: Instant,
}

impl DashboardData {
    fn empty() -> Self {
        Self {
            apps: Vec::new(),
            metrics: Vec::new(),
            security_status: None,
            security_decisions: Vec::new(),
            gateway_status: None,
            gateway_routes: Vec::new(),
            gateway_cache_stats: Vec::new(),
            backups: Vec::new(),
            last_refresh: Instant::now(),
        }
    }
}

/// The main Dashboard TUI struct.
pub struct Dashboard {
    client: BosunClient,
    data: DashboardData,
    /// Selected row in the active panel (for tables).
    app_selected: usize,
    backup_selected: usize,
    /// Currently active panel.
    active_panel: Panel,
    /// Error message to show in status bar.
    error_msg: Option<String>,
}

impl Dashboard {
    pub fn new(client: BosunClient) -> Self {
        Self {
            client,
            data: DashboardData::empty(),
            app_selected: 0,
            backup_selected: 0,
            active_panel: Panel::Apps,
            error_msg: None,
        }
    }

    // ── Public entry point ─────────────────────────────────────────

    /// Enter raw mode, start the event loop, and run until the user quits.
    pub fn run(&mut self) -> anyhow::Result<()> {
        enable_raw_mode().context("Failed to enable raw terminal mode")?;
        stdout()
            .execute(crossterm::terminal::EnterAlternateScreen)
            .context("Failed to enter alternate screen")?;

        let mut terminal = Terminal::new(ratatui::backend::CrosstermBackend::new(stdout()))
            .context("Failed to create terminal")?;

        // Initial data fetch
        self.refresh_data();

        let tick_rate = Duration::from_secs(1);
        let mut last_tick = Instant::now();

        loop {
            // Draw
            terminal
                .draw(|f| self.render(f))
                .context("Failed to render frame")?;

            // Wait for input with timeout
            let timeout = tick_rate
                .checked_sub(last_tick.elapsed())
                .unwrap_or(Duration::ZERO);

            if event::poll(timeout).context("Failed to poll events")? {
                if let Event::Key(key) = event::read().context("Failed to read event")? {
                    // Only process key press events (not release/repeat)
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    if !self.handle_key(key.code) {
                        break; // quit
                    }
                }
            }

            // Tick: refresh data every second
            if last_tick.elapsed() >= tick_rate {
                self.refresh_data();
                last_tick = Instant::now();
            }
        }

        // Cleanup
        disable_raw_mode().context("Failed to disable raw mode")?;
        stdout()
            .execute(crossterm::terminal::LeaveAlternateScreen)
            .context("Failed to leave alternate screen")?;

        Ok(())
    }

    // ── Data fetching ──────────────────────────────────────────────

    fn refresh_data(&mut self) {
        self.error_msg = None;

        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                self.error_msg = Some(format!("Failed to create runtime: {e}"));
                return;
            }
        };

        // Fetch all data concurrently (non-blocking)
        let data = rt.block_on(async { self.fetch_all().await });
        self.data = data;
    }

    async fn fetch_all(&mut self) -> DashboardData {
        use std::sync::Arc;
        use tokio::sync::Mutex;

        let client = Arc::new(Mutex::new(&mut self.client));
        let mut data = DashboardData::empty();
        data.last_refresh = Instant::now();

        // Apps
        {
            let mut c = client.lock().await;
            match c.list_apps().await {
                Ok(apps) => data.apps = apps,
                Err(e) => {
                    // Keep old apps if the fetch fails
                    data.apps = self.data.apps.clone();
                    self.error_msg = Some(format!("Apps: {e}"));
                }
            }
        }

        // Metrics
        {
            let mut c = client.lock().await;
            match c.get_metrics(None).await {
                Ok(metrics) => data.metrics = metrics,
                Err(e) => {
                    self.error_msg = Some(format!("Metrics: {e}"));
                }
            }
        }

        // Security
        {
            let mut c = client.lock().await;
            match c.get_security_status().await {
                Ok(resp) => data.security_status = resp.status,
                Err(e) => {
                    self.error_msg
                        .get_or_insert_with(|| format!("Security: {e}"));
                }
            }
        }
        {
            let mut c = client.lock().await;
            match c.get_security_decisions().await {
                Ok(resp) => data.security_decisions = resp.decisions,
                Err(e) => {
                    self.error_msg
                        .get_or_insert_with(|| format!("Decisions: {e}"));
                }
            }
        }

        // Gateway
        {
            let mut c = client.lock().await;
            match c.get_gateway_status().await {
                Ok(resp) => {
                    data.gateway_status = resp.status.map(|s| {
                        if s.enabled {
                            format!("{} (uptime: {})", s.version, s.uptime)
                        } else {
                            "Disabled".to_string()
                        }
                    });
                }
                Err(e) => {
                    data.gateway_status = Some(format!("Error: {e}"));
                }
            }
        }
        {
            let mut c = client.lock().await;
            match c.list_gateway_routes().await {
                Ok(resp) => data.gateway_routes = resp.routes,
                Err(_) => {}
            }
        }
        // Fetch cache stats for each route (best-effort)
        {
            let mut c = client.lock().await;
            for route in &data.gateway_routes {
                if let Ok(resp) = c.get_gateway_cache_stats(&route.name).await {
                    if let Some(stats) = resp.stats {
                        data.gateway_cache_stats.push(stats);
                    }
                }
            }
        }

        // Backups
        {
            let mut c = client.lock().await;
            match c.list_backups(None).await {
                Ok(backups) => data.backups = backups,
                Err(e) => {
                    self.error_msg
                        .get_or_insert_with(|| format!("Backups: {e}"));
                }
            }
        }

        data
    }

    // ── Key handling ───────────────────────────────────────────────

    /// Returns false if the user quit.
    fn handle_key(&mut self, code: KeyCode) -> bool {
        match code {
            KeyCode::Char('q') | KeyCode::Esc => return false,
            KeyCode::Tab => {
                self.active_panel = self.active_panel.next();
            }
            KeyCode::Down | KeyCode::Char('j') => self.select_next(),
            KeyCode::Up | KeyCode::Char('k') => self.select_prev(),
            KeyCode::Char('l') => self.handle_logs(),
            KeyCode::Char('r') => self.handle_restart(),
            KeyCode::Char('s') => self.handle_security_details(),
            _ => {}
        }
        true
    }

    fn select_next(&mut self) {
        match self.active_panel {
            Panel::Apps => {
                let max = self.data.apps.len().saturating_sub(1);
                self.app_selected = (self.app_selected + 1).min(max);
            }
            Panel::Backups => {
                let max = self.data.backups.len().saturating_sub(1);
                self.backup_selected = (self.backup_selected + 1).min(max);
            }
            _ => {}
        }
    }

    fn select_prev(&mut self) {
        match self.active_panel {
            Panel::Apps => {
                self.app_selected = self.app_selected.saturating_sub(1);
            }
            Panel::Backups => {
                self.backup_selected = self.backup_selected.saturating_sub(1);
            }
            _ => {}
        }
    }

    fn selected_app_name(&self) -> Option<&str> {
        self.data.apps.get(self.app_selected).map(|a| a.name.as_str())
    }

    fn handle_logs(&mut self) {
        if let Some(name) = self.selected_app_name() {
            // Exit TUI and run logs command
            disable_raw_mode().ok();
            let _ = stdout().execute(crossterm::terminal::LeaveAlternateScreen);

            #[allow(clippy::single_match)]
            match std::process::Command::new(std::env::current_exe().unwrap_or_default())
                .arg("apps")
                .arg("logs")
                .arg(name)
                .arg("--follow")
                .status()
            {
                Ok(_) => {}
                Err(e) => {
                    eprintln!("Failed to spawn logs: {e}");
                }
            }

            // Re-enter TUI
            enable_raw_mode().ok();
            let _ = stdout().execute(crossterm::terminal::EnterAlternateScreen);
        }
    }

    fn handle_restart(&mut self) {
        if let Some(name) = self.selected_app_name() {
            let app_name = name.to_string();
            // We need to restart via client. Spawn an async task.
            // Since we're in a sync render loop, we'll do a quick
            // block_on for the restart.
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    self.error_msg = Some(format!("Restart error: {e}"));
                    return;
                }
            };
            match rt.block_on(self.client.restart_app(&app_name)) {
                Ok(()) => self.error_msg = None,
                Err(e) => self.error_msg = Some(format!("Restart: {e}")),
            }
        }
    }

    fn handle_security_details(&mut self) {
        // Just toggle active panel to Security
        self.active_panel = Panel::Security;
    }

    // ── Rendering ──────────────────────────────────────────────────

    fn render(&mut self, f: &mut Frame) {
        let area = f.area();

        // Vertical split: header, main, footer
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),  // header
                Constraint::Min(0),     // main content
                Constraint::Length(1),  // footer / status bar
            ])
            .split(area);

        self.render_header(f, chunks[0]);
        self.render_main(f, chunks[1]);
        self.render_footer(f, chunks[2]);
    }

    fn render_header(&self, f: &mut Frame, area: Rect) {
        let title = format!(
            " Bosun Dashboard — {} apps deployed — last refresh: {}s ago ",
            self.data.apps.len(),
            self.data.last_refresh.elapsed().as_secs(),
        );
        let header = Paragraph::new(title)
            .style(Style::default().fg(Color::White).bg(Color::DarkGray))
            .centered();
        f.render_widget(header, area);
    }

    fn render_main(&mut self, f: &mut Frame, area: Rect) {
        // 2x2 grid
        let top_bottom = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)])
            .split(area);

        let top = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)])
            .split(top_bottom[0]);

        let bottom = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)])
            .split(top_bottom[1]);

        // Panel 1 (top-left): Apps
        self.render_apps_panel(f, top[0]);
        // Panel 2 (top-right): Security
        self.render_security_panel(f, top[1]);
        // Panel 3 (bottom-left): Gateway
        self.render_gateway_panel(f, bottom[0]);
        // Panel 4 (bottom-right): Backups
        self.render_backups_panel(f, bottom[1]);
    }

    fn panel_border<'a>(&self, panel: Panel, title: &'a str) -> Block<'a> {
        let style = if self.active_panel == panel {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        };
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(style)
    }

    // ── Panel 1: Apps ──────────────────────────────────────────────

    fn render_apps_panel(&mut self, f: &mut Frame, area: Rect) {
        let block = self.panel_border(Panel::Apps, " Apps (l=logs, r=restart) ");

        if self.data.apps.is_empty() {
            let p = Paragraph::new("No apps deployed.\nRun: bosun deploy ./app")
                .block(block)
                .centered();
            f.render_widget(p, area);
            return;
        }

        // Build rows
        let header = Row::new(vec!["NAME", "STATUS", "CPU%", "RAM", "DOMAIN"])
            .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));

        let rows: Vec<Row> = self
            .data
            .apps
            .iter()
            .enumerate()
            .map(|(i, app)| {
                let metric = self.data.metrics.iter().find(|m| m.app_name == app.name);
                let cpu = metric
                    .map(|m| format!("{:.1}%", m.cpu_percent))
                    .unwrap_or_else(|| "-".to_string());
                let ram = metric.map(|m| {
                    let mb = m.ram_bytes as f64 / 1_048_576.0;
                    format!("{:.1} MB", mb)
                }).unwrap_or_else(|| "-".to_string());
                let domain = app.domain.as_deref().unwrap_or("-");

                let status_style = match app.status {
                    1 => Style::default().fg(Color::Green),  // RUNNING
                    2 => Style::default().fg(Color::Red),    // STOPPED
                    3 => Style::default().fg(Color::Yellow), // DEPLOYING
                    4 => Style::default().fg(Color::Red),    // FAILED
                    _ => Style::default(),
                };
                let status_label = match app.status {
                    1 => "Running",
                    2 => "Stopped",
                    3 => "Deploying",
                    4 => "Failed",
                    _ => "Unknown",
                };

                let highlight = i == self.app_selected && self.active_panel == Panel::Apps;
                let row_style = if highlight {
                    Style::default().bg(Color::DarkGray)
                } else {
                    Style::default()
                };

                Row::new(vec![
                    app.name.clone(),
                    Span::styled(status_label.to_string(), status_style).to_string(),
                    cpu,
                    ram,
                    domain.to_string(),
                ])
                .style(row_style)
            })
            .collect();

        let widths = [
            Constraint::Percentage(20),
            Constraint::Percentage(15),
            Constraint::Percentage(10),
            Constraint::Percentage(20),
            Constraint::Percentage(35),
        ];

        let mut table_state = TableState::default().with_selected(Some(self.app_selected));
        let table = Table::new(rows, widths)
            .header(header)
            .block(block)
            .row_highlight_style(Style::default());

        f.render_stateful_widget(table, area, &mut table_state);
    }

    // ── Panel 2: Security ──────────────────────────────────────────

    fn render_security_panel(&self, f: &mut Frame, area: Rect) {
        let block = self.panel_border(Panel::Security, " Security ");

        let mut lines: Vec<Line> = Vec::new();

        match &self.data.security_status {
            Some(status) => {
                lines.push(Line::from(vec![
                    Span::raw("Engine: "),
                    Span::styled(&status.engine, Style::default().fg(Color::Cyan)),
                ]));
                lines.push(Line::from(vec![
                    Span::raw("Attacks blocked: "),
                    Span::styled(
                        format!("{}", status.attacks_blocked),
                        Style::default().fg(Color::Green),
                    ),
                ]));
                lines.push(Line::from(vec![
                    Span::raw("Active bans: "),
                    Span::styled(
                        format!("{}", status.active_bans),
                        Style::default().fg(Color::Yellow),
                    ),
                ]));
                if let Some(last) = self.data.security_decisions.first() {
                    let ts = chrono_human(last.expires_unix);
                    lines.push(Line::from(vec![
                        Span::raw("Last alert: "),
                        Span::styled(
                            format!("{} from {} ({})", last.reason, last.ip, ts),
                            Style::default().fg(Color::Red),
                        ),
                    ]));
                } else {
                    lines.push(Line::from("Last alert: —"));
                }
            }
            None => {
                lines.push(Line::from("Security engine not available."));
                lines.push(Line::from("Install CrowdSec: bosun-daemon --with-crowdsec"));
            }
        }

        let p = Paragraph::new(lines).block(block);
        f.render_widget(p, area);
    }

    // ── Panel 3: Gateway ───────────────────────────────────────────

    fn render_gateway_panel(&self, f: &mut Frame, area: Rect) {
        let block = self.panel_border(Panel::Gateway, " Gateway (APISIX) ");

        let mut lines: Vec<Line> = Vec::new();

        match &self.data.gateway_status {
            Some(status) => {
                lines.push(Line::from(vec![
                    Span::raw("Status: "),
                    if status.starts_with("Error") {
                        Span::styled(status.as_str(), Style::default().fg(Color::Red))
                    } else if status == "Disabled" {
                        Span::styled(status.as_str(), Style::default().fg(Color::Yellow))
                    } else {
                        Span::styled(status.as_str(), Style::default().fg(Color::Green))
                    },
                ]));
            }
            None => {
                lines.push(Line::from("APISIX not connected"));
            }
        }

        let route_count = self.data.gateway_routes.len();
        lines.push(Line::from(format!("Routes: {route_count}")));

        // Aggregate cache stats
        let total_hits: u64 = self.data.gateway_cache_stats.iter().map(|s| s.hits).sum();
        let total_misses: u64 = self.data.gateway_cache_stats.iter().map(|s| s.misses).sum();
        let total_reqs = total_hits + total_misses;
        let hit_rate = if total_reqs > 0 {
            format!("{:.1}%", total_hits as f64 / total_reqs as f64 * 100.0)
        } else {
            "—".to_string()
        };
        lines.push(Line::from(format!("Cache hit rate: {hit_rate}")));

        // Active plugins (deduplicated across routes)
        let mut plugins: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for route in &self.data.gateway_routes {
            for plugin in &route.plugins {
                plugins.insert(plugin.as_str());
            }
        }
        let plugin_list: Vec<&str> = plugins.into_iter().collect();
        lines.push(Line::from(format!(
            "Active plugins: {}",
            if plugin_list.is_empty() {
                "—".to_string()
            } else {
                plugin_list.join(", ")
            }
        )));

        let p = Paragraph::new(lines).block(block);
        f.render_widget(p, area);
    }

    // ── Panel 4: Backups ───────────────────────────────────────────

    fn render_backups_panel(&mut self, f: &mut Frame, area: Rect) {

        if self.data.backups.is_empty() {
            let block = self.panel_border(Panel::Backups, " Backups ");
            let p = Paragraph::new("No backups yet.\nRun: bosun backup create <app>")
                .block(block)
                .centered();
            f.render_widget(p, area);
            return;
        }

        // Show latest backup per app (grouped)
        let mut per_app: std::collections::BTreeMap<String, &BackupInfo> =
            std::collections::BTreeMap::new();
        for backup in &self.data.backups {
            per_app
                .entry(backup.app_name.clone())
                .and_modify(|existing| {
                    if backup.timestamp_unix > existing.timestamp_unix {
                        *existing = backup;
                    }
                })
                .or_insert(backup);
        }

        let header = Row::new(vec!["APP", "SIZE", "AGE"])
            .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));

        let mut backup_list: Vec<&BackupInfo> = per_app.values().copied().collect();
        backup_list.sort_by_key(|b| (b.timestamp_unix as i64).wrapping_neg()); // newest first

        self.backup_selected = self.backup_selected.min(backup_list.len().saturating_sub(1));
        let selected = self.backup_selected;
        let is_active = self.active_panel == Panel::Backups;

        let rows: Vec<Row> = backup_list
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let size_mb = b.size_bytes as f64 / 1_048_576.0;
                let age_secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
                    .saturating_sub(b.timestamp_unix);
                let age = format_age(age_secs);

                let highlight = i == selected && is_active;
                let row_style = if highlight {
                    Style::default().bg(Color::DarkGray)
                } else {
                    Style::default()
                };

                Row::new(vec![
                    b.app_name.clone(),
                    format!("{:.1} MB", size_mb),
                    age,
                ])
                .style(row_style)
            })
            .collect();

        let widths = [
            Constraint::Percentage(40),
            Constraint::Percentage(30),
            Constraint::Percentage(30),
        ];

        let block = self.panel_border(Panel::Backups, " Backups ");

        let mut table_state =
            TableState::default().with_selected(Some(selected));
        let table = Table::new(rows, widths)
            .header(header)
            .block(block)
            .row_highlight_style(Style::default());

        f.render_stateful_widget(table, area, &mut table_state);
    }

    // ── Footer / status bar ────────────────────────────────────────

    fn render_footer(&self, f: &mut Frame, area: Rect) {
        let status_text = if let Some(ref err) = self.error_msg {
            Span::styled(format!(" ⚠ {err} "), Style::default().fg(Color::Yellow))
        } else {
            Span::styled(" OK ", Style::default().fg(Color::Green))
        };

        let help = format!(
            " q:quit  Tab:switch  ↑↓:select  l:logs  r:restart  s:security  Panel: {:?} ",
            self.active_panel
        );

        let line = Line::from(vec![
            status_text,
            Span::raw(" │ "),
            Span::styled(help, Style::default().fg(Color::DarkGray)),
        ]);

        let footer = Paragraph::new(line)
            .style(Style::default().bg(Color::DarkGray))
            .centered();
        f.render_widget(footer, area);
    }
}

// ── Helpers ───────────────────────────────────────────────────────

/// Convert a unix timestamp to a human-readable string ("14d 3h" style).
fn chrono_human(ts: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let age = now.saturating_sub(ts);
    format_age(age)
}

fn format_age(seconds: u64) -> String {
    let days = seconds / 86400;
    let hours = (seconds % 86400) / 3600;
    let mins = (seconds % 3600) / 60;

    if days > 0 {
        format!("{}d {}h", days, hours)
    } else if hours > 0 {
        format!("{}h {}m", hours, mins)
    } else if mins > 0 {
        format!("{}m", mins)
    } else {
        format!("{}s", seconds)
    }
}
