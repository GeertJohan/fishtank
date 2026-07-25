//! The machine-list view — fishtank's core component.
//!
//! Holds the inventory rows behind an [`ArcSwap`]. Two background cadences poll
//! the BMCs (power on the frequent `poll_interval`, health on the slower
//! `health_interval`); results patch the matching row in place.
//!
//! Selection, power & boot control:
//! - `space` toggles the cursor row's membership in the active selection; marked
//!   rows show a teal `▶` marker and a teal name.
//! - `p` opens the power modal, `b` the boot modal. Both use the same widget: a
//!   cursor list with per-row hotkeys (`> [k] label`). Navigate with ↑/↓ (j/k);
//!   they act on the selection, falling back to the cursor row when nothing is
//!   marked.
//! - Confirming an action is **press-and-hold for 1.5s**: a coloured bar fills
//!   behind the label. Hold a row's hotkey (which also selects it), or move the
//!   cursor and hold Enter. In enhanced (kitty) terminals releasing the key
//!   early cancels; otherwise the fill is skipped and a single press commits.
//!   `Esc` cancels and the modal consumes every other key.
//! - Each acted row then runs `modifying… → modifying ✓` (or `modify ✗`), the
//!   ✓/✗ lingering 5s (hiding live state) before reverting. MAC-mismatch rows
//!   are skipped.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use crossterm::event::{KeyCode, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState},
};
use tokio::sync::{Notify, Semaphore, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::bmc::{self, BootAction, BootOverride, Health, PowerAction, PowerState};
use crate::inventory::{Inventory, Machine, Protocol};
use crate::ratata::{Component, ConsoleRequest, Event};

use super::machine_detail::DetailTab;

/// Upward messages to [`AppRoot`](super::AppRoot).
pub enum MachineListMsg {
    /// Open the per-machine detail view for this machine, on the given tab.
    OpenDetail(Machine, super::machine_detail::DetailTab),
}

/// Max concurrent BMC polls/actions in flight.
const MAX_CONCURRENT_POLLS: usize = 16;

/// A power poll slower than this flips the row's state to "slow".
const SLOW_THRESHOLD: Duration = Duration::from_secs(10);

/// How long the `modifying ✓`/`modify ✗` result lingers before reverting.
const MODIFY_LINGER: Duration = Duration::from_secs(5);

/// Press-and-hold duration and animation frame interval. The fill bar spans the
/// full modal width and advances smoothly over `HOLD_DURATION`.
const HOLD_DURATION: Duration = Duration::from_millis(1500);
const HOLD_FRAME: Duration = Duration::from_millis(40);

/// A hold is sustained by the key's auto-repeat acting as a heartbeat. If no
/// key event for the held key arrives within this window, the key is considered
/// released and the hold cancels. Must exceed a typical auto-repeat initial
/// delay yet stay well under the 1.5s fill so a quick tap can never complete.
const HEARTBEAT_TIMEOUT: Duration = Duration::from_millis(750);

/// Width of the Serial column (left-truncated when longer).
const SERIAL_W: u16 = 16;

/// Selection / modal accent colour ("teal").
const TEAL: Color = Color::Cyan;

/// Power actions, in the order shown in the power modal.
const POWER_ACTIONS: [Action; 5] = [
    Action::Power(PowerAction::On),
    Action::Power(PowerAction::SoftOff),
    Action::Power(PowerAction::ForceOff),
    Action::Power(PowerAction::Cycle),
    Action::Power(PowerAction::Reset),
];

/// Boot overrides, in the order shown in the boot modal.
const BOOT_ACTIONS: [Action; 5] = [
    Action::Boot(BootAction::Pxe),
    Action::Boot(BootAction::Disk),
    Action::Boot(BootAction::Bios),
    Action::Boot(BootAction::Cd),
    Action::Boot(BootAction::Clear),
];

/// An action issued from a modal: either a power control or a boot override.
/// Both share the same modal widget (a cursor list with per-row hotkeys), the
/// selection, the MAC-mismatch guard and the modify lifecycle.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Power(PowerAction),
    Boot(BootAction),
}

/// Which modal is open — selects the action set and the title; the interaction
/// is identical for both.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ModalKind {
    Power,
    Boot,
}

impl ModalKind {
    fn actions(self) -> &'static [Action] {
        match self {
            ModalKind::Power => &POWER_ACTIONS,
            ModalKind::Boot => &BOOT_ACTIONS,
        }
    }

    fn title(self) -> &'static str {
        match self {
            ModalKind::Power => " Power ",
            ModalKind::Boot => " Boot ",
        }
    }
}

