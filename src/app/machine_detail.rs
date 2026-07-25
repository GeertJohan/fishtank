//! The per-machine detail view — a tabbed page.
//!
//! Tabs (switch with F1/F2/F3): **Overview** (assorted system facts),
//! **Users** (BMC accounts + privileges), **Logs** (the SEL event log,
//! newest-first, severity-coloured). Opened from the list with `Enter`
//! (Overview), `u` (Users) or `l` (Logs). Each tab fetches its data lazily on
//! first view; `r` refetches the current tab. `j`/`k` (and arrows / PageUp-Down
//! / `g`/`G`) scroll; `q` or `Esc` returns to the list (which keeps polling in
//! the background).

use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use crossterm::event::{KeyCode, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
};
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

use crate::bmc::{self, BmcUser, Health, Overview, SelEntry};
use crate::inventory::Machine;
use crate::ratata::{Component, Event};

/// Selection / accent colour, matching the rest of the app.
const TEAL: Color = Color::Cyan;

/// Which detail tab is shown (also the F-key order).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DetailTab {
    Overview,
    Users,
    Logs,
}

impl DetailTab {
    const ALL: [DetailTab; 3] = [DetailTab::Overview, DetailTab::Users, DetailTab::Logs];

    fn label(self) -> &'static str {
        match self {
            DetailTab::Overview => "Overview",
            DetailTab::Users => "Users",
            DetailTab::Logs => "Logs",
        }
    }

    /// F-key index (1-based) used both as the shortcut and the tab-bar label.
    fn fkey(self) -> u8 {
        match self {
            DetailTab::Overview => 1,
            DetailTab::Users => 2,
            DetailTab::Logs => 3,
        }
    }
}

/// Lazy-load state for one tab's data.
#[derive(Clone)]
enum Load<T> {
    Idle,
    Loading,
    Loaded(T),
    Error(String),
}

#[derive(Clone)]
struct DetailState {
    tab: DetailTab,
    overview: Load<Arc<[(String, String)]>>,
    users: Load<Arc<[BmcUser]>>,
    logs: Load<Arc<[SelEntry]>>,
}

/// Upward message: the user asked to leave the detail view.
pub enum MachineDetailMsg {
    Close,
}

/// Result of a spawned per-tab fetch.
enum Fetched {
    Overview(Result<Overview, String>),
    Users(Result<Vec<BmcUser>, String>),
    Logs(Result<Vec<SelEntry>, String>),
}

pub struct MachineDetail {
    machine: Machine,
    demo: bool,
    state: ArcSwap<DetailState>,
    /// Render auto-scrolls this to keep the selection visible, hence the Mutex.
    /// Shared across tabs; reset to the top when switching.
    selected: Arc<Mutex<TableState>>,
}

impl MachineDetail {
    pub fn new(machine: Machine, demo: bool, tab: DetailTab) -> Arc<Self> {
        Arc::new(Self {
            machine,
            demo,
            state: ArcSwap::new(Arc::new(DetailState {
                tab,
                overview: Load::Idle,
                users: Load::Idle,
                logs: Load::Idle,
            })),
            selected: Arc::new(Mutex::new(TableState::default().with_selected(0))),
        })
    }

    fn update(&self, f: impl FnOnce(&mut DetailState)) {
        let mut s = (*self.state.load_full()).clone();
        f(&mut s);
        self.state.store(Arc::new(s));
    }

    /// Number of selectable rows in the active tab (for scroll clamping).
    fn current_len(&self) -> usize {
        let s = self.state.load();
        match s.tab {
            DetailTab::Overview => match &s.overview {
                Load::Loaded(v) => v.len(),
                _ => 0,
            },
            DetailTab::Users => match &s.users {
                Load::Loaded(v) => v.len(),
                _ => 0,
            },
            DetailTab::Logs => match &s.logs {
                Load::Loaded(v) => v.len(),
                _ => 0,
            },
        }
    }

