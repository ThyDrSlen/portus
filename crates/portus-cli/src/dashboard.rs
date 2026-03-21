use std::io::{self, IsTerminal};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use crossterm::event::{self, Event, KeyCode};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use portus_core::model::Lease;
use portus_core::paths;
use portus_core::scan::{scan_ports, PortProcess};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap};
use ratatui::Terminal;

pub fn run_dashboard() -> Result<()> {
    if !io::stdout().is_terminal() {
        bail!("dashboard requires an interactive terminal");
    }

    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("failed to enter alternate screen")?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("failed to create terminal")?;

    let mut snapshot = DashboardSnapshot::load();
    loop {
        terminal.draw(|frame| draw_dashboard(frame, &snapshot))?;

        if event::poll(Duration::from_secs(1)).context("failed to poll terminal events")? {
            if let Event::Key(key) = event::read().context("failed to read terminal event")? {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('r') => snapshot = DashboardSnapshot::load(),
                    _ => {}
                }
            }
        } else {
            snapshot = DashboardSnapshot::load();
        }
    }

    Ok(())
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen);
    }
}

struct DashboardSnapshot {
    daemon_status: String,
    leases: Vec<Lease>,
    listeners: Vec<PortProcess>,
    registry_path: String,
    error: Option<String>,
}

impl DashboardSnapshot {
    fn load() -> Self {
        let registry_path = paths::registry_path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|err| format!("unavailable ({})", err));

        let daemon_status = match daemon_status() {
            Ok(status) => status,
            Err(err) => format!("unavailable ({})", err),
        };

        let mut errors = Vec::new();
        let leases = match super::helpers::load_active_leases() {
            Ok(leases) => leases,
            Err(err) => {
                errors.push(format!("registry: {}", err));
                Vec::new()
            }
        };

        let listeners = match scan_ports(None) {
            Ok(listeners) => listeners,
            Err(err) => {
                errors.push(format!("scan: {}", err));
                Vec::new()
            }
        };

        Self {
            daemon_status,
            leases,
            listeners,
            registry_path,
            error: (!errors.is_empty()).then(|| errors.join(" | ")),
        }
    }
}

fn daemon_status() -> Result<String> {
    let pid_path = paths::pid_path()?;
    let socket_path = paths::socket_path()?;

    if pid_path.exists() && socket_path.exists() {
        let pid = std::fs::read_to_string(&pid_path)
            .with_context(|| format!("failed to read {}", pid_path.display()))?;
        Ok(format!("running (pid {})", pid.trim()))
    } else {
        Ok("offline".into())
    }
}

fn draw_dashboard(frame: &mut ratatui::Frame<'_>, snapshot: &DashboardSnapshot) {
    let areas = Layout::vertical([
        Constraint::Length(5),
        Constraint::Min(8),
        Constraint::Min(8),
    ])
    .split(frame.area());

    let mut header_lines = vec![
        Line::from(vec![
            Span::styled(
                "Portus Dashboard",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw("  q: quit  r: refresh"),
        ]),
        Line::from(format!("Daemon: {}", snapshot.daemon_status)),
        Line::from(format!(
            "Registry: {}  Active leases: {}  Listeners: {}",
            snapshot.registry_path,
            snapshot.leases.len(),
            snapshot.listeners.len()
        )),
    ];

    if let Some(error) = &snapshot.error {
        header_lines.push(Line::from(format!("Warnings: {}", error)));
    }

    let header = Paragraph::new(header_lines)
        .block(Block::default().title("Status").borders(Borders::ALL))
        .wrap(Wrap { trim: true });
    frame.render_widget(header, areas[0]);

    let lease_rows: Vec<Row<'static>> = if snapshot.leases.is_empty() {
        vec![Row::new(vec![
            Cell::from("-"),
            Cell::from("-"),
            Cell::from("no active leases"),
            Cell::from("-"),
            Cell::from("-"),
        ])]
    } else {
        snapshot
            .leases
            .iter()
            .map(|lease| {
                Row::new(vec![
                    Cell::from(lease.port.to_string()),
                    Cell::from(format!("{:?}", lease.state).to_lowercase()),
                    Cell::from(lease.service_name.clone()),
                    Cell::from(
                        lease
                            .client_pid
                            .map(|pid| pid.to_string())
                            .unwrap_or_else(|| "-".into()),
                    ),
                    Cell::from(shorten(&lease.project_path, 36)),
                ])
            })
            .collect()
    };

    let lease_table = Table::new(
        lease_rows,
        [
            Constraint::Length(8),
            Constraint::Length(12),
            Constraint::Length(18),
            Constraint::Length(10),
            Constraint::Min(20),
        ],
    )
    .header(
        Row::new(vec!["Port", "State", "Service", "PID", "Project"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(
        Block::default()
            .title("Managed Leases")
            .borders(Borders::ALL),
    );
    frame.render_widget(lease_table, areas[1]);

    let listener_rows: Vec<Row<'static>> = if snapshot.listeners.is_empty() {
        vec![Row::new(vec![
            Cell::from("-"),
            Cell::from("-"),
            Cell::from("-"),
            Cell::from("no listeners found"),
        ])]
    } else {
        snapshot
            .listeners
            .iter()
            .take(12)
            .map(|listener| {
                Row::new(vec![
                    Cell::from(listener.port.to_string()),
                    Cell::from(listener.pid.to_string()),
                    Cell::from(format!("{:?}", listener.protocol).to_lowercase()),
                    Cell::from(listener.command.clone()),
                ])
            })
            .collect()
    };

    let listener_table = Table::new(
        listener_rows,
        [
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Min(20),
        ],
    )
    .header(
        Row::new(vec!["Port", "PID", "Proto", "Command"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(
        Block::default()
            .title("System Listeners")
            .borders(Borders::ALL),
    );
    frame.render_widget(listener_table, areas[2]);
}

fn shorten(value: &str, max: usize) -> String {
    if value.len() <= max {
        value.to_string()
    } else {
        format!("...{}", &value[value.len() - (max - 3)..])
    }
}