/// Lifecycle of an in-progress / just-finished power modification on a row.
/// After it finishes (Done/Failed) the status lingers until BOTH the linger
/// timer fired (`lingered`) AND a fresh poll has landed (`refreshed`), so we
/// never snap back to a stale row before the new state is reflected.
#[derive(Clone, Copy)]
struct Modify {
    kind: ModifyKind,
    seq: u64,
    lingered: bool,
    refreshed: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ModifyKind {
    Busy,
    Done,
    Failed,
}

/// A machine plus its latest, independently-updated state.
#[derive(Clone)]
pub struct MachineRow {
    pub machine: Machine,
    pub serial: Option<String>,
    pub observed_mac: Option<String>,
    pub reachable: Option<bool>,
    pub power: PowerState,
    pub health: Health,
    pub latency: Duration,
    pub fault: bool,
    pub identify: bool,
    pub boot: BootOverride,
    marked: bool,
    modify: Option<Modify>,
}

impl MachineRow {
    fn mac_mismatch(&self) -> bool {
        match (&self.machine.mac, &self.observed_mac) {
            (Some(cfg), Some(obs)) => norm_mac(cfg) != norm_mac(obs),
            _ => false,
        }
    }
}

#[derive(Clone)]
struct MachineListState {
    rows: Arc<[MachineRow]>,
}

/// The centered power/boot modal (which rows it targets).
#[derive(Clone)]
struct Modal {
    kind: ModalKind,
    targets: Vec<usize>,
    label: String,
    /// Highlighted entry (the `>` cursor).
    cursor: usize,
    /// Boot only: apply the override persistently (across reboots) vs next-boot.
    persistent: bool,
}

/// An in-progress press-and-hold on a modal action.
#[derive(Clone, Copy)]
struct Hold {
    action: Action,
    /// The key sustaining this hold — the action's hotkey, or Enter when the
    /// hold was started from the cursor row. Releasing/stopping it cancels.
    key: KeyCode,
    seq: u64,
    /// When the hold began — the fill fraction is `elapsed / HOLD_DURATION`.
    started: Instant,
    /// Last time we saw a key event for the held key (auto-repeat heartbeat).
    last_beat: Instant,
}

#[derive(Clone, Default)]
struct UiState {
    modal: Option<Modal>,
    hold: Option<Hold>,
}

/// A result coming back from a spawned task.
enum Update {
    Power {
        idx: usize,
        poll: bmc::PowerPoll,
    },
    Health {
        idx: usize,
        health: Option<Health>,
    },
    ActionDone {
        idx: usize,
        seq: u64,
        action: Action,
        result: Result<(), String>,
    },
    ModifyExpire {
        idx: usize,
        seq: u64,
    },
    HoldTick {
        seq: u64,
    },
}

pub struct MachineList {
    state: ArcSwap<MachineListState>,
    ui: ArcSwap<UiState>,
    selected: Arc<Mutex<TableState>>,
    seq: AtomicU64,
    power_interval: Duration,
    health_interval: Duration,
    demo: bool,
    /// Enhanced (kitty) keyboard: key-release events are available, so power
    /// confirm is a true hold (releasing cancels). Otherwise it auto-completes.
    enhanced: bool,
    /// Sends console (SOL) suspend-and-exec requests up to the runtime.
    console_tx: mpsc::Sender<ConsoleRequest>,
    /// True while a console session is pending/active — gates re-launch so a
    /// held `c` (or rapid taps) can't queue repeated sessions.
    console_pending: Arc<AtomicBool>,
    /// The key+time of the last hold that fired. Auto-repeat of that key keeps
    /// arriving briefly after the hold completes and the modal closes; we
    /// swallow it at the list level so e.g. holding Enter to confirm doesn't
    /// then open the detail page (and held `p`/`b`/`c` don't re-trigger).
    last_fire: Mutex<Option<(KeyCode, Instant)>>,
}

impl MachineList {
    pub fn new(
        inventory: Inventory,
        demo: bool,
        enhanced: bool,
        console_tx: mpsc::Sender<ConsoleRequest>,
    ) -> Arc<Self> {
        let rows: Vec<MachineRow> = inventory
            .machines
            .into_iter()
            .map(|machine| MachineRow {
                serial: machine.serial.clone(),
                observed_mac: None,
                reachable: None,
                power: PowerState::Unknown,
                health: Health::Unknown,
                latency: Duration::ZERO,
                fault: false,
                identify: false,
                boot: BootOverride::None,
                marked: false,
                modify: None,
                machine,
            })
            .collect();

        let mut ts = TableState::default();
        if !rows.is_empty() {
            ts.select(Some(0));
        }

        Arc::new(Self {
            state: ArcSwap::new(Arc::new(MachineListState { rows: rows.into() })),
            ui: ArcSwap::new(Arc::new(UiState::default())),
            selected: Arc::new(Mutex::new(ts)),
            seq: AtomicU64::new(0),
            power_interval: Duration::from_secs(inventory.poll_interval_secs.max(1)),
            health_interval: Duration::from_secs(inventory.health_interval_secs.max(1)),
            demo,
            enhanced,
            console_tx,
            console_pending: Arc::new(AtomicBool::new(false)),
            last_fire: Mutex::new(None),
        })
    }

    fn row_count(&self) -> usize {
        self.state.load().rows.len()
    }

    /// True while the modal owns input — the parent must not act on globals
    /// (Ctrl+C / q) so the modal can consume every key.
    pub fn is_capturing(&self) -> bool {
        self.ui.load().modal.is_some()
    }

    fn selected_idx(&self) -> Option<usize> {
        self.selected.lock().unwrap().selected()
    }

    fn row_name(&self, idx: usize) -> String {
        self.state
            .load()
            .rows
            .get(idx)
            .map(|r| r.machine.name.clone())
            .unwrap_or_default()
    }

    fn select_next(&self) {
        let n = self.row_count();
        if n == 0 {
            return;
        }
        let mut st = self.selected.lock().unwrap();
        let i = st.selected().map_or(0, |i| (i + 1) % n);
        st.select(Some(i));
    }

    fn select_prev(&self) {
        let n = self.row_count();
        if n == 0 {
            return;
        }
        let mut st = self.selected.lock().unwrap();
        let i = st
            .selected()
            .map_or(0, |i| if i == 0 { n - 1 } else { i - 1 });
        st.select(Some(i));
    }

    fn toggle_mark(&self) {
        let Some(idx) = self.selected_idx() else {
            return;
        };
        self.state.rcu(|cur| {
            let mut rows = cur.rows.to_vec();
            if let Some(r) = rows.get_mut(idx) {
                r.marked = !r.marked;
            }
            Arc::new(MachineListState { rows: rows.into() })
        });
    }

    // --- detail navigation -------------------------------------------------

    /// Ask [`AppRoot`](super::AppRoot) to open the detail view (on `tab`) for the
    /// cursor row.
    async fn open_detail(&self, tab: DetailTab, msg_tx: &mpsc::Sender<MachineListMsg>) {
        if let Some(idx) = self.selected_idx()
            && let Some(m) = self.state.load().rows.get(idx).map(|r| r.machine.clone())
        {
            let _ = msg_tx.send(MachineListMsg::OpenDetail(m, tab)).await;
        }
    }

    // --- console (SOL) -----------------------------------------------------

