# AI Agent Instructions

## Project

fishtank is a k9s-style TUI for inspecting bare-metal BMCs (baseboard
management controllers) in parallel, over IPMI and Redfish. It shows a live
list of machines with protocol, BMC host/MAC, serial, power state, health, and
reachability, polled concurrently in the background.

Built on top of [ratata](src/ratata) — the async-first ratatui component
framework (each component is its own tokio task; state behind `ArcSwap`;
on-demand redraw via `Notify`; child→parent typed `Msg`, parent→child
trickle-down events; cancellation via `CancellationToken`). Read
`src/ratata/DESIGN.md` before touching framework code.

## Running

- `-m`, `--machines <path>`: machine inventory in JSON5 (plain JSON works too).
  Use `-` or `/dev/stdin` to read from a pipe — fishtank does no decryption of
  its own, so producers that hold secrets elsewhere generate the inventory and
  pipe it in (e.g. `some-tool | fishtank --machines /dev/stdin`). Defaults:
  `fishtank-machines.json5` then `fishtank-machines.json` in CWD / config dir.
- `--demo`: ignore the inventory and render a built-in fake inventory (for UI
  work and the agent harness — no live BMCs needed).

IPMI polling shells out to `ipmitool`; the password is passed via the
`IPMI_PASSWORD` env var + `-E` so it never appears in `ps`/argv.

## TUI Interaction (AGENT.justfile)

Run the TUI in a detached tmux session (uses `--demo`):

```bash
just -f AGENT.justfile tmux-start       # Start TUI (--demo)
just -f AGENT.justfile tmux-capture     # Capture screen
just -f AGENT.justfile tmux-send "text" # Send text input
just -f AGENT.justfile tmux-key Down    # Send special key
just -f AGENT.justfile tmux-kill        # Kill session
```

**Special keys:** Tab, Enter, Escape, Up, Down, Left, Right, BSpace, C-c
**Keys in-app (list):** j/k or Up/Down navigate; Space toggles a machine into the
selection (teal marker + name); r re-polls. p opens the power modal and b opens
the boot modal, both acting on the selection (or the highlighted cursor row when
nothing is marked). Enter / u / l open the per-machine detail view (on the
Overview / Users / Logs tab respectively). c opens the serial console for the
cursor row (single machine).

**Detail view (tabbed):** a per-machine page with three tabs, switched with
F1 (Overview), F2 (Users), F3 (Logs); each fetches its data lazily on first
view. Overview shows assorted system facts (vendor/model/serial/firmware/CPU/
memory/BMC network…); Users lists BMC accounts with their privilege and
enabled state; Logs is the SEL event log, newest-first and severity-coloured
(OK/WARN/CRIT), sensor/text columns flexing. j/k or Up/Down scroll,
PageUp/PageDown jump, g/G go to top/bottom, r refetches the current tab, q or
Esc return to the list (which keeps polling in the background). `q` only quits
from the list view — in the detail view it means "back"; Ctrl+C quits anywhere.

**Serial console (SOL):** c suspends the TUI and hands the real terminal to
`ipmitool … sol activate` (type `~.` to exit); on return the TUI is restored.
IPMI-only — refused on Redfish machines and on a MAC mismatch. Pre-flight
`sol deactivate` clears a stale session. In `--demo` (no BMC) c runs a
placeholder shell so the suspend/exec/resume path is still exercisable.

The power and boot modals share one widget: a cursor list with per-row hotkeys,
shown as `> [k] label` (the `>` marks the cursor). Navigate with j/k or Up/Down.
- **Power**: o=on, s=soft-off, f=force-off, c=cycle (cold), r=reset (warm).
- **Boot**: p=pxe, d=disk, b=bios, c=cd, n=no override. `t` toggles the whole
  modal between **Once** (next boot only, default) and **Persistent** (sticks
  across reboots — IPMI `options=persistent` / Redfish `Continuous`); the title
  and an "Apply:" line show the current mode.

Both modals capture all input: only Esc closes them; every other key (including
q / Ctrl+C) is consumed and does not reach the app. On enhanced (kitty)
terminals confirming an action is **press-and-hold for 1.5s** — hold a row's
hotkey (which also selects that row), or move the cursor and hold Enter; a
coloured bar fills the dialog width behind the label. The hold is sustained by
the key's auto-repeat (a "still held" heartbeat); letting go stops the repeats
so it cancels within ~0.75s (a release event cancels instantly where the
terminal sends them). A quick tap never completes, and the held key's lingering
auto-repeat after completion is swallowed (so it doesn't leak to the list). On
terminals without the enhanced protocol the hold/animation is skipped and a
single hotkey/Enter confirms immediately. Acted rows show modifying…/✓ in the
State column (✓ lingers 5s). Power and boot ops are refused on a MAC mismatch.

Note: `tmux send-keys` sends a single press (no auto-repeat), so it acts like a
tap — a held action will *not* complete in the harness from one keypress; send
the action key repeatedly (~every 100ms for 2s) to simulate holding.

**User can watch:** `tmux attach -t claude-fishtank`

## Debugging

**Logs:** `~/.local/share/fishtank/fishtank.log`

```bash
tail -f ~/.local/share/fishtank/fishtank.log
```

### GDB Debugging

```bash
just -f AGENT.justfile tmux-gdb-start                          # Start gdb (paused)
just -f AGENT.justfile tmux-gdb-cmd "break machine_list.rs:80" # Set breakpoint
just -f AGENT.justfile tmux-gdb-cmd "run"                      # Run program
just -f AGENT.justfile tmux-capture                            # Capture screen
just -f AGENT.justfile tmux-gdb-cmd "bt"                       # Backtrace
just -f AGENT.justfile tmux-gdb-cmd "continue"                # Continue
just -f AGENT.justfile tmux-kill                               # Kill session
```

## Project Structure

- `src/ratata/` — the component framework: trait, runtime, spawn helper, event/redraw plumbing, local-terminal source
- `src/app/` — the application: `AppRoot` (root, list↔detail view-swap) + `MachineList` (list view) + `MachineDetail` (per-machine tabbed detail: Overview / Users / Logs)
- `src/bmc/` — BMC backends: `ipmi` (ipmitool), `redfish` (reqwest), `mock` (demo)
- `src/inventory.rs` — machine inventory TOML + in-memory SOPS decryption
- `src/main.rs`, `src/cli.rs` — entry point and argument parsing
