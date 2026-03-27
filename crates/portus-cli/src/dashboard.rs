use std::io::{self, IsTerminal};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use crossterm::event::{self, Event, KeyCode};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use portus_core::model::{Lease, LeaseState};
use portus_core::paths;
use portus_core::scan::{scan_ports, PortProcess};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap};
use ratatui::Terminal;

// ── Typed view-state helpers ────────────────────────────────────────────

/// Daemon health as determined from PID/socket file presence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DaemonHealth {
    /// PID file and socket both exist — daemon appears to be running.
    Running,
    /// Neither PID file nor socket exists — daemon is offline.
    Offline,
    /// Could not determine status (e.g. path resolution failed).
    Unavailable,
}

impl DaemonHealth {
    /// Classify from the raw daemon-status probe result.
    pub(crate) fn classify(result: &std::result::Result<DaemonProbe, String>) -> Self {
        match result {
            Ok(probe) => {
                if probe.pid_exists && probe.socket_exists {
                    Self::Running
                } else {
                    Self::Offline
                }
            }
            Err(_) => Self::Unavailable,
        }
    }

    /// Human-readable label that conveys state *without* relying on color.
    pub(crate) fn label(self, probe: Option<&DaemonProbe>) -> String {
        match self {
            Self::Running => {
                let pid_str = probe.and_then(|p| p.pid_value.as_deref()).unwrap_or("?");
                format!("● running (pid {})", pid_str)
            }
            Self::Offline => "○ offline".into(),
            Self::Unavailable => "⚠ unavailable".into(),
        }
    }

    pub(crate) fn style(self) -> Style {
        match self {
            Self::Running => Style::default().fg(Color::Green),
            Self::Offline => Style::default().fg(Color::DarkGray),
            Self::Unavailable => Style::default().fg(Color::Yellow),
        }
    }
}

/// Raw probe result so we can separate IO from classification.
#[derive(Debug, Clone)]
pub(crate) struct DaemonProbe {
    pub(crate) pid_exists: bool,
    pub(crate) socket_exists: bool,
    pub(crate) pid_value: Option<String>,
}

/// Display variant for a lease state in the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LeaseDisplay {
    Pending,
    Active,
}

impl LeaseDisplay {
    pub(crate) fn from_state(state: &LeaseState) -> Self {
        match state {
            LeaseState::Active => Self::Active,
            // Pending, Released, Expired — we only show Pending/Active in the
            // dashboard (the loader already filters to those two).
            _ => Self::Pending,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Pending => "◌ pending",
            Self::Active => "● active",
        }
    }

    pub(crate) fn style(self) -> Style {
        match self {
            Self::Pending => Style::default().fg(Color::Yellow),
            Self::Active => Style::default().fg(Color::Green),
        }
    }
}

/// Whether a system listener is correlated with a Portus lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListenerOwnership {
    /// Listener matches a lease by port+protocol.
    Managed,
    /// No matching lease found.
    Unmanaged,
}

impl ListenerOwnership {
    /// Match a listener against all loaded leases using exact port+protocol.
    pub(crate) fn classify(listener: &PortProcess, leases: &[Lease]) -> Self {
        let is_managed = leases
            .iter()
            .any(|lease| lease.port == listener.port && lease.protocol == listener.protocol);
        if is_managed {
            Self::Managed
        } else {
            Self::Unmanaged
        }
    }

    pub(crate) fn tag(self) -> &'static str {
        match self {
            Self::Managed => "[managed]",
            Self::Unmanaged => "[unmanaged]",
        }
    }

    pub(crate) fn style(self) -> Style {
        match self {
            Self::Managed => Style::default().fg(Color::Cyan),
            Self::Unmanaged => Style::default().fg(Color::DarkGray),
        }
    }
}

// ── Dashboard entry point (unchanged interface) ─────────────────────────

/// Interactive TUI dashboard for monitoring ports and leases.
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

// ── Snapshot (data loading) ─────────────────────────────────────────────

struct DashboardSnapshot {
    daemon_probe: std::result::Result<DaemonProbe, String>,
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

        let daemon_probe = probe_daemon().map_err(|err| format!("{}", err));

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
            daemon_probe,
            leases,
            listeners,
            registry_path,
            error: (!errors.is_empty()).then(|| errors.join(" | ")),
        }
    }
}

fn probe_daemon() -> Result<DaemonProbe> {
    let pid_path = paths::pid_path()?;
    let socket_path = paths::socket_path()?;

    let pid_exists = pid_path.exists();
    let socket_exists = socket_path.exists();

    let pid_value = if pid_exists {
        std::fs::read_to_string(&pid_path)
            .map(|s| s.trim().to_string())
            .ok()
    } else {
        None
    };

    Ok(DaemonProbe {
        pid_exists,
        socket_exists,
        pid_value,
    })
}