    /// Launch the serial console for the cursor row (single machine, like the
    /// TODO specifies — not the multi-selection). Sends a [`ConsoleRequest`] up
    /// to the runtime, which suspends the TUI and hands over the terminal. In
    /// `--demo` there's no BMC, so it runs a placeholder shell to exercise the
    /// suspend/exec/resume path.
    fn launch_console(&self) {
        let Some(idx) = self.selected_idx() else {
            return;
        };
        let Some(row) = self.state.load().rows.get(idx).cloned() else {
            return;
        };
        let m = &row.machine;

        // Validate before reserving the single-console slot.
        let real_ipmi = !self.demo && matches!(m.protocol, Protocol::Ipmi);
        if !self.demo {
            if !matches!(m.protocol, Protocol::Ipmi) {
                tracing::warn!("console unavailable for {}: IPMI (SOL) only", m.name);
                return;
            }
            if row.mac_mismatch() {
                tracing::warn!("refusing console for {}: MAC mismatch", m.name);
                return;
            }
        }

        // One session at a time — ignore launches while one is pending/active.
        if self.console_pending.swap(true, Ordering::SeqCst) {
            return;
        }

        let (reply, reply_rx) = oneshot::channel();
        let req = if self.demo {
            let note = format!("Console for {} (demo — no real BMC)", m.name);
            let script = format!(
                "clear; printf '\\n  fishtank demo console for %s\\n  (no real BMC in --demo)\\n\\n' '{}'; \
                 printf '  Press Enter to return to fishtank…'; read _",
                m.name
            );
            ConsoleRequest {
                program: "sh".to_string(),
                args: vec!["-c".to_string(), script],
                envs: Vec::new(),
                note,
                reply,
            }
        } else {
            let note = format!(
                "Connecting to {} serial console — type '~.' to exit.",
                m.name
            );
            ConsoleRequest {
                program: "ipmitool".to_string(),
                args: vec![
                    "-I".to_string(),
                    "lanplus".to_string(),
                    "-H".to_string(),
                    m.host.clone(),
                    "-U".to_string(),
                    m.username.clone(),
                    "-E".to_string(),
                    "sol".to_string(),
                    "activate".to_string(),
                ],
                envs: vec![("IPMI_PASSWORD".to_string(), m.password.clone())],
                note,
                reply,
            }
        };

        let console_tx = self.console_tx.clone();
        let pending = self.console_pending.clone();
        let machine = m.clone();
        tokio::spawn(async move {
            // Pre-flight: clear any stale SOL session before activating.
            if real_ipmi {
                let _ = crate::bmc::ipmi::sol_deactivate(&machine).await;
            }
            if console_tx.send(req).await.is_ok() {
                // Hold the slot until the runtime finishes the session.
                let _ = reply_rx.await;
            }
            pending.store(false, Ordering::SeqCst);
        });
    }

    // --- modal / hold ------------------------------------------------------

    fn open_modal(&self, kind: ModalKind) {
        let snap = self.state.load();
        if snap.rows.is_empty() {
            return;
        }
        // Act on the marked rows, or fall back to the highlighted (cursor) row
        // when nothing is marked.
        let marked: Vec<usize> = snap
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.marked)
            .map(|(i, _)| i)
            .collect();
        let targets = if marked.is_empty() {
            match self.selected_idx() {
                Some(i) => vec![i],
                None => return,
            }
        } else {
            marked
        };
        let label = if targets.len() == 1 {
            snap.rows
                .get(targets[0])
                .map(|r| r.machine.name.clone())
                .unwrap_or_default()
        } else {
            format!("{} machines", targets.len())
        };
        self.ui.store(Arc::new(UiState {
            modal: Some(Modal {
                kind,
                targets,
                label,
                cursor: 0,
                persistent: false,
            }),
            hold: None,
        }));
    }

    /// Toggle the boot modal's persistent flag (once ⇄ persistent). Boot only;
    /// no-op while a hold is active.
    fn toggle_persistent(&self, redraw: &Notify) {
        let ui = self.ui.load();
        if ui.hold.is_some() {
            return;
        }
        let Some(modal) = ui.modal.as_ref() else {
            return;
        };
        if modal.kind != ModalKind::Boot {
            return;
        }
        self.ui.store(Arc::new(UiState {
            modal: Some(Modal {
                persistent: !modal.persistent,
                ..modal.clone()
            }),
            hold: None,
        }));
        redraw.notify_one();
    }

    fn close_modal(&self) {
        self.ui.store(Arc::new(UiState::default()));
    }

    /// Move the modal cursor (wrapping). No-op while a hold is active.
    fn move_modal_cursor(&self, delta: isize, redraw: &Notify) {
        let ui = self.ui.load();
        if ui.hold.is_some() {
            return;
        }
        let Some(modal) = ui.modal.as_ref() else {
            return;
        };
        let n = modal.kind.actions().len();
        if n == 0 {
            return;
        }
        let next = (modal.cursor as isize + delta).rem_euclid(n as isize) as usize;
        self.ui.store(Arc::new(UiState {
            modal: Some(Modal {
                cursor: next,
                ..modal.clone()
            }),
            hold: None,
        }));
        redraw.notify_one();
    }

    /// Set the modal cursor to a specific row (used when a hotkey selects it).
    fn set_modal_cursor(&self, idx: usize) {
        let ui = self.ui.load();
        let Some(modal) = ui.modal.as_ref() else {
            return;
        };
        let n = modal.kind.actions().len();
        if n == 0 {
            return;
        }
        self.ui.store(Arc::new(UiState {
            modal: Some(Modal {
                cursor: idx.min(n - 1),
                ..modal.clone()
            }),
            hold: ui.hold,
        }));
    }

    /// Confirm an action from the modal: a press-and-hold (with the fill
    /// animation) on enhanced terminals, or an immediate commit on legacy ones
    /// (no key-release events, so hold/animation is skipped). `key` is the key
    /// sustaining the hold — the action's hotkey, or Enter for the cursor row.
    fn begin_action(
        &self,
        action: Action,
        key: KeyCode,
        tx: &mpsc::Sender<Update>,
        sem: &Arc<Semaphore>,
        cancel: &CancellationToken,
        redraw: &Notify,
    ) {
        if self.enhanced {
            self.start_hold(action, key, tx, redraw);
        } else {
            self.commit_action(action, tx, sem, cancel, redraw);
        }
    }

    /// Begin a press-and-hold on `action`, animating the fill bar.
    fn start_hold(&self, action: Action, key: KeyCode, tx: &mpsc::Sender<Update>, redraw: &Notify) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        let modal = self.ui.load().modal.clone();
        self.ui.store(Arc::new(UiState {
            modal,
            hold: Some(Hold {
                action,
                key,
                seq,
                started: now,
                last_beat: now,
            }),
        }));
        redraw.notify_one();

