# ratata — design

ratata is an async-first component framework on top of [ratatui]. Each
component is a tokio task; communication happens through typed mpsc channels;
state is published as snapshots into an `ArcSwap`; rendering is on-demand,
not on a fixed tick.

This document captures *why* the framework looks the way it does. For the
trait shape itself, read the rustdoc on `Component`, `ChildHandle`, and
`Runtime`.

## The four required capabilities

The framework exists to provide four things that no other ratatui-ecosystem
crate (as of our 2026-05-24 survey) provides together:

1. **Component tree.** Parents own children; the tree is the structure.
2. **Key-event delegation along the focused path.** A keystroke reaches the
   right component based on focus, not by broadcast.
3. **Child → parent messages.** A child can tell its parent something
   ("user picked location X") without the parent polling.
4. **On-demand redraws.** No fixed render rate. The render loop sleeps until
   something requests a frame. A target FPS acts as an upper bound, not a
   target.

The closest existing crate is [`rat-salsa`] (hits all four). We're building
our own to (a) learn the patterns deeply, (b) keep flexibility for the
longer-term SSH-MMORPG target, where per-session overhead and customisability
matter.

## Async-first / actor-per-component

Each component runs as its own tokio task — `async fn run(self, …)` is the
task body and typically a `select!` loop over the component's inputs.

**Why.** The alternative is a synchronous design where the framework owns a
loop and calls `handle_event` / `handle_msg` methods on components. That
works fine for simple TUIs, but it makes spontaneous emission awkward:
anything that wants to push a message (a network packet arriving, an
animation tick, an HTTP response) has to route through framework-provided
machinery. With actor-per-component, every component already has its own
`select!`, so plugging in another source is just another arm.

**Cost.** Tokio tasks are ~64 bytes of overhead plus the future's state
(typically a few hundred bytes for our shapes). The runtime can comfortably
handle millions of tasks on a single machine — well past anything an SSH
game server would need.

**Footguns.**
- Forget the `cancel.cancelled()` arm in your `select!` and your component
  outlives its parent.
- Spawn a helper task without `cancel.child_token()` and it leaks work after
  the component is torn down.
- `await` a long-running call inline inside an event arm and the component
  freezes for the duration. Long work spawns; only short, non-blocking work
  inlines.

## Static composition; no `Box<dyn Component>`

Parents store children as concrete types or as enum variants of known
component types:

```rust
enum HomeBody {
    LocationList(LocationListView),
    LocationDetail(LocationDetailView),
}
```

**Why.** No heap allocation on view swap. No vtable. Method calls inline.
For an SSH game server multiplying every overhead by hundreds of concurrent
sessions, this matters. The price is verbosity — you write the enum and
match on it instead of holding a `Box<dyn Component>` slot.

Where the set of children is genuinely dynamic (a modal stack that any
component might push onto), `Box<dyn …>` is fine — use it there, locally,
not as the framework's default.

## Per-component typed messages (`type Msg`)

Each component declares its own `Msg` type. The child holds the `Sender`,
the parent holds the `Receiver`. The parent reads in its `select!` and
reacts; there is no message bubbling through the trait — each parent-child
pair has its own channel.

**Naming note.** We considered renaming `Msg` to `Emit`, `Out`, `Response`,
etc. to make the direction (always upward) explicit. `Response` was ruled
out by cardinality (a component emits many messages over its lifetime, not
one). The other candidates didn't add enough value over the conventional
"Msg" to justify diverging from precedent (Elm, Iced, rat-salsa).

## Components are `Arc<Self>`-shared; state lives on the component

The framework's [`Component`] trait has *no* `State` associated type and
*no* separate `Render` trait. State lives as a field on the component
struct (the convention is `state: ArcSwap<MyState>`), and rendering is a
`&self` method on the [`Component`] trait itself. The framework doesn't
construct or inspect state — it only knows about messages, events, redraw
signals, and cancellation.

