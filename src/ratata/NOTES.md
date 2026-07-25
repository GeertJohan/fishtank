# ratata — notes & open questions

Working scratch list of things we decided to defer, alternatives we
considered, and things to revisit. Not documentation — see `DESIGN.md` for
the rationale on what's currently shipped.

## Open design questions

### Cross-component / app-wide messaging
**Status**: nothing in the framework.

Needed for sigboom when:
- Network packets arrive and need to reach multiple panels (chat, world view, scoreboard)
- "Player died" fans out to HP bar, animation layer, chat, sound

Candidates:
- `tokio::sync::broadcast<Event>` — producer publishes, components subscribe independently. Decoupled, no central router.
- Route through root — network task → root.handle_msg → root calls subscriber methods. Centralised, root knows topology.
- Per-target Sender<Event> registry — most coupled, most explicit.

Probably broadcast. Add a framework-level "shared bus" optional resource that
components can subscribe to via Ctx. Defer until sigboom actually has a
network layer.

### Focus tracking
**Status**: each parent tracks its own focused child ad-hoc.

Open whether to ship a `FocusManager` that does Tab traversal across the
whole tree. The hard part is making it composable when a focus group lives
several levels deep. rat-salsa's `rat-focus` is the most polished example —
worth reading before designing ours.

For now, document the convention: "the parent forwards events to the `event_tx`
of whichever child it considers focused; Tab cycles focus among siblings
at the same level." Good enough until a real need shows up.

### Spawned-task result routing
**Status**: each component creates its own private mpsc for task results.

Works fine but is repetitive. Possible helper:

```rust
ctx.spawn_with_result(fut, |task_result| my_internal_msg)
```

Where `ctx` would be an additional handle the component constructs. Not
clearly better than `tokio::spawn` + private `internal_tx.clone()` — leave
as-is until the pattern is unwieldy.

### Async `simple_loop` helper
**Status**: not written.

The 70% of components that just need "handle events, react to children,
maybe handle an internal channel" all write the same `select!` skeleton.
Helper sketch:

```rust
pub async fn simple_loop<F>(
    cancel: CancellationToken,
    mut event_rx: mpsc::Receiver<Event>,
    mut handle: F,
) where F: FnMut(Event) -> ControlFlow,
{
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            Some(ev) = event_rx.recv() => {
                if matches!(handle(ev), ControlFlow::Break(())) { break; }
            }
        }
    }
}
```

Not yet useful — only one component (sigboom's placeholder root) would use
it. Revisit once we have 3+ components that fit the same mould.

## Performance things to revisit

### Per-component dirty subtree skipping
**Status**: not implemented.

ratatui already diffs Buffer against terminal — only changed cells hit the
wire. The win from also skipping `Component::render` for unchanged subtrees
is purely CPU spent computing cells. For our scale this is fine. If
profiling shows render CPU is a bottleneck at high session counts:

- Add `fn is_dirty(&self, state: &State) -> bool` (default `true`).
- Parents skip child render when clean *and* the area is unchanged.
- Cache the rendered Buffer slice per component for the unchanged-area case.

The `Component` trait shape currently allows adding this without breaking
existing code.

### Persistent collections (`im`)
**Status**: not used.

If a State grows to hold many MBs of dense data with frequent partial
mutation, full clones in `rcu` become expensive even with `Arc`-wrapped
fields. `im::Vector`/`im::HashMap` give O(log N) writes with O(1) clones
of the container.

Worth it only when profiling says so. Cost: dependency, slightly less
familiar API.

### ArcSwap sharding for hot/cold state separation
**Status**: not used.

If a component has both very-frequently-changing fields (e.g., per-tick
animation state) and rarely-changing fields, sharding into multiple
`ArcSwap<Substate>` lets each be updated independently without cloning the
other. Reader does multiple atomic loads. Useful but adds complexity.

Defer until measured.

### Render path lock contention on `Arc<Mutex<TreeState>>` etc.
**Status**: known, accepted for now.

Stateful ratatui widgets (TreeState, ListState) mutate during render
(auto-scroll). We hold their state behind `Arc<Mutex<…>>` so both the
component task and the render thread can touch them. Contention should be
negligible at <100Hz render with <100Hz writes, but worth measuring under
sustained input bursts. If contention shows: investigate per-widget
"snapshot before render" patterns.

## Trait/API ergonomics

### Boilerplate per `rcu` mutation
The `state.rcu(|cur| { let mut new = (**cur).clone(); new.foo = bar; Arc::new(new) })`
pattern is verbose for setting one field. Options:

- A `with_mutation` helper: `state.with(|s| s.foo = bar)`.
- A `mutate` macro: `mutate!(state, foo = bar)`.
- Live with the verbosity (compiler optimises it well; the explicit clone
  is honest about cost).

Probably live with it, but watch for repeated pain.

### `Arc<Self>` requires Sync, requires interior mutability
Components must be `Send + Sync + 'static`. All state mutation goes through
`&self` (typically via `ArcSwap`). This works for everything we've needed
so far, but rules out components that want plain `&mut self`-style state
machines. The async-first pattern doesn't really want that anyway —
mutations would conflict with the renderer's read access regardless.

### Async trait return type
The *trait* declares `fn run(...) -> impl Future<Output = ()> + Send + 'static`
because we need the Send bound for `tokio::spawn`. Implementors are free
to use `async fn run(...)` syntax — the compiler infers Send from the
captures and accepts the impl as long as the resulting future is Send.
Both forms coexist; the impl-side form is what shows up in components.

## Things that will break when we extract ratata as a separate crate

- `src/ratata/event.rs` uses `crossterm::event::{KeyEvent, MouseEvent}`
  directly. For a standalone crate we'd want the user to wire any event
  source. Either keep crossterm as an optional feature or define
  ratata-native event types and let the user translate.
- `src/ratata/tui.rs` is opinionated about stdout + crossterm. For a
  standalone crate it should probably move behind a feature flag
  (`features = ["stdout-tui"]`) so SSH/test consumers can opt out.
- `color_eyre::Result` in `Runtime::run` — leaks a specific error library.
  Switch to `Box<dyn Error>` or a ratata-defined error type before
  extracting.
- `Component: Send + Sync + 'static` — the `'static` bound means a
  component can't borrow non-static data. Fine for our pattern (state is
  owned via ArcSwap), but worth revisiting if someone wants short-lived
  scope-bound components.

## Naming we considered and rejected

- `type Msg` → `Response/Resp`: ruled out by cardinality (plural over time).
- `type Msg` → `Emit/Out`: rejected for now in favour of convention (Elm,
  Iced, rat-salsa all use `Msg`). Worth revisiting if we onboard people
  from a non-FRP background who find "Msg" directionally ambiguous.

## Helper TODOs (not blockers)

- Test harness: `TestRuntime` that takes a script of events, runs the root,
  asserts on state/messages.
- Panic recovery: optional `on_panic` hook on `Component` so a panicking
  child can be caught and the parent informed (instead of just `JoinError`
  on the `msg_rx` closing).
- Lifecycle tracing: opt-in `tracing` spans around component spawn / state
  publish / event dispatch. Useful for the "where did this render come
  from?" debugging path.