        // Drive the animation: a frame every HOLD_FRAME for a little past
        // HOLD_DURATION. Stale ticks (after cancel/fire) are ignored by the seq
        // check in the handler.
        let frames = (HOLD_DURATION.as_millis() / HOLD_FRAME.as_millis()) as usize + 4;
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(HOLD_FRAME);
            for _ in 0..frames {
                iv.tick().await;
                if tx.send(Update::HoldTick { seq }).await.is_err() {
                    break;
                }
            }
        });
    }

    /// Cancel the active hold but keep the modal open (back to the action list).
    fn cancel_hold(&self, redraw: &Notify) {
        let modal = self.ui.load().modal.clone();
        self.ui.store(Arc::new(UiState { modal, hold: None }));
        redraw.notify_one();
    }

    /// Whether `code` is the lingering auto-repeat of a key that just confirmed
    /// a hold (within [`HEARTBEAT_TIMEOUT`]). Expired entries are cleared.
    fn suppress_after_fire(&self, code: KeyCode) -> bool {
        let mut lf = self.last_fire.lock().unwrap();
        match *lf {
            Some((key, t)) if t.elapsed() < HEARTBEAT_TIMEOUT => key == code,
            Some(_) => {
                *lf = None;
                false
            }
            None => false,
        }
    }

    /// Record an auto-repeat heartbeat for the active hold (key still held).
    fn beat(&self) {
        let ui = self.ui.load();
        if let Some(h) = ui.hold {
            self.ui.store(Arc::new(UiState {
                modal: ui.modal.clone(),
                hold: Some(Hold {
                    last_beat: Instant::now(),
                    ..h
                }),
            }));
        }
    }

    fn advance_hold(
        &self,
        seq: u64,
        tx: &mpsc::Sender<Update>,
        sem: &Arc<Semaphore>,
        cancel: &CancellationToken,
        redraw: &Notify,
    ) {
        let Some(hold) = self.ui.load().hold else {
            return;
        };
        if hold.seq != seq {
            return;
        }
        // No recent auto-repeat → the key was released (or never sustained).
        // Cancel rather than auto-firing — this is what makes a tap safe even
        // on terminals that never deliver key-release events.
        if hold.last_beat.elapsed() > HEARTBEAT_TIMEOUT {
            self.cancel_hold(redraw);
            return;
        }
        if hold.started.elapsed() >= HOLD_DURATION {
            // Hold complete → commit the action (this also closes the modal).
            // Remember the key so its lingering auto-repeat doesn't leak to the
            // list (Enter→detail, p→power, …) before the user lets go.
            *self.last_fire.lock().unwrap() = Some((hold.key, Instant::now()));
            self.commit_action(hold.action, tx, sem, cancel, redraw);
        } else {
            // Still filling — render recomputes the bar from `started.elapsed()`.
            redraw.notify_one();
        }
    }

    /// Commit the modal's action against each (non-mismatched) target.
    fn commit_action(
        &self,
        action: Action,
        tx: &mpsc::Sender<Update>,
        sem: &Arc<Semaphore>,
        cancel: &CancellationToken,
        redraw: &Notify,
    ) {
        let Some(modal) = self.ui.load().modal.clone() else {
            return;
        };
        let persistent = modal.persistent;
        self.close_modal();

        let snap = self.state.load();
        let mut ops: Vec<(usize, u64)> = Vec::new();
        for &idx in &modal.targets {
            match snap.rows.get(idx) {
                Some(row) if row.mac_mismatch() => {
                    tracing::warn!(
                        "skipping {} for {}: MAC mismatch",
                        action_label(action),
                        row.machine.name
                    );
                }
                Some(_) => ops.push((idx, self.seq.fetch_add(1, Ordering::Relaxed))),
                None => {}
            }
        }
        drop(snap);

        if ops.is_empty() {
            redraw.notify_one();
            return;
        }

        self.state.rcu(|cur| {
            let mut rows = cur.rows.to_vec();
            for &(idx, seq) in &ops {
                if let Some(r) = rows.get_mut(idx) {
                    r.modify = Some(Modify {
                        kind: ModifyKind::Busy,
                        seq,
                        lingered: false,
                        refreshed: false,
                    });
                }
            }
            Arc::new(MachineListState { rows: rows.into() })
        });
        redraw.notify_one();

        for (idx, seq) in ops {
            let Some(machine) = self.state.load().rows.get(idx).map(|r| r.machine.clone()) else {
                continue;
            };
            let tx = tx.clone();
            let sem = sem.clone();
            let demo = self.demo;
            let child = cancel.child_token();
            tokio::spawn(async move {
                let _permit = match sem.acquire_owned().await {
                    Ok(p) => p,
                    Err(_) => return,
                };
                tokio::select! {
                    _ = child.cancelled() => {}
                    result = async {
                        match action {
                            Action::Power(a) => bmc::power_action(&machine, demo, a).await,
                            Action::Boot(a) => bmc::set_boot(&machine, demo, a, persistent).await,
                        }
                    } => {
                        let _ = tx.send(Update::ActionDone { idx, seq, action, result }).await;
                    }
                }
            });
        }
    }

    fn modify_seq(&self, idx: usize) -> Option<u64> {
        self.state
            .load()
            .rows
            .get(idx)
            .and_then(|r| r.modify.map(|m| m.seq))
    }

    fn set_modify(&self, idx: usize, modify: Option<Modify>) {
        self.state.rcu(|cur| {
            let mut rows = cur.rows.to_vec();
            if let Some(r) = rows.get_mut(idx) {
                r.modify = modify;
            }
            Arc::new(MachineListState { rows: rows.into() })
        });
    }

    fn set_power(&self, idx: usize, power: PowerState) {
        self.state.rcu(|cur| {
            let mut rows = cur.rows.to_vec();
            if let Some(r) = rows.get_mut(idx) {
                r.power = power;
            }
            Arc::new(MachineListState { rows: rows.into() })
        });
    }

    fn set_boot_override(&self, idx: usize, boot: BootOverride) {
        self.state.rcu(|cur| {
            let mut rows = cur.rows.to_vec();
            if let Some(r) = rows.get_mut(idx) {
                r.boot = boot;
            }
            Arc::new(MachineListState { rows: rows.into() })
        });
    }

    // --- polling -----------------------------------------------------------

    fn apply_power(&self, idx: usize, poll: bmc::PowerPoll) {
        if let Some(row) = self.state.load().rows.get(idx).cloned() {
            if !poll.reachable {
                if let Some(err) = &poll.error {
                    tracing::debug!("poll of {} unreachable: {err}", row.machine.name);
                }
            } else if let (Some(cfg), Some(obs)) = (&row.machine.mac, &poll.mac)
                && norm_mac(cfg) != norm_mac(obs)
            {
                tracing::warn!(
                    "MAC mismatch for {}: config {cfg} != BMC {obs} (wrong host/MAC mapping?)",
                    row.machine.name
                );
            }
        }

        self.state.rcu(|cur| {
            let mut rows = cur.rows.to_vec();
            if let Some(r) = rows.get_mut(idx) {
                if poll.reachable {
                    if let Some(s) = poll.serial.clone() {
                        r.serial = Some(s);
                    }
                    if let Some(m) = poll.mac.clone() {
                        r.observed_mac = Some(m);
                    }
                    r.reachable = Some(true);
                    r.power = poll.power;
                    r.latency = poll.latency;
                    r.fault = poll.fault;
                    r.identify = poll.identify;
                    r.boot = poll.boot;
                } else {
                    r.reachable = Some(false);
                }
                // A completed poll is the post-action "refresh" we wait for
                // before lifting a finished modify status.
                if let Some(m) = r.modify
                    && matches!(m.kind, ModifyKind::Done | ModifyKind::Failed)
                    && !m.refreshed
                {
                    r.modify = Some(Modify {
                        refreshed: true,
                        ..m
                    });
                }
            }
            Arc::new(MachineListState { rows: rows.into() })
        });
        self.clear_modify_if_ready(idx);
    }

    /// Mark the linger timer as elapsed for the matching modify generation.
    fn mark_lingered(&self, idx: usize, seq: u64) {
        self.state.rcu(|cur| {
            let mut rows = cur.rows.to_vec();
            if let Some(r) = rows.get_mut(idx)
                && let Some(m) = r.modify
                && m.seq == seq
                && !m.lingered
            {
                r.modify = Some(Modify {
                    lingered: true,
                    ..m
                });
            }
            Arc::new(MachineListState { rows: rows.into() })
        });
    }

    /// Clear a finished modify status once both conditions hold: the linger
    /// timer elapsed AND a fresh poll has landed.
    fn clear_modify_if_ready(&self, idx: usize) {
        let ready = self
            .state
            .load()
            .rows
            .get(idx)
            .and_then(|r| r.modify)
            .map(|m| m.lingered && m.refreshed)
            .unwrap_or(false);
        if ready {
            self.set_modify(idx, None);
        }
    }

    fn apply_health(&self, idx: usize, health: Option<Health>) {
        let Some(h) = health else { return };
        self.state.rcu(|cur| {
            let mut rows = cur.rows.to_vec();
            if let Some(r) = rows.get_mut(idx) {
                r.health = h;
            }
            Arc::new(MachineListState { rows: rows.into() })
        });
    }

    fn spawn_power_poll(
        &self,
        idx: usize,
        machine: Machine,
        fetch_static: bool,
        tx: &mpsc::Sender<Update>,
        sem: &Arc<Semaphore>,
        cancel: &CancellationToken,
    ) {
        let tx = tx.clone();
        let sem = sem.clone();
        let demo = self.demo;
        let child = cancel.child_token();
        tokio::spawn(async move {
            let _permit = match sem.acquire_owned().await {
                Ok(p) => p,
                Err(_) => return,
            };
            tokio::select! {
                _ = child.cancelled() => {}
                poll = bmc::poll_power(&machine, demo, fetch_static) => {
                    let _ = tx.send(Update::Power { idx, poll }).await;
                }
            }
        });
    }

    fn needs_static(row: &MachineRow) -> bool {
        row.serial.is_none() || (row.machine.mac.is_some() && row.observed_mac.is_none())
    }

    fn poll_all_power(
        &self,
        tx: &mpsc::Sender<Update>,
        sem: &Arc<Semaphore>,
        cancel: &CancellationToken,
    ) {
        let rows = self.state.load().rows.clone();
        for (idx, row) in rows.iter().enumerate() {
            self.spawn_power_poll(
                idx,
                row.machine.clone(),
                Self::needs_static(row),
                tx,
                sem,
                cancel,
            );
        }
    }

    fn poll_one_power(
        &self,
        idx: usize,
        tx: &mpsc::Sender<Update>,
        sem: &Arc<Semaphore>,
        cancel: &CancellationToken,
    ) {
        if let Some(row) = self.state.load().rows.get(idx).cloned() {
            self.spawn_power_poll(
                idx,
                row.machine.clone(),
                Self::needs_static(&row),
                tx,
                sem,
                cancel,
            );
        }
    }

    fn poll_all_health(
        &self,
        tx: &mpsc::Sender<Update>,
        sem: &Arc<Semaphore>,
        cancel: &CancellationToken,
    ) {
        let rows = self.state.load().rows.clone();
        for (idx, row) in rows.iter().enumerate() {
            let machine = row.machine.clone();
            let tx = tx.clone();
            let sem = sem.clone();
            let demo = self.demo;
            let child = cancel.child_token();
            tokio::spawn(async move {
                let _permit = match sem.acquire_owned().await {
                    Ok(p) => p,
                    Err(_) => return,
                };
                tokio::select! {
                    _ = child.cancelled() => {}
                    health = bmc::poll_health(&machine, demo) => {
                        let _ = tx.send(Update::Health { idx, health }).await;
                    }
                }
            });
        }
    }

    fn render_row(r: &MachineRow) -> Row<'static> {
        let m = &r.machine;
        let mismatch = r.mac_mismatch();

        let marker = if r.marked {
            Cell::from(Span::styled("▶", Style::new().fg(TEAL)))
        } else {
            Cell::from(" ")
        };
        let name = if r.marked {
            Cell::from(Span::styled(m.name.clone(), Style::new().fg(TEAL)))
        } else {
            Cell::from(m.name.clone())
        };
        let proto = Cell::from(m.protocol.to_string());
        let host = Cell::from(m.host.clone());
        let mac_text = m.mac.clone().unwrap_or_else(|| "—".to_string());
        let mac = if mismatch {
            Cell::from(Span::styled(mac_text, Style::new().fg(Color::Red)))
        } else {
            Cell::from(mac_text)
        };
        let serial_text = r.serial.clone().unwrap_or_else(|| "—".to_string());
        let serial = Cell::from(truncate_start(&serial_text, SERIAL_W as usize));

        let (power, health) = match r.reachable {
            Some(true) => (power_cell(r.power), health_cell(r.health)),
            Some(false) => (Cell::from("—"), Cell::from("—")),
            None => (Cell::from("…"), Cell::from("…")),
        };

        Row::new(vec![
            marker,
            name,
            proto,
            host,
            mac,
            serial,
            power,
            health,
            flags_cell(r),
            boot_cell(r),
            state_cell(r, mismatch),
        ])
    }
}