// ── Drawing ─────────────────────────────────────────────────────────────

fn draw_dashboard(frame: &mut ratatui::Frame<'_>, snapshot: &DashboardSnapshot) {
    let areas = Layout::vertical([
        Constraint::Length(6),
        Constraint::Min(8),
        Constraint::Min(8),
    ])
    .split(frame.area());

    draw_status_pane(frame, snapshot, areas[0]);
    draw_lease_pane(frame, snapshot, areas[1]);
    draw_listener_pane(frame, snapshot, areas[2]);
}

fn draw_status_pane(
    frame: &mut ratatui::Frame<'_>,
    snapshot: &DashboardSnapshot,
    area: ratatui::layout::Rect,
) {
    let health = DaemonHealth::classify(&snapshot.daemon_probe);
    let probe = snapshot.daemon_probe.as_ref().ok();

    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                "Portus Dashboard",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  q: quit  r: refresh",
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(vec![
            Span::raw("Daemon: "),
            Span::styled(health.label(probe), health.style()),
        ]),
        Line::from(vec![
            Span::styled("Leases: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                snapshot.leases.len().to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled("  Listeners: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                snapshot.listeners.len().to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("Registry: ", Style::default().fg(Color::DarkGray)),
            Span::raw(&snapshot.registry_path),
        ]),
    ];

    if let Some(error) = &snapshot.error {
        lines.push(Line::from(vec![
            Span::styled("⚠ ", Style::default().fg(Color::Yellow)),
            Span::styled(error.as_str(), Style::default().fg(Color::Yellow)),
        ]));
    }

    let header = Paragraph::new(lines)
        .block(Block::default().title("Status").borders(Borders::ALL))
        .wrap(Wrap { trim: true });
    frame.render_widget(header, area);
}

