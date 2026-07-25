//! ratata — actor-style component framework on top of ratatui.
//!
//! Each component runs as its own tokio task with full async freedom. State
//! is published into an [`arc_swap::ArcSwap`] slot, render reads the latest
//! snapshot lock-free, and the render loop only fires when something requests
//! a redraw (FPS is a cap, not a tick).
//!
//! Communication paradigms:
//! - **Messages** (child → parent): typed `Self::Msg`, sent on an mpsc the
//!   child holds and the parent reads via [`ChildHandle::msg_rx`].
//! - **Events** (parent → child, trickle-down): the parent decides whether to
//!   handle an input event or forward it to a focused child via
//!   [`ChildHandle::event_tx`].
//! - **State snapshots** (component → renderer): `Arc<ArcSwap<State>>`,
//!   lock-free read, atomic write.
//! - **Redraw signals** (anyone → renderer): shared `Arc<Notify>`, coalesced.
//! - **Cancellation** (runtime → tree): `CancellationToken` propagated down at
//!   spawn time via `parent_cancel.child_token()`.
//!
//! See [`Component`] for the trait and [`Runtime`] for the entry point.

mod component;
mod event;
mod runtime;
mod spawn;
mod tui;

pub use component::Component;
pub use event::Event;
pub use runtime::{ConsoleRequest, Runtime};
// `ChildHandle` is part of ratata's public API (the return type of
// `spawn_child`), even though callers currently bind it without naming the type.
#[allow(unused_imports)]
pub use spawn::{ChildHandle, spawn_child};
pub use tui::{KeyboardMode, Tui};