impl Component for MachineList {
    type Msg = MachineListMsg;

    async fn run(
        self: Arc<Self>,
        mut event_rx: mpsc::Receiver<Event>,
        msg_tx: mpsc::Sender<MachineListMsg>,
        redraw: Arc<Notify>,
        cancel: CancellationToken,
    ) {
        let (tx, mut rx) = mpsc::channel::<Update>(64);
        let sem = Arc::new(Semaphore::new(MAX_CONCURRENT_POLLS));

        self.poll_all_power(&tx, &sem, &cancel);
        self.poll_all_health(&tx, &sem, &cancel);

        let mut power_iv = tokio::time::interval(self.power_interval);
        power_iv.tick().await;
        let mut health_iv = tokio::time::interval(self.health_interval);
        health_iv.tick().await;

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
                    let release = k.kind == KeyEventKind::Release;
                    let ui = self.ui.load();

                    if let Some(hold) = ui.hold {
                        let held_key = k.code == hold.key;
                        if release && held_key {
                            // Clean cancel where the terminal reports key releases.
                            self.cancel_hold(&redraw);
                        } else if press && k.code == KeyCode::Esc {
                            self.close_modal();
                            redraw.notify_one();
                        } else if (press || repeat) && held_key {
                            // Auto-repeat heartbeat: the key is still held.
                            self.beat();
                        }
                    } else if let Some(modal) = ui.modal.as_ref() {
                        // Copy out before any self.* call (which restores `ui`).
                        let actions = modal.kind.actions();
                        let cursor = modal.cursor.min(actions.len().saturating_sub(1));
                        if press || repeat {
                            match k.code {
                                KeyCode::Down | KeyCode::Char('j') => self.move_modal_cursor(1, &redraw),
                                KeyCode::Up | KeyCode::Char('k') => self.move_modal_cursor(-1, &redraw),
                                // Enter confirms the cursor row (sustained by Enter).
                                KeyCode::Enter if press => {
                                    let action = actions[cursor];
                                    self.begin_action(action, KeyCode::Enter, &tx, &sem, &cancel, &redraw);
                                }
                                KeyCode::Esc if press => {
                                    self.close_modal();
                                    redraw.notify_one();
                                }
                                // Boot: toggle once ⇄ persistent.
                                KeyCode::Char('t') if press => self.toggle_persistent(&redraw),
                                // A hotkey both selects its row and starts its hold.
                                KeyCode::Char(ch) if press => {
                                    if let Some(i) = actions.iter().position(|&a| action_char(a) == ch) {
                                        self.set_modal_cursor(i);
                                        self.begin_action(
                                            actions[i],
                                            KeyCode::Char(ch),
                                            &tx,
                                            &sem,
                                            &cancel,
                                            &redraw,
                                        );
                                    }
                                    // else: consumed (no leak to parent).
                                }
                                // Modal consumes every other key (no leak to parent).
                                _ => {}
                            }
                        }
                    } else if self.suppress_after_fire(k.code) {
                        // Lingering auto-repeat of the key that just confirmed a
                        // hold — swallow it so it doesn't act on the list.
                    } else {
                        match k.code {
                            KeyCode::Down | KeyCode::Char('j') if press || repeat => {
                                self.select_next();
                                redraw.notify_one();
                            }
                            KeyCode::Up | KeyCode::Char('k') if press || repeat => {
                                self.select_prev();
                                redraw.notify_one();
                            }
                            KeyCode::Char(' ') if press => {
                                self.toggle_mark();
                                redraw.notify_one();
                            }
                            KeyCode::Char('p') if press => {
                                self.open_modal(ModalKind::Power);
                                redraw.notify_one();
                            }
                            KeyCode::Char('b') if press => {
                                self.open_modal(ModalKind::Boot);
                                redraw.notify_one();
                            }
                            KeyCode::Char('r') if press => {
                                self.poll_all_power(&tx, &sem, &cancel);
                                self.poll_all_health(&tx, &sem, &cancel);
                            }
                            KeyCode::Enter if press => {
                                self.open_detail(DetailTab::Overview, &msg_tx).await;
                            }
                            KeyCode::Char('u') if press => {
                                self.open_detail(DetailTab::Users, &msg_tx).await;
                            }
                            KeyCode::Char('l') if press => {
                                self.open_detail(DetailTab::Logs, &msg_tx).await;
                            }
                            KeyCode::Char('c') if press => self.launch_console(),
                            _ => {}
                        }
                    }
                }
                Some(update) = rx.recv() => {
                    match update {
                        Update::Power { idx, poll } => self.apply_power(idx, poll),
                        Update::Health { idx, health } => self.apply_health(idx, health),
                        Update::HoldTick { seq } => self.advance_hold(seq, &tx, &sem, &cancel, &redraw),
                        Update::ActionDone { idx, seq, action, result } => {
                            if self.modify_seq(idx) == Some(seq) {
                                let kind = match result {
                                    Ok(()) => {
                                        match action {
                                            Action::Power(a) => {
                                                if let Some(p) = a.optimistic_power() {
                                                    self.set_power(idx, p);
                                                }
                                            }
                                            Action::Boot(a) => {
                                                self.set_boot_override(idx, a.optimistic_boot());
                                            }
                                        }
                                        ModifyKind::Done
                                    }
                                    Err(e) => {
                                        tracing::warn!("{}: {} failed: {e}", self.row_name(idx), action_label(action));
                                        ModifyKind::Failed
                                    }
                                };
                                self.set_modify(idx, Some(Modify { kind, seq, lingered: false, refreshed: false }));
                                // Refresh so the result reflects real state, and
                                // start the linger timer; the status clears only
                                // once BOTH have happened.
                                self.poll_one_power(idx, &tx, &sem, &cancel);
                                let tx2 = tx.clone();
                                let child = cancel.child_token();
                                tokio::spawn(async move {
                                    tokio::select! {
                                        _ = child.cancelled() => {}
                                        _ = tokio::time::sleep(MODIFY_LINGER) => {
                                            let _ = tx2.send(Update::ModifyExpire { idx, seq }).await;
                                        }
                                    }
                                });
                            }
                        }
                        Update::ModifyExpire { idx, seq } => {
                            self.mark_lingered(idx, seq);
                            self.clear_modify_if_ready(idx);
                        }
                    }
                    redraw.notify_one();
                }
                _ = power_iv.tick() => self.poll_all_power(&tx, &sem, &cancel),
                _ = health_iv.tick() => self.poll_all_health(&tx, &sem, &cancel),
            }
        }
    }

    fn render(&self, area: Rect, frame: &mut Frame) {
        let snap = self.state.load();

        let header = Row::new([
            "", "Name", "Proto", "BMC Host", "MAC", "Serial", "Power", "Health", "Flags", "Boot",
            "State",
        ])
        .style(Style::new().add_modifier(Modifier::BOLD));

        let rows: Vec<Row> = snap.rows.iter().map(MachineList::render_row).collect();

        let widths = [
            Constraint::Length(1),        // selection marker
            Constraint::Min(20),          // Name
            Constraint::Length(7),        // Proto
            Constraint::Length(15),       // BMC Host
            Constraint::Length(17),       // MAC
            Constraint::Length(SERIAL_W), // Serial
            Constraint::Length(5),        // Power
            Constraint::Length(5),        // Health
            Constraint::Length(6),        // Flags
            Constraint::Length(5),        // Boot
            Constraint::Length(12),       // State
        ];

        let table = Table::new(rows, widths)
            .header(header)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" Machines ({}) ", snap.rows.len())),
            )
            .row_highlight_style(
                Style::new()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            );

        let mut st = self.selected.lock().unwrap();
        frame.render_stateful_widget(table, area, &mut st);
        drop(st);

        let ui = self.ui.load();
        if ui.modal.is_some() {
            render_modal(&ui, self.enhanced, area, frame);
        }
    }
}

