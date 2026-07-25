use std::future::Future;
use std::sync::Arc;

use ratatui::{Frame, layout::Rect};
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

use super::Event;

/// A node in the component tree.
///
/// Each component is shared via `Arc<Self>` from construction. The component
/// task gets one clone of the Arc (consumed by `run`); the parent and the
/// renderer hold additional clones. All access goes through `&self`, so state
/// mutation uses interior mutability — the convention is
/// `field: ArcSwap<MyState>`, mutated via `state.rcu(…)` or
/// `state.store(Arc::new(new))` and read via `state.load()`.
///
/// `Self::Msg` is what this component emits upward to its parent. Plural —
/// the same type fires many times over the component's lifetime.
///
/// Lifecycle:
/// - `run` is the task body. Runs until `cancel` fires or `run` returns.
/// - Spawn child tokio tasks with `cancel.child_token()` so they die when
///   this component does.
/// - Returning from `run` signals "I'm done" — the parent's `msg_rx.recv()`
///   will start returning `None`.
///
/// State communication:
/// - Messages emitted to the parent go through `msg_tx`.
/// - Input events arrive on `event_rx`. With trickle-down semantics, the
///   parent has already decided this event isn't its own and is forwarding
///   to the focused child.
/// - `render` paints the current state into a frame; called from the render
///   loop with whatever the latest `ArcSwap::load` returns.
/// - `redraw.notify_one()` wakes the render loop after state changes that
///   affect what's drawn.
pub trait Component: Send + Sync + 'static {
    /// Messages this component emits upward to its parent.
    type Msg: Send + 'static;

    /// The component's task body. Runs until `cancel` fires or the component
    /// returns naturally.
    ///
    /// Takes `Arc<Self>` rather than `Self` so the parent and renderer keep
    /// their own clones; `self` here is just one more Arc clone owned by the
    /// task. Use `self.clone()` to give Arc clones to any tasks you spawn
    /// from within `run`.
    fn run(
        self: Arc<Self>,
        event_rx: mpsc::Receiver<Event>,
        msg_tx: mpsc::Sender<Self::Msg>,
        redraw: Arc<Notify>,
        cancel: CancellationToken,
    ) -> impl Future<Output = ()> + Send + 'static;

    /// Paint the component's current state into the given area.
    ///
    /// Called from the render loop via the parent's Arc clone. Reads state
    /// fields directly through `&self` (typically via `field.load()` on an
    /// `ArcSwap`). Pure of mutation, except for deliberate interior-mutable
    /// counters / scroll states that are documented at the use site.
    fn render(&self, area: Rect, frame: &mut Frame);
}