    fn is_idle(&self, tab: DetailTab) -> bool {
        let s = self.state.load();
        match tab {
            DetailTab::Overview => matches!(s.overview, Load::Idle),
            DetailTab::Users => matches!(s.users, Load::Idle),
            DetailTab::Logs => matches!(s.logs, Load::Idle),
        }
    }

    fn set_loading(&self, tab: DetailTab) {
        self.update(|s| match tab {
            DetailTab::Overview => s.overview = Load::Loading,
            DetailTab::Users => s.users = Load::Loading,
            DetailTab::Logs => s.logs = Load::Loading,
        });
    }

    fn fetch(&self, tab: DetailTab, tx: &mpsc::Sender<Fetched>, cancel: &CancellationToken) {
        let machine = self.machine.clone();
        let demo = self.demo;
        let tx = tx.clone();
        let child = cancel.child_token();
        tokio::spawn(async move {
            tokio::select! {
                _ = child.cancelled() => {}
                fetched = async {
                    match tab {
                        DetailTab::Overview => Fetched::Overview(bmc::poll_overview(&machine, demo).await),
                        DetailTab::Users => Fetched::Users(bmc::poll_users(&machine, demo).await),
                        DetailTab::Logs => Fetched::Logs(bmc::poll_log(&machine, demo).await),
                    }
                } => {
                    let _ = tx.send(fetched).await;
                }
            }
        });
    }

    /// Fetch a tab's data if it hasn't been loaded yet.
    fn ensure_fetched(
        &self,
        tab: DetailTab,
        tx: &mpsc::Sender<Fetched>,
        cancel: &CancellationToken,
    ) {
        if self.is_idle(tab) {
            self.set_loading(tab);
            self.fetch(tab, tx, cancel);
        }
    }

    fn switch_tab(&self, tab: DetailTab, tx: &mpsc::Sender<Fetched>, cancel: &CancellationToken) {
        self.update(|s| s.tab = tab);
        self.selected.lock().unwrap().select(Some(0));
        self.ensure_fetched(tab, tx, cancel);
    }

    fn refetch_current(&self, tx: &mpsc::Sender<Fetched>, cancel: &CancellationToken) {
        let tab = self.state.load().tab;
        self.set_loading(tab);
        self.fetch(tab, tx, cancel);
    }

    fn apply(&self, fetched: Fetched) {
        self.update(|s| match fetched {
            Fetched::Overview(r) => s.overview = into_load(r.map(Into::into)),
            Fetched::Users(r) => s.users = into_load(r.map(Into::into)),
            Fetched::Logs(r) => s.logs = into_load(r.map(Into::into)),
        });
    }

    fn move_selection(&self, delta: isize) {
        let n = self.current_len();
        if n == 0 {
            return;
        }
        let mut st = self.selected.lock().unwrap();
        let cur = st.selected().unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, n as isize - 1) as usize;
        st.select(Some(next));
    }

    fn select_index(&self, idx: usize) {
        let n = self.current_len();
        if n == 0 {
            return;
        }
        self.selected.lock().unwrap().select(Some(idx.min(n - 1)));
    }
}

fn into_load<T>(r: Result<T, String>) -> Load<T> {
    match r {
        Ok(v) => Load::Loaded(v),
        Err(e) => Load::Error(e),
    }
}

impl Component for MachineDetail {
    type Msg = MachineDetailMsg;