/// Draw the centered power/boot modal: a cursor list with per-row hotkeys
/// (`> [k] label`). On enhanced terminals confirming is a press-and-hold fill
/// animation — hold a row's hotkey, or move the `>` cursor with ↑/↓ and hold
/// Enter; on legacy terminals a single hotkey/Enter commits immediately.
fn render_modal(ui: &UiState, enhanced: bool, area: Rect, frame: &mut Frame) {
    let Some(modal) = &ui.modal else { return };
    // Boot shows an extra "Apply:" line for the persistence toggle.
    let is_boot = matches!(modal.kind, ModalKind::Boot);
    let rect = centered_rect(46, if is_boot { 12 } else { 11 }, area);
    frame.render_widget(Clear, rect);

    let title = if is_boot {
        format!(
            " Boot — {} ",
            if modal.persistent {
                "Persistent"
            } else {
                "Once"
            }
        )
    } else {
        modal.kind.title().to_string()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::new().fg(TEAL));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let mut lines = vec![Line::from(vec![
        Span::styled("Target: ", Style::new().add_modifier(Modifier::BOLD)),
        Span::raw(modal.label.clone()),
    ])];
    if is_boot {
        let (mode, other, color) = if modal.persistent {
            ("Persistent", "once", Color::Yellow)
        } else {
            ("Once", "persistent", Color::Green)
        };
        lines.push(Line::from(vec![
            Span::styled("Apply:  ", Style::new().add_modifier(Modifier::BOLD)),
            Span::styled(mode, Style::new().fg(color).add_modifier(Modifier::BOLD)),
            Span::raw("   "),
            Span::styled("[t]", Style::new().fg(TEAL)),
            Span::styled(format!(" {other}"), Style::new().fg(Color::DarkGray)),
        ]));
    }
    lines.push(Line::raw(""));
    let bar_w = inner.width as usize;
    for (i, &a) in modal.kind.actions().iter().enumerate() {
        let held_frac = match ui.hold {
            Some(h) if h.action == a => Some(
                (h.started.elapsed().as_secs_f32() / HOLD_DURATION.as_secs_f32()).clamp(0.0, 1.0),
            ),
            _ => None,
        };
        let selected = i == modal.cursor;
        lines.push(action_line(a, selected, held_frac, bar_w));
    }
    lines.push(Line::raw(""));
    let hint = if enhanced {
        "↑/↓ select · hold hotkey/Enter · Esc cancel"
    } else {
        "↑/↓ select · hotkey/Enter confirm · Esc"
    };
    lines.push(Line::from(Span::styled(
        hint,
        Style::new().fg(Color::DarkGray),
    )));

    frame.render_widget(Paragraph::new(lines), inner);
}