fn draw_lease_pane(
    frame: &mut ratatui::Frame<'_>,
    snapshot: &DashboardSnapshot,
    area: ratatui::layout::Rect,
) {
    let block = Block::default()
        .title("Managed Leases")
        .borders(Borders::ALL);

    if snapshot.leases.is_empty() {
        let empty = Paragraph::new(Line::from(vec![Span::styled(
            "No active leases",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )]))
        .alignment(Alignment::Center)
        .block(block);
        frame.render_widget(empty, area);
        return;
    }

    let lease_rows: Vec<Row<'static>> = snapshot
        .leases
        .iter()
        .map(|lease| {
            let display = LeaseDisplay::from_state(&lease.state);
            Row::new(vec![
                Cell::from(lease.port.to_string()),
                Cell::from(Span::styled(display.label(), display.style())),
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
        .collect();

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
    .block(block);
    frame.render_widget(lease_table, area);
}

fn draw_listener_pane(
    frame: &mut ratatui::Frame<'_>,
    snapshot: &DashboardSnapshot,
    area: ratatui::layout::Rect,
) {
    let block = Block::default()
        .title("System Listeners")
        .borders(Borders::ALL);

    if snapshot.listeners.is_empty() {
        let empty = Paragraph::new(Line::from(vec![Span::styled(
            "No listeners detected",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )]))
        .alignment(Alignment::Center)
        .block(block);
        frame.render_widget(empty, area);
        return;
    }

    let listener_rows: Vec<Row<'static>> = snapshot
        .listeners
        .iter()
        .take(12)
        .map(|listener| {
            let ownership = ListenerOwnership::classify(listener, &snapshot.leases);
            Row::new(vec![
                Cell::from(listener.port.to_string()),
                Cell::from(listener.pid.to_string()),
                Cell::from(format!("{:?}", listener.protocol).to_lowercase()),
                Cell::from(Span::styled(ownership.tag(), ownership.style())),
                Cell::from(shorten(&listener.command, 40)),
            ])
        })
        .collect();

    let listener_table = Table::new(
        listener_rows,
        [
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Length(13),
            Constraint::Min(16),
        ],
    )
    .header(
        Row::new(vec!["Port", "PID", "Proto", "Owner", "Command"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(block);
    frame.render_widget(listener_table, area);
}

fn shorten(value: &str, max: usize) -> String {
    if value.len() <= max {
        value.to_string()
    } else {
        format!("...{}", &value[value.len() - (max - 3)..])
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use portus_core::model::{Lease, LeaseState, Protocol};
    use portus_core::scan::PortProcess;

    // ── DaemonHealth ────────────────────────────────────────────────

    #[test]
    fn health_running_when_both_files_exist() {
        let probe = Ok(DaemonProbe {
            pid_exists: true,
            socket_exists: true,
            pid_value: Some("1234".into()),
        });
        assert_eq!(DaemonHealth::classify(&probe), DaemonHealth::Running);
    }

    #[test]
    fn health_offline_when_pid_missing() {
        let probe = Ok(DaemonProbe {
            pid_exists: false,
            socket_exists: true,
            pid_value: None,
        });
        assert_eq!(DaemonHealth::classify(&probe), DaemonHealth::Offline);
    }

    #[test]
    fn health_offline_when_socket_missing() {
        let probe = Ok(DaemonProbe {
            pid_exists: true,
            socket_exists: false,
            pid_value: Some("99".into()),
        });
        assert_eq!(DaemonHealth::classify(&probe), DaemonHealth::Offline);
    }

    #[test]
    fn health_unavailable_on_error() {
        let probe: std::result::Result<DaemonProbe, String> = Err("path resolution failed".into());
        assert_eq!(DaemonHealth::classify(&probe), DaemonHealth::Unavailable);
    }

    #[test]
    fn health_label_running_includes_pid() {
        let probe = DaemonProbe {
            pid_exists: true,
            socket_exists: true,
            pid_value: Some("42".into()),
        };
        let label = DaemonHealth::Running.label(Some(&probe));
        assert!(label.contains("42"));
        assert!(label.contains("running"));
    }

    #[test]
    fn health_label_offline_no_pid() {
        let label = DaemonHealth::Offline.label(None);
        assert!(label.contains("offline"));
    }

    #[test]
    fn health_label_unavailable() {
        let label = DaemonHealth::Unavailable.label(None);
        assert!(label.contains("unavailable"));
    }

    // ── LeaseDisplay ────────────────────────────────────────────────

    #[test]
    fn lease_display_pending() {
        let display = LeaseDisplay::from_state(&LeaseState::Pending);
        assert_eq!(display, LeaseDisplay::Pending);
        assert!(display.label().contains("pending"));
    }

    #[test]
    fn lease_display_active() {
        let display = LeaseDisplay::from_state(&LeaseState::Active);
        assert_eq!(display, LeaseDisplay::Active);
        assert!(display.label().contains("active"));
    }

    #[test]
    fn lease_display_released_maps_to_pending() {
        // Released/Expired shouldn't appear (filtered out), but if they
        // do the fallback is Pending display.
        let display = LeaseDisplay::from_state(&LeaseState::Released);
        assert_eq!(display, LeaseDisplay::Pending);
    }

    // ── ListenerOwnership ───────────────────────────────────────────

    fn make_lease(port: u16, protocol: Protocol) -> Lease {
        Lease::new(
            "/tmp/test".into(),
            "test-svc".into(),
            port,
            protocol,
            Some(100),
            60,
        )
    }

    fn make_listener(port: u16, protocol: Protocol) -> PortProcess {
        PortProcess {
            port,
            pid: 200,
            command: "node".into(),
            protocol,
        }
    }

    #[test]
    fn listener_managed_when_port_and_protocol_match() {
        let leases = vec![make_lease(3000, Protocol::Tcp)];
        let listener = make_listener(3000, Protocol::Tcp);
        assert_eq!(
            ListenerOwnership::classify(&listener, &leases),
            ListenerOwnership::Managed
        );
    }

    #[test]
    fn listener_unmanaged_when_port_differs() {
        let leases = vec![make_lease(3000, Protocol::Tcp)];
        let listener = make_listener(4000, Protocol::Tcp);
        assert_eq!(
            ListenerOwnership::classify(&listener, &leases),
            ListenerOwnership::Unmanaged
        );
    }

    #[test]
    fn listener_unmanaged_when_protocol_differs() {
        let leases = vec![make_lease(3000, Protocol::Tcp)];
        let listener = make_listener(3000, Protocol::Udp);
        assert_eq!(
            ListenerOwnership::classify(&listener, &leases),
            ListenerOwnership::Unmanaged
        );
    }

    #[test]
    fn listener_unmanaged_when_no_leases() {
        let listener = make_listener(3000, Protocol::Tcp);
        assert_eq!(
            ListenerOwnership::classify(&listener, &[]),
            ListenerOwnership::Unmanaged
        );
    }

    #[test]
    fn listener_managed_among_many_leases() {
        let leases = vec![
            make_lease(8080, Protocol::Tcp),
            make_lease(5432, Protocol::Tcp),
            make_lease(3000, Protocol::Tcp),
        ];
        let listener = make_listener(5432, Protocol::Tcp);
        assert_eq!(
            ListenerOwnership::classify(&listener, &leases),
            ListenerOwnership::Managed
        );
    }

    #[test]
    fn ownership_tag_text() {
        assert_eq!(ListenerOwnership::Managed.tag(), "[managed]");
        assert_eq!(ListenerOwnership::Unmanaged.tag(), "[unmanaged]");
    }
}
