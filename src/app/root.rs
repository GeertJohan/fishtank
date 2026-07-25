//! Root component: hosts the machine list and an optional per-machine detail
//! view, swapping between them.
//!
//! The [`MachineList`] is mounted once and kept alive for the whole session (it
//! keeps polling in the background). When the list emits
//! [`MachineListMsg::OpenDetail`], a [`MachineDetail`] child is spawned and
//! becomes the active view; when it emits [`MachineDetailMsg::Close`] (or its
//! task ends) we tear it down and fall back to the list. AppRoot owns global
//! key handling (Ctrl+C / q to quit), forwards other input to the active child,
//! and draws a one-line header above the body.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use arc_swap::ArcSwapOption;
use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

use crate::inventory::Inventory;
use crate::ratata::{ChildHandle, Component, ConsoleRequest, Event, KeyboardMode, spawn_child};

use super::machine_detail::{MachineDetail, MachineDetailMsg};
use super::machine_list::{MachineList, MachineListMsg};

/// AppRoot has no upward messages — it's the root.
pub type AppMsg = ();

pub struct AppRoot {
    keyboard_mode: KeyboardMode,
    inventory: Inventory,
    demo: bool,
    /// Sender for console (SOL) suspend-and-exec requests, handed to the list.
    console_tx: mpsc::Sender<ConsoleRequest>,
    /// Arcs kept so `render` can reach whichever view is active.
    machine_list: ArcSwapOption<MachineList>,
    machine_detail: ArcSwapOption<MachineDetail>,
    /// True while the detail view is the active (drawn / focused) body.
    showing_detail: AtomicBool,
}

impl AppRoot {
    pub fn new(
        keyboard_mode: KeyboardMode,
        inventory: Inventory,
        demo: bool,
        console_tx: mpsc::Sender<ConsoleRequest>,
    ) -> Arc<Self> {
        Arc::new(Self {
            keyboard_mode,
            inventory,
            demo,
            console_tx,
            machine_list: ArcSwapOption::empty(),
            machine_detail: ArcSwapOption::empty(),
            showing_detail: AtomicBool::new(false),
        })
    }
}

/// Await the optional detail child's next message, or never if there's none.
/// `None` means the detail task ended (treated the same as an explicit Close).
async fn detail_msg(detail: &mut Option<ChildHandle<MachineDetail>>) -> Option<MachineDetailMsg> {
    match detail {
        Some(h) => Some(h.msg_rx.recv().await.unwrap_or(MachineDetailMsg::Close)),
        None => std::future::pending().await,
    }
}

impl Component for AppRoot {
    type Msg = AppMsg;

    async fn run(
        self: Arc<Self>,
        mut event_rx: mpsc::Receiver<Event>,
        _msg_tx: mpsc::Sender<AppMsg>,
        redraw: Arc<Notify>,
        cancel: CancellationToken,
    ) {
        // Mount the machine list (lives for the whole session).
        let enhanced = matches!(self.keyboard_mode, KeyboardMode::Enhanced);
        let list = spawn_child(
            MachineList::new(
                self.inventory.clone(),
                self.demo,
                enhanced,
                self.console_tx.clone(),
            ),
            redraw.clone(),
            &cancel,
        );
        self.machine_list.store(Some(list.component.clone()));
        let list_events = list.event_tx;
        let mut list_msgs = list.msg_rx;
        redraw.notify_one();

        // The detail view, when open.
        let mut detail: Option<ChildHandle<MachineDetail>> = None;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                maybe_ev = event_rx.recv() => {
                    let Some(ev) = maybe_ev else { break };
                    let detail_active = self.showing_detail.load(Ordering::Relaxed);
                    // The list's power/boot modal captures input; don't act on
                    // globals then. (Irrelevant while the detail view is up.)
                    let capturing = !detail_active
                        && self
                            .machine_list
                            .load_full()
                            .map(|ml| ml.is_capturing())
                            .unwrap_or(false);
                    if let Event::Key(k) = &ev
                        && k.kind == KeyEventKind::Press
                        && !capturing
                    {
                        let ctrl_c = k.code == KeyCode::Char('c')
                            && k.modifiers.contains(KeyModifiers::CONTROL);
                        // Ctrl+C always quits; bare `q` only on the list view
                        // (in the detail view it's forwarded and means "back").
                        let q_quit = k.code == KeyCode::Char('q') && !detail_active;
                        if ctrl_c || q_quit {
                            break;
                        }
                    }
                    if matches!(ev, Event::Resize(_, _)) {
                        redraw.notify_one();
                    }
                    // Trickle down to the active view.
                    if detail_active {
                        if let Some(d) = &detail {
                            let _ = d.event_tx.send(ev).await;
                        }
                    } else {
                        let _ = list_events.send(ev).await;
                    }
                }
                msg = list_msgs.recv() => {
                    match msg {
                        None => break, // list task ended
                        Some(MachineListMsg::OpenDetail(machine, tab)) => {
                            // Replace any existing detail child.
                            if let Some(old) = detail.take() {
                                old.cancel.cancel();
                            }
                            let child = spawn_child(
                                MachineDetail::new(machine, self.demo, tab),
                                redraw.clone(),
                                &cancel,
                            );
                            self.machine_detail.store(Some(child.component.clone()));
                            detail = Some(child);
                            self.showing_detail.store(true, Ordering::Relaxed);
                            redraw.notify_one();
                        }
                    }
                }
                dmsg = detail_msg(&mut detail) => {
                    // Close (explicit or task-ended): tear down, back to list.
                    let _ = dmsg;
                    if let Some(d) = detail.take() {
                        d.cancel.cancel();
                    }
                    self.machine_detail.store(None);
                    self.showing_detail.store(false, Ordering::Relaxed);
                    redraw.notify_one();
                }
            }
        }

        if let Some(d) = detail.take() {
            d.cancel.cancel();
        }
        list.cancel.cancel();
    }

    fn render(&self, area: Rect, frame: &mut Frame) {
        let [header, body] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);

        let detail_active = self.showing_detail.load(Ordering::Relaxed);
        let hint = if detail_active {
            "  F1/F2/F3 tabs · j/k scroll · r refresh · q/Esc back · ^C quit"
        } else {
            "  j/k nav · Space sel · p power · b boot · Enter detail · u users · l logs · c console · q quit"
        };
        let title = Line::from(vec![
            Span::styled(
                " fishtank ",
                Style::new()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(hint),
        ]);
        frame.render_widget(Paragraph::new(title), header);

        if detail_active && let Some(d) = self.machine_detail.load_full() {
            d.render(body, frame);
            return;
        }
        if let Some(ml) = self.machine_list.load_full() {
            ml.render(body, frame);
        }
    }
}