/// One modal action row: `> [k] label`, with `>` on the cursor row and the
/// hotkey `[k]` coloured by action. `held_frac` is `Some` while this action is
/// being held (drives the full-width fill bar).
fn action_line(a: Action, selected: bool, held_frac: Option<f32>, width: usize) -> Line<'static> {
    let color = action_color(a);
    let marker = if selected { "> " } else { "  " };
    let text = format!("{marker}[{}] {}", action_char(a), action_short(a));

    if let Some(frac) = held_frac {
        // The action colour fills behind the text across the full modal width.
        // Split on a char boundary (multibyte glyphs), not a byte index.
        let field = format!("{text:<width$}");
        let total = field.chars().count();
        let fill = ((frac * width as f32).round() as usize).min(total);
        let filled: String = field.chars().take(fill).collect();
        let rest: String = field.chars().skip(fill).collect();
        return Line::from(vec![
            Span::styled(filled, Style::new().bg(color).fg(Color::Black)),
            Span::styled(rest, Style::new().fg(Color::White)),
        ]);
    }

    let marker_style = if selected {
        Style::new().fg(TEAL).add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    };
    let mut label_style = Style::new().fg(Color::White);
    if selected {
        label_style = label_style.add_modifier(Modifier::BOLD);
    }
    Line::from(vec![
        Span::styled(marker.to_string(), marker_style),
        Span::styled(format!("[{}]", action_char(a)), Style::new().fg(color)),
        Span::styled(format!(" {}", action_short(a)), label_style),
    ])
}