    async fn run(
        self: Arc<Self>,
        mut event_rx: mpsc::Receiver<Event>,
        msg_tx: mpsc::Sender<MachineDetailMsg>,
        redraw: Arc<Notify>,
        cancel: CancellationToken,
    ) {
        let (tx, mut rx) = mpsc::channel::<Fetched>(8);
        // Fetch whichever tab we opened on.
        self.ensure_fetched(self.state.load().tab, &tx, &cancel);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                maybe_ev = event_rx.recv() => {
                    let Some(Event::Key(k)) = maybe_ev else {
                        if event_rx.is_closed() { break; }
                        continue;
                    };
                    let press = k.kind == KeyEventKind::Press;
                    let repeat = k.kind == KeyEventKind::Repeat;
                    if !press && !repeat {
                        continue;
                    }
                    match k.code {
                        KeyCode::F(1) if press => self.switch_tab(DetailTab::Overview, &tx, &cancel),
                        KeyCode::F(2) if press => self.switch_tab(DetailTab::Users, &tx, &cancel),
                        KeyCode::F(3) if press => self.switch_tab(DetailTab::Logs, &tx, &cancel),
                        KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
                        KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
                        KeyCode::PageDown => self.move_selection(10),
                        KeyCode::PageUp => self.move_selection(-10),
                        KeyCode::Char('g') if press => self.select_index(0),
                        KeyCode::Char('G') if press => self.select_index(usize::MAX),
                        KeyCode::Char('r') if press => self.refetch_current(&tx, &cancel),
                        KeyCode::Esc | KeyCode::Char('q') if press => {
                            let _ = msg_tx.send(MachineDetailMsg::Close).await;
                            break;
                        }
                        _ => continue, // don't redraw on unhandled keys
                    }
                    redraw.notify_one();
                }
                Some(fetched) = rx.recv() => {
                    self.apply(fetched);
                    redraw.notify_one();
                }
            }
        }
    }

    fn render(&self, area: Rect, frame: &mut Frame) {
        let snap = self.state.load();
        let m = &self.machine;

        let [info, tabs, body] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(area);

        // Machine identity line.
        let info_line = Line::from(vec![
            Span::styled(
                format!(" {} ", m.name),
                Style::new()
                    .fg(Color::Black)
                    .bg(TEAL)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("  {}  {}", m.protocol, m.host)),
        ]);
        frame.render_widget(Paragraph::new(info_line), info);

        // Tab bar.
        let mut spans = Vec::new();
        for t in DetailTab::ALL {
            let text = format!(" F{} {} ", t.fkey(), t.label());
            let style = if t == snap.tab {
                Style::new()
                    .fg(Color::Black)
                    .bg(TEAL)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::Gray)
            };
            spans.push(Span::styled(text, style));
            spans.push(Span::raw(" "));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), tabs);

        // Body for the active tab.
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", snap.tab.label()))
            .border_style(Style::new().fg(TEAL));

        match snap.tab {
            DetailTab::Overview => self.render_overview(&snap.overview, block, body, frame),
            DetailTab::Users => self.render_users(&snap.users, block, body, frame),
            DetailTab::Logs => self.render_logs(&snap.logs, block, body, frame),
        }
    }
}

