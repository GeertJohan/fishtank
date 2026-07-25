use std::sync::Arc;

use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{Component, Event};

/// Default channel capacity. Large enough to absorb input bursts (a few KB
/// pasted at once) without backpressuring; small enough that a runaway
/// producer surfaces quickly via `try_send` failures.
const DEFAULT_CHANNEL_CAP: usize = 64;

/// Everything a parent needs to interact with a spawned child.
///
/// Includes the `Arc<C>` clone the parent keeps for itself — `component` is
/// the same shared instance the spawned task runs against, so the parent can
/// call `child.component.render(area, frame)`, peek getters, etc.
///
/// Channel ends are flipped from the child's perspective: the parent holds
/// the [`mpsc::Sender`] for events going down (`event_tx`) and the
/// [`mpsc::Receiver`] for messages coming up (`msg_rx`).
///
/// Cancellation: `cancel` is a child of the `parent_cancel` token passed to
/// [`spawn_child`], so it fires automatically when the parent's tree is torn
/// down. The parent can also call `cancel.cancel()` directly to drop just
/// this subtree (e.g., on a view-swap).
///
/// `join` is exposed for explicit shutdown sequencing in tests. In normal
/// operation, dropping the handle and letting cancellation propagate is
/// sufficient.
pub struct ChildHandle<C: Component> {
    pub component: Arc<C>,
    pub event_tx: mpsc::Sender<Event>,
    pub msg_rx: mpsc::Receiver<C::Msg>,
    pub cancel: CancellationToken,
    pub join: JoinHandle<()>,
}

/// Spawn a component as its own tokio task. The component must be wrapped
/// in `Arc` already (typical pattern: `MyComponent::new()` returns
/// `Arc<Self>`); `spawn_child` clones the Arc into both the task and the
/// returned [`ChildHandle`].
pub fn spawn_child<C: Component>(
    component: Arc<C>,
    redraw: Arc<Notify>,
    parent_cancel: &CancellationToken,
) -> ChildHandle<C> {
    let (event_tx, event_rx) = mpsc::channel(DEFAULT_CHANNEL_CAP);
    let (msg_tx, msg_rx) = mpsc::channel(DEFAULT_CHANNEL_CAP);
    let cancel = parent_cancel.child_token();

    let task_arc = component.clone();
    let join = tokio::spawn(task_arc.run(event_rx, msg_tx, redraw, cancel.clone()));

    ChildHandle {
        component,
        event_tx,
        msg_rx,
        cancel,
        join,
    }
}