/// The hotkey letter for an action — shown as `[k]` and used to select+hold.
fn action_char(a: Action) -> char {
    match a {
        Action::Power(PowerAction::On) => 'o',
        Action::Power(PowerAction::SoftOff) => 's',
        Action::Power(PowerAction::ForceOff) => 'f',
        Action::Power(PowerAction::Cycle) => 'c',
        Action::Power(PowerAction::Reset) => 'r',
        Action::Boot(BootAction::Pxe) => 'p',
        Action::Boot(BootAction::Disk) => 'd',
        Action::Boot(BootAction::Bios) => 'b',
        Action::Boot(BootAction::Cd) => 'c',
        Action::Boot(BootAction::Clear) => 'n',
    }
}

fn action_label(a: Action) -> &'static str {
    match a {
        Action::Power(p) => p.label(),
        Action::Boot(b) => b.label(),
    }
}

fn action_short(a: Action) -> &'static str {
    match a {
        Action::Power(PowerAction::On) => "on",
        Action::Power(PowerAction::SoftOff) => "soft-off",
        Action::Power(PowerAction::ForceOff) => "force-off",
        Action::Power(PowerAction::Cycle) => "cycle (cold)",
        Action::Power(PowerAction::Reset) => "reset (warm)",
        Action::Boot(BootAction::Pxe) => "pxe (network)",
        Action::Boot(BootAction::Disk) => "disk",
        Action::Boot(BootAction::Bios) => "bios setup",
        Action::Boot(BootAction::Cd) => "cd / dvd",
        Action::Boot(BootAction::Clear) => "no override",
    }
}

fn action_color(a: Action) -> Color {
    match a {
        Action::Power(PowerAction::On) => Color::Green,
        Action::Power(PowerAction::SoftOff) => Color::Yellow,
        Action::Power(PowerAction::ForceOff) => Color::Red,
        Action::Power(PowerAction::Cycle) => TEAL,
        Action::Power(PowerAction::Reset) => TEAL,
        Action::Boot(BootAction::Pxe) => Color::Yellow,
        Action::Boot(BootAction::Clear) => Color::DarkGray,
        Action::Boot(_) => Color::White,
    }
}

fn centered_rect(w: u16, h: u16, area: Rect) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn power_cell(p: PowerState) -> Cell<'static> {
    let (txt, color) = match p {
        PowerState::On => ("on", Color::Green),
        PowerState::Off => ("off", Color::Red),
        PowerState::Unknown => ("?", Color::DarkGray),
    };
    Cell::from(Span::styled(txt, Style::new().fg(color)))
}

fn health_cell(h: Health) -> Cell<'static> {
    let (txt, color) = match h {
        Health::Ok => ("OK", Color::Green),
        Health::Warning => ("WARN", Color::Yellow),
        Health::Critical => ("CRIT", Color::Red),
        Health::Unknown => ("?", Color::DarkGray),
    };
    Cell::from(Span::styled(txt, Style::new().fg(color)))
}

fn flags_cell(r: &MachineRow) -> Cell<'static> {
    if r.reachable != Some(true) {
        return Cell::from("—");
    }
    if r.fault {
        Cell::from(Span::styled(
            "FAULT",
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ))
    } else if r.identify {
        Cell::from(Span::styled("ID", Style::new().fg(Color::Cyan)))
    } else {
        Cell::from(Span::styled("·", Style::new().fg(Color::DarkGray)))
    }
}

fn boot_cell(r: &MachineRow) -> Cell<'static> {
    if r.reachable != Some(true) {
        return Cell::from("—");
    }
    let color = if r.boot == BootOverride::Pxe {
        Color::Yellow
    } else if r.boot.is_override() {
        Color::White
    } else {
        Color::DarkGray
    };
    Cell::from(Span::styled(r.boot.short(), Style::new().fg(color)))
}

/// The "State" column. A power modification (modifying…/✓/✗) takes over while
/// active, hiding the live sync state; a MAC mismatch otherwise wins over ok/slow.
fn state_cell(r: &MachineRow, mac_mismatch: bool) -> Cell<'static> {
    if let Some(m) = &r.modify {
        let (txt, color) = match m.kind {
            ModifyKind::Busy => ("modifying…", Color::Yellow),
            ModifyKind::Done => ("modifying ✓", Color::Green),
            ModifyKind::Failed => ("modify ✗", Color::Red),
        };
        return Cell::from(Span::styled(
            txt,
            Style::new().fg(color).add_modifier(Modifier::BOLD),
        ));
    }

    let (txt, color, bold) = match r.reachable {
        None => ("polling", Color::Yellow, false),
        Some(false) => ("unreachable", Color::Red, false),
        Some(true) if mac_mismatch => ("MAC MISMATCH", Color::Red, true),
        Some(true) if r.latency > SLOW_THRESHOLD => ("slow", Color::Yellow, false),
        Some(true) => ("ok", Color::Green, false),
    };
    let mut style = Style::new().fg(color);
    if bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    Cell::from(Span::styled(txt, style))
}

fn truncate_start(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let tail: String = s.chars().skip(n - keep).collect();
    format!("…{tail}")
}

fn norm_mac(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_hexdigit())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}