The mechanism that ties this together: **components are `Arc<Self>`-shared
from construction**. `run(self: Arc<Self>, …)` takes one Arc clone (the
task's); the parent (and any spawned helper tasks) hold additional clones.
Because `&Component` is enough to call render, peek getters, or anything
else, the Arc *is* the handle — no separate type needed.

```rust
pub struct FpsCounter {
    state: ArcSwap<FpsCounterState>,
}

impl FpsCounter {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: ArcSwap::new(Arc::new(FpsCounterState::new())),
        })
    }

    pub fn current_fps(&self) -> f64 {
        self.state.load().fps
    }
}

impl Component for FpsCounter {
    type Msg = ();

    fn run(self: Arc<Self>, …) -> impl Future<…> + Send + 'static {
        async move {
            // self is Arc<Self>; mutate state via self.state.rcu(…)
        }
    }

    fn render(&self, area, frame) {
        let snap = self.state.load();
        // draw using `snap`
    }
}
```

The parent holds an `Arc<FpsCounter>` via [`ChildHandle::component`] and
can `child.component.render(area, frame)` or `child.component.current_fps()`
without any separate handle type.

Requires `Component: Send + Sync + 'static`. Sync is satisfied by ArcSwap /
AtomicU64 / Mutex — anything we'd use for interior mutability is already
Sync.

### Why `ArcSwap` rather than `RwLock`

Read locks (with parking_lot) are nearly free, but a writer in progress
blocks readers. For tight render budgets and unpredictable write latency
(a slow `Vec` rebuild blocks render for the duration), read and write
rates become coupled. ArcSwap decouples them: readers do an atomic pointer
load and never wait; writers pay a snapshot build cost (an allocation +
a CAS) regardless of read pressure.

### Clone cost

`rcu` / load-clone-store clones the whole state on every mutation. Two
mitigations:

- Wrap heavy fields in `Arc<[T]>` / `Arc<str>` / `Arc<HashMap<…>>` inside
  state. Cloning is then a few atomic ref bumps; bulk data is shared.
- Use `Arc<Mutex<T>>` for fields the renderer needs to mutate (e.g.
  `TreeState` from tui-tree-widget, whose render auto-scrolls). The Mutex
  is held briefly on both sides.

For genuinely large mutable state, future options include `im` (persistent
collections, O(1) clone) or sharding into multiple `ArcSwap<Substate>`
fields. v1 doesn't need either.

### Render reads, but may also write atomics

`Component::render(&self, area, frame)` takes `&self`, so the component is
immutable. But fields with interior mutability (`AtomicU64`, `Mutex<T>`)
can still change during render. We use this deliberately:

- `FpsCounter` bumps an `Arc<AtomicU64>` frame counter inside render. The
  task reads the counter every second to compute FPS.
- `TreeState`/`ListState` from the widget ecosystem are auto-scrolled by
  their stateful widgets during render. Wrap them in `Arc<Mutex<…>>`.

These exceptions are noted, not hidden — they're the only places component
state changes during render.

## Trickle-down event handling (not bubble-up)

When an input event arrives, the parent receives it first. The parent
inspects it, handles global shortcuts (Tab, Ctrl+C, etc.), and forwards
the rest down its child's `event_tx`. Children do the same recursively.

**Why not bubble-up?** Bubble-up reads naturally in a sync framework (the
deepest focused child returns "I didn't handle it", parent tries). In an
async framework, that would require the parent to *await* the child for a
"did you handle it?" response — serializing input processing and making
the parent block on the child.

Trickle-down inverts the dependency: the parent decides immediately, no
await. Global shortcuts get priority over local handlers (which matches
user intuition: Ctrl+Q wins over a text-input area).

## Cancellation propagation

`tokio_util::sync::CancellationToken::child_token()` builds the tree:

```rust
let child_cancel = parent_cancel.child_token();
tokio::spawn(child.run(…, child_cancel));
```

When the parent cancels (or is dropped if you call `.cancel()` first), all
descendants observe it via their own `cancel.cancelled()` arm and exit.
Long-running spawned tasks inside a component (HTTP calls, timers) should
use `cancel.child_token()` themselves so they die when the component does.

**Footgun.** Dropping a `CancellationToken` does NOT cancel — you must call
`.cancel()`. The `spawn_child` helper sets up the parent→child link, but
the parent must call `child.cancel.cancel()` on view swap. Dropping a
`ChildHandle` without cancelling leaks the child's task until its `run`
naturally returns.

## On-demand render via `tokio::sync::Notify`

The runtime holds one `Arc<Notify>`, shared with every component. Anyone
who wants a redraw calls `redraw.notify_one()`. The render loop does:

```rust
tokio::select! {
    _ = redraw.notified() => { /* throttle, then draw */ }
    Some(ev) = events.recv() => { /* forward to root */ }
    join = &mut root_handle.join => { /* root exited; quit */ }
}
```

`Notify::notify_one` coalesces: 100 calls between two `notified().await`
returns count as one wakeup. The render branch enforces the FPS cap by
sleeping for `frame_min - elapsed` before drawing if we've drawn recently.

**Idle = 0 CPU.** When nothing is changing, the runtime parks on `select!`
waiting for the next event. No tick, no busy loop. Verified empirically
during smoke testing.

## `Component` is not a ratatui `Widget`

Different abstractions:

| | ratatui `Widget` | ratata `Component` |
|---|---|---|
| Lifecycle | One render call, then dropped | Long-lived; runs in its own task; the `Arc<Self>` is the renderable |
| State | None (or external via StatefulWidget) | Owns state behind ArcSwap, on `&self` via Arc sharing |
| Input | Doesn't handle input | Receives events on `event_rx` |
| Render | `fn render(self, …)` (consuming) | `fn render(&self, area, frame)` on the trait |
| Shape | Lightweight value | `Arc<MyComponent>` — small struct under shared ownership |

The component's render impl *uses* ratatui widgets internally — inside
`render(&self, area, frame)` you instantiate `Block`, `Paragraph`, `List`,
etc. and call their render methods on the frame. ratatui's widget library
is the rendering vocabulary; ratata is the orchestration layer.

If you want a component to also work as a standalone ratatui widget for
embedding outside ratata, `impl WidgetRef for MyComponent {…}` alongside
the `Component` impl. Optional, decoupled from the framework.

## `render` takes `&mut Frame`, not `&mut Buffer`

`Buffer` would be enough for cells, but `Frame` also exposes
`set_cursor_position` — required for text inputs (Login has it). The
ratatui `Frame` carries a `'a` lifetime tied to `terminal.draw`'s closure;
since render is called from inside that closure, the lifetime works out.

## Trait shape: `async fn run(self, …)` instead of `handle_event` methods

We considered framework-driven methods (`async fn handle_event(&mut self,
…)`, `async fn handle_msg(…)`) with the framework owning the loop. Picked
single `async fn run(self, …)` instead.

**Pros.** Components have full async freedom: any number of internal
channels in their `select!`, custom timing, debouncing, broadcast
subscriptions, etc. Matches the actor mental model 1:1.

**Cons.** Boilerplate (every component writes the `select!` skeleton,
the `cancel.cancelled()` arm, the `event_rx.recv()` arm). Footguns the framework
can't catch.

**Mitigation plan.** A `simple_loop(component, …)` helper for the 70% of
components that just need "handle events; react to children". Components
that fit the simple pattern call `simple_loop(self, …).await` from their
`run`; complex components write a custom select. Not yet written — bring
it in once we see a real second component that needs it.

## Communication paradigms in one table

| Direction | Mechanism | When |
|---|---|---|
| Child → parent | typed mpsc on `Self::Msg` (`msg_tx` / `msg_rx`) | "user picked X", "fetch done" |
| Parent → child | typed mpsc on `Event` (`event_tx` / `event_rx`) | forwarded input, area changes |
| Component task → renderer | `ArcSwap<State>` on `self`, read via `&self` | publish render snapshot |
| Parent → child (read-only) | inherent methods on `&MyComponent` via the Arc clone | "what's the current FPS?" |
| Anyone → renderer | `Arc<Notify>` | request a frame |
| Runtime → tree | `CancellationToken` chain | shutdown / view swap |
| Spawned task → component | private mpsc owned by component | HTTP results, timer ticks |

Cross-component messaging (broadcast, app-wide events) is **not** in the
framework yet. When sigboom needs network messages routed to multiple
panels, the right primitive is probably `tokio::sync::broadcast` exposed
via a helper. Cross that bridge then.

## Things deliberately left out of v1

These are not oversights — they're trade-offs we made consciously and that
we'll revisit when there's a real need.

- **Per-component dirty subtree skipping.** ratatui already diffs the
  buffer against the terminal, so an unchanged subtree's cells just don't
  write. Doing additional subtree caching at the framework level is
  duplicating work. Add later if profiling shows the cell computation
  itself is a bottleneck.
- **Global focus manager.** Focus is per-parent for now. A `FocusManager`
  with Tab traversal across the whole tree is doable later; we don't
  need it yet.
- **Cross-component / app-wide events.** No primitive yet (see above).
- **Error handling beyond panic-then-die.** A component that panics aborts
  its task; the parent's `msg_rx.recv()` returns `None`. No panic recovery
  layer. Add a `Component::on_panic` hook if/when it bites.
- **Tests.** No test harness yet. The trait is shape-stable enough now
  that a `TestRuntime` (mock terminal + scripted events + state assertions)
  would be a real win.

## Reference implementations in this repo

- `src/ratata/component.rs` — the `Component` trait (Msg + run + render)
- `src/ratata/spawn.rs` — `ChildHandle` + `spawn_child` (takes `Arc<C>`)
- `src/ratata/runtime.rs` — the runtime loop (takes `Arc<R>`)
- `src/ratata/tui.rs` — default local-terminal source (crossterm + kitty keyboard)
- `src/game/fps.rs` — small component: timer task + atomic counter
- `src/game/root.rs` — root component: child mounting, cancellation, Ctrl+C
- `src/game/mod.rs` — the `Tui + Runtime` wiring helper

[ratatui]: https://github.com/ratatui/ratatui
[`rat-salsa`]: https://github.com/thscharler/rat-salsa
