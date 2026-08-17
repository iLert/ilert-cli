use std::io;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::prelude::*;
use ratatui::widgets::*;
use serde_json::Value;

use crate::http::HttpClient;

const REFRESH_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq)]
enum Tab {
    Alerts,
    Incidents,
    Services,
}

impl Tab {
    fn all() -> &'static [Tab] {
        &[Tab::Alerts, Tab::Incidents, Tab::Services]
    }

    fn title(&self) -> &'static str {
        match self {
            Tab::Alerts => "Alerts",
            Tab::Incidents => "Incidents",
            Tab::Services => "Services",
        }
    }

    fn next(&self) -> Tab {
        match self {
            Tab::Alerts => Tab::Incidents,
            Tab::Incidents => Tab::Services,
            Tab::Services => Tab::Alerts,
        }
    }

    fn prev(&self) -> Tab {
        match self {
            Tab::Alerts => Tab::Services,
            Tab::Incidents => Tab::Alerts,
            Tab::Services => Tab::Incidents,
        }
    }
}

struct DashboardState {
    tab: Tab,
    alerts: Vec<Value>,
    incidents: Vec<Value>,
    services: Vec<Value>,
    last_error: Option<String>,
    scroll_offset: usize,
}

pub async fn run_dashboard(client: &HttpClient) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut state = DashboardState {
        tab: Tab::Alerts,
        alerts: Vec::new(),
        incidents: Vec::new(),
        services: Vec::new(),
        last_error: None,
        scroll_offset: 0,
    };

    // Initial fetch
    refresh_data(client, &mut state).await;

    let mut last_refresh = std::time::Instant::now();

    loop {
        terminal.draw(|f| draw_ui(f, &state))?;

        // Poll for events with a short timeout
        if event::poll(Duration::from_millis(250))?
            && let Event::Key(key) = event::read()?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => break,
                KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                    state.tab = state.tab.next();
                    state.scroll_offset = 0;
                }
                KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                    state.tab = state.tab.prev();
                    state.scroll_offset = 0;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    state.scroll_offset = state.scroll_offset.saturating_add(1);
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    state.scroll_offset = state.scroll_offset.saturating_sub(1);
                }
                KeyCode::Char('r') => {
                    refresh_data(client, &mut state).await;
                    last_refresh = std::time::Instant::now();
                }
                _ => {}
            }
        }

        // Auto-refresh
        if last_refresh.elapsed() >= REFRESH_INTERVAL {
            refresh_data(client, &mut state).await;
            last_refresh = std::time::Instant::now();
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    Ok(())
}

async fn refresh_data(client: &HttpClient, state: &mut DashboardState) {
    let alert_q: Vec<(String, String)> = vec![
        ("states".into(), "PENDING".into()),
        ("states".into(), "ACCEPTED".into()),
        ("max-results".into(), "50".into()),
    ];
    let incident_q: Vec<(String, String)> = vec![
        ("states".into(), "INVESTIGATING".into()),
        ("states".into(), "IDENTIFIED".into()),
        ("states".into(), "MONITORING".into()),
    ];
    let (alerts_res, incidents_res, services_res) = tokio::join!(
        client.request(reqwest::Method::GET, "/api/alerts", &alert_q, &[], None),
        client.request(
            reqwest::Method::GET,
            "/api/incidents",
            &incident_q,
            &[],
            None
        ),
        client.request(reqwest::Method::GET, "/api/services", &[], &[], None),
    );

    state.last_error = None;

    // The message half of these comes off the wire, and it lands in the status
    // bar unquoted.
    let failure = |label: &str, e: anyhow::Error| {
        Some(crate::sanitize::terminal_string(format!("{label}: {e}")))
    };

    match alerts_res {
        Ok((_, body)) => state.alerts = extract_items(body.value()),
        Err(e) => state.last_error = failure("Alerts", e),
    }
    match incidents_res {
        Ok((_, body)) => state.incidents = extract_items(body.value()),
        Err(e) => state.last_error = failure("Incidents", e),
    }
    match services_res {
        Ok((_, body)) => state.services = extract_items(body.value()),
        Err(e) => state.last_error = failure("Services", e),
    }
}

fn extract_items(value: &Value) -> Vec<Value> {
    if let Some(arr) = value.as_array() {
        return arr.clone();
    }
    if let Some(obj) = value.as_object() {
        for v in obj.values() {
            if let Some(arr) = v.as_array() {
                return arr.clone();
            }
        }
    }
    Vec::new()
}

fn draw_ui(f: &mut Frame, state: &DashboardState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // tabs
            Constraint::Min(5),    // content
            Constraint::Length(1), // status bar
        ])
        .split(f.area());

    // Tab bar
    let titles: Vec<Line> = Tab::all()
        .iter()
        .map(|t| {
            let count = match t {
                Tab::Alerts => state.alerts.len(),
                Tab::Incidents => state.incidents.len(),
                Tab::Services => state.services.len(),
            };
            Line::from(format!(" {} ({}) ", t.title(), count))
        })
        .collect();

    let tabs = Tabs::new(titles)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" ilert Dashboard "),
        )
        .select(Tab::all().iter().position(|t| *t == state.tab).unwrap_or(0))
        .style(Style::default().fg(Color::White))
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .bold()
                .add_modifier(Modifier::UNDERLINED),
        );

    f.render_widget(tabs, chunks[0]);

    // Content
    match state.tab {
        Tab::Alerts => draw_alerts(f, chunks[1], state),
        Tab::Incidents => draw_incidents(f, chunks[1], state),
        Tab::Services => draw_services(f, chunks[1], state),
    }

    // Status bar
    let status_text = if let Some(ref err) = state.last_error {
        format!(" Error: {err} | q:quit  Tab:switch  r:refresh  j/k:scroll")
    } else {
        format!(
            " {} | q:quit  Tab:switch  r:refresh  j/k:scroll  Auto-refresh: {}s",
            chrono::Local::now().format("%H:%M:%S"),
            REFRESH_INTERVAL.as_secs()
        )
    };
    let status =
        Paragraph::new(status_text).style(Style::default().bg(Color::DarkGray).fg(Color::White));
    f.render_widget(status, chunks[2]);
}