impl MachineDetail {
    fn render_overview(
        &self,
        load: &Load<Arc<[(String, String)]>>,
        block: Block,
        area: Rect,
        frame: &mut Frame,
    ) {
        let Some(rows_data) =
            self.placeholder_or_data(load, "No data.", block.clone(), area, frame)
        else {
            return;
        };
        let rows: Vec<Row> = rows_data
            .iter()
            .map(|(k, v)| {
                Row::new(vec![
                    Cell::from(Span::styled(k.clone(), Style::new().fg(Color::DarkGray))),
                    Cell::from(Span::styled(v.clone(), Style::new().fg(Color::White))),
                ])
            })
            .collect();
        let table = Table::new(rows, [Constraint::Length(16), Constraint::Fill(1)])
            .block(block)
            .row_highlight_style(
                Style::new()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_stateful_widget(table, area, &mut self.selected.lock().unwrap());
    }

    fn render_users(
        &self,
        load: &Load<Arc<[BmcUser]>>,
        block: Block,
        area: Rect,
        frame: &mut Frame,
    ) {
        let Some(users) = self.placeholder_or_data(load, "No users.", block.clone(), area, frame)
        else {
            return;
        };
        let header = Row::new(["ID", "Name", "Privilege", "Enabled"])
            .style(Style::new().add_modifier(Modifier::BOLD));
        let rows: Vec<Row> = users.iter().map(user_row).collect();
        let widths = [
            Constraint::Length(4),
            Constraint::Fill(1),
            Constraint::Fill(1),
            Constraint::Length(8),
        ];
        let table = Table::new(rows, widths)
            .header(header)
            .block(block)
            .row_highlight_style(
                Style::new()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_stateful_widget(table, area, &mut self.selected.lock().unwrap());
    }

    fn render_logs(
        &self,
        load: &Load<Arc<[SelEntry]>>,
        block: Block,
        area: Rect,
        frame: &mut Frame,
    ) {
        let Some(entries) =
            self.placeholder_or_data(load, "No SEL entries.", block.clone(), area, frame)
        else {
            return;
        };
        let rows: Vec<Row> = entries.iter().map(sel_row).collect();
        // Sensor and text both flex; the fixed columns stay tight.
        let widths = [
            Constraint::Length(4),  // severity tag
            Constraint::Length(4),  // record id
            Constraint::Length(19), // timestamp
            Constraint::Fill(1),    // sensor (flex)
            Constraint::Fill(2),    // event text (flex, larger share)
        ];
        let table = Table::new(rows, widths).block(block).row_highlight_style(
            Style::new()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
        frame.render_stateful_widget(table, area, &mut self.selected.lock().unwrap());
    }

    /// Render the Loading/Error/empty placeholder for a tab, or return the data
    /// when it's loaded and non-empty.
    fn placeholder_or_data<'a, T>(
        &self,
        load: &'a Load<Arc<[T]>>,
        empty_msg: &str,
        block: Block,
        area: Rect,
        frame: &mut Frame,
    ) -> Option<&'a Arc<[T]>> {
        let (msg, color) = match load {
            Load::Idle | Load::Loading => ("Loading…".to_string(), Color::Yellow),
            Load::Error(e) => (format!("Error: {e}"), Color::Red),
            Load::Loaded(v) if v.is_empty() => (empty_msg.to_string(), Color::DarkGray),
            Load::Loaded(v) => return Some(v),
        };
        let p = Paragraph::new(msg)
            .block(block)
            .style(Style::new().fg(color));
        frame.render_widget(p, area);
        None
    }
}

fn user_row(u: &BmcUser) -> Row<'static> {
    let priv_color = match u.privilege.to_ascii_uppercase().as_str() {
        "ADMINISTRATOR" => Color::Red,
        "OPERATOR" => Color::Yellow,
        "USER" => Color::Green,
        _ => Color::DarkGray,
    };
    let (en_text, en_color) = if u.enabled {
        ("yes", Color::Green)
    } else {
        ("no", Color::DarkGray)
    };
    Row::new(vec![
        Cell::from(Span::styled(
            format!("{:>3}", u.id),
            Style::new().fg(Color::DarkGray),
        )),
        Cell::from(Span::styled(u.name.clone(), Style::new().fg(Color::White))),
        Cell::from(Span::styled(
            u.privilege.clone(),
            Style::new().fg(priv_color),
        )),
        Cell::from(Span::styled(en_text, Style::new().fg(en_color))),
    ])
}

/// One SEL row: a colour-coded severity tag, id, timestamp, sensor and text.
fn sel_row(e: &SelEntry) -> Row<'static> {
    let (tag, color) = match e.severity {
        Health::Ok => ("OK", Color::Green),
        Health::Warning => ("WARN", Color::Yellow),
        Health::Critical => ("CRIT", Color::Red),
        Health::Unknown => ("·", Color::DarkGray),
    };
    let when = if e.when.is_empty() {
        "—".to_string()
    } else {
        e.when.clone()
    };
    let id = if e.id.is_empty() {
        "—".to_string()
    } else {
        e.id.clone()
    };
    Row::new(vec![
        Cell::from(Span::styled(
            tag,
            Style::new().fg(color).add_modifier(Modifier::BOLD),
        )),
        Cell::from(Span::styled(
            format!("{id:>3}"),
            Style::new().fg(Color::DarkGray),
        )),
        Cell::from(Span::styled(when, Style::new().fg(Color::DarkGray))),
        Cell::from(Span::styled(
            e.sensor.clone(),
            Style::new().fg(Color::White),
        )),
        Cell::from(e.text.clone()),
    ])
}