fn draw_alerts(f: &mut Frame, area: Rect, state: &DashboardState) {
    if state.alerts.is_empty() {
        let msg = Paragraph::new("  No open alerts")
            .style(Style::default().fg(Color::Green))
            .block(Block::default().borders(Borders::ALL).title(" Alerts "));
        f.render_widget(msg, area);
        return;
    }

    let header = Row::new(vec!["ID", "Summary", "Status", "Priority", "Source"])
        .style(Style::default().bold())
        .bottom_margin(1);

    let rows: Vec<Row> = state
        .alerts
        .iter()
        .skip(state.scroll_offset)
        .map(|a| {
            let id = field_str(a, "id");
            let summary = truncate_str(&field_str(a, "summary"), 40);
            let status = field_str(a, "status");
            let priority = field_str(a, "priority");
            let source = a
                .get("alertSource")
                .map(|s| field_str(s, "name"))
                .unwrap_or_default();

            let style = match status.as_str() {
                "PENDING" => Style::default().fg(Color::Yellow),
                "ACCEPTED" => Style::default().fg(Color::Cyan),
                _ => Style::default(),
            };

            Row::new(vec![id, summary, status, priority, source]).style(style)
        })
        .collect();

    let widths = [
        Constraint::Length(8),
        Constraint::Percentage(40),
        Constraint::Length(12),
        Constraint::Length(10),
        Constraint::Percentage(25),
    ];

    let table = Table::new(rows, widths).header(header).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" Alerts ({}) ", state.alerts.len())),
    );

    f.render_widget(table, area);
}

fn draw_incidents(f: &mut Frame, area: Rect, state: &DashboardState) {
    if state.incidents.is_empty() {
        let msg = Paragraph::new("  No active incidents")
            .style(Style::default().fg(Color::Green))
            .block(Block::default().borders(Borders::ALL).title(" Incidents "));
        f.render_widget(msg, area);
        return;
    }

    let header = Row::new(vec!["ID", "Summary", "Status", "Created"])
        .style(Style::default().bold())
        .bottom_margin(1);

    let rows: Vec<Row> = state
        .incidents
        .iter()
        .skip(state.scroll_offset)
        .map(|inc| {
            let id = field_str(inc, "id");
            let summary = truncate_str(&field_str(inc, "summary"), 50);
            let status = field_str(inc, "status");
            let created = field_str(inc, "createdAt");

            let style = match status.as_str() {
                "INVESTIGATING" => Style::default().fg(Color::Yellow),
                "IDENTIFIED" => Style::default().fg(Color::Cyan),
                "MONITORING" => Style::default().fg(Color::Blue),
                _ => Style::default(),
            };

            Row::new(vec![id, summary, status, created]).style(style)
        })
        .collect();

    let widths = [
        Constraint::Length(8),
        Constraint::Percentage(50),
        Constraint::Length(16),
        Constraint::Length(20),
    ];

    let table = Table::new(rows, widths).header(header).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" Incidents ({}) ", state.incidents.len())),
    );

    f.render_widget(table, area);
}

fn draw_services(f: &mut Frame, area: Rect, state: &DashboardState) {
    if state.services.is_empty() {
        let msg = Paragraph::new("  No services configured")
            .style(Style::default().fg(Color::DarkGray))
            .block(Block::default().borders(Borders::ALL).title(" Services "));
        f.render_widget(msg, area);
        return;
    }

    let header = Row::new(vec!["ID", "Name", "Status"])
        .style(Style::default().bold())
        .bottom_margin(1);

    let rows: Vec<Row> = state
        .services
        .iter()
        .skip(state.scroll_offset)
        .map(|svc| {
            let id = field_str(svc, "id");
            let name = field_str(svc, "name");
            let status = field_str(svc, "status");

            let style = match status.as_str() {
                "OPERATIONAL" => Style::default().fg(Color::Green),
                "DEGRADED" | "DEGRADED_PERFORMANCE" => Style::default().fg(Color::Yellow),
                s if s.contains("OUTAGE") => Style::default().fg(Color::Red).bold(),
                "UNDER_MAINTENANCE" => Style::default().fg(Color::Blue),
                _ => Style::default(),
            };

            Row::new(vec![id, name, status]).style(style)
        })
        .collect();

    let widths = [
        Constraint::Length(8),
        Constraint::Percentage(60),
        Constraint::Length(24),
    ];

    let table = Table::new(rows, widths).header(header).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" Services ({}) ", state.services.len())),
    );

    f.render_widget(table, area);
}

/// One field of an API object, ready to put in a cell.
///
/// Escaped here rather than at each call site because every string the
/// dashboard shows arrives this way. ratatui writes cell contents into the
/// terminal as-is, so an alert summary carrying `ESC [ 2J` would clear the
/// screen underneath the frame it is being drawn into, and a bidi override
/// would reorder a service name without changing it.
fn field_str(value: &Value, key: &str) -> String {
    let raw = match value.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => return String::new(),
    };
    crate::sanitize::terminal_string(raw)
}

/// Truncate to `max` characters.
///
/// Counted in `char`s, not bytes: a summary with an umlaut in it made the old
/// byte slice land mid-codepoint and panic, which in raw mode leaves the
/// terminal in the alternate screen with echo off.
fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(3);
    let mut out: String = s.chars().take(keep).collect();
    out.push_str("...");
    out
}
