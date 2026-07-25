# TODO: BMC logs & console

Two features with different mechanics. **Both now have a v1 implemented** (see
**Status** below); this file keeps the design notes and the remaining follow-ups.

1. **Event logs** — read-only viewer of the BMC's System Event Log (SEL).
2. **Serial console** — interactive Serial-over-LAN (SOL) session.

## Status (implemented)

- **Event logs — DONE (v1).** `bmc::poll_log` + `SelEntry` (ipmi `sel elist`
  parser with severity heuristics, redfish `LogServices/SEL/Entries`, mock).
  Now the **Logs** tab of the tabbed `MachineDetail`; newest-first,
  severity-coloured, scrollable; `r` refetch. The list keeps polling behind it.
- **Tabbed detail page — DONE.** `MachineDetail` is a tabbed view (F1 Overview,
  F2 Users, F3 Logs), opened from the list with Enter / `u` / `l` (to that tab),
  each fetching lazily. Overview = `bmc::poll_overview` (mc info/fru/lan/chassis,
  redfish system, mock); Users = `bmc::poll_users` (ipmi `user list`, redfish
  Accounts, mock). `AppRoot` does the list↔detail view-swap.
- **Serial console — DONE (v1, option a).** `ConsoleRequest` + `Runtime::run_local`
  do suspend-and-exec; `c` on the list launches `ipmitool … sol activate` (IPMI
  only; refused on MAC mismatch; pre-flight `sol deactivate`). `--demo` runs a
  placeholder shell so the path is testable without a BMC.

### Remaining follow-ups

- Clear SEL (`sel clear` / Redfish `ClearLog`) behind a press-and-hold confirm.
- A **Sensors** tab (sdr) alongside Overview/Users/Logs.
- Users tab: add/enable/disable/set-privilege actions (read-only for now).
- SEL-count column on the list (cheap `sel info` on the slow cadence).
- Console also reachable from the detail view; surface console errors in the UI.
- Embedded console pane (option b) if consoles-in-layout become a need.

---

## 1. Event logs (SEL)

The BMC's hardware event log: power events, sensor assertions/deassertions, ECC
errors, fan/PSU faults, etc. Read-only and safe — good first feature.

### Sources

| | IPMI (`ipmitool`) | Redfish |
|---|---|---|
| List | `sel elist` (extended: id, timestamp, sensor, event, assert/deassert), `sel list` | `GET /redfish/v1/Systems/<id>/LogServices/SEL/Entries` (also `Managers/<id>/LogServices/.../Entries`) → JSON entries: `Created`, `Severity` (OK/Warning/Critical), `Message`, `MessageId`, `Sensor` |
| Summary | `sel info` (count, last add time, free space, overflow) | collection `Members@odata.count` |
| Clear | `sel clear` (**destructive — guard like power ops**) | `LogService.ClearLog` action / `DELETE` |
| Streaming | none — **poll** (SEL is not push) | `EventService` SSE subscriptions exist but are advanced and vendor-uneven |

Notes:
- `sel elist` is the workhorse for the Supermicro/IPMI nodes. Severity is derived
  from the event type / assertion (ipmitool doesn't give a clean severity column —
  parse the event description; threshold-crossings = warning/critical).
- Redfish gives a clean `Severity` field directly.
- Timestamps can be pre-init (BMC clock not set) → "Pre-Init" / epoch-ish; handle
  gracefully.

### Plan

- New backend calls:
  - `bmc::ipmi`: `sel_list(machine) -> Vec<SelEntry>` (run `sel elist`, parse), and
    `sel_clear(machine)` (guarded).
  - `bmc::redfish`: `GET LogServices/SEL/Entries`, map to the same `SelEntry`.
  - `bmc::mod`: `pub struct SelEntry { id, when: Option<..>, severity: Health-ish,
    sensor: String, text: String }`, `poll_log(machine, demo) -> Vec<SelEntry>`,
    `clear_log(machine, demo)`.
- UI: a **per-machine detail page** opened with **Enter** on a row — the view-swap
  ratata already anticipates (`src/app/root.rs` notes an `enum Body { List, Detail }`).
  Detail page has tabs/sections: **Info** (vendor/model/fw/serials), **Sensors**
  (sdr), **Logs** (SEL). Start with Logs.
  - Scrollable list, newest-first, severity-coloured, with `sel info` summary in
    the title.
  - Optional `c` = clear SEL, behind the same press-and-hold confirm as power.
- Feeds the previously-discussed **SEL-count column** on the list too (cheap via
  `sel info`, on the slow/health cadence).
- Mock backend: synthesize a few demo SEL entries for `--demo`.

---

## 2. Serial console (SOL)

The actual host serial console (BIOS/OS), bidirectional and interactive.

### Sources / reality

- **IPMI:** `ipmitool ... sol activate` = a live raw terminal session.
  - `sol deactivate` kills a stale session; `sol info` shows config.
  - **Exclusive**: one active SOL session per BMC — must deactivate stragglers.
  - Escape sequence `~.` exits (like ssh); needs SOL enabled + matching baud.
- **Redfish:** *no* standard interactive serial stream. It only advertises
  `SerialConsole`/`GraphicalConsole` capability under Managers; actual access is
  vendor-specific (usually falls back to IPMI SOL, SSH-to-BMC, or a KVM applet).
  → **Practical live console = IPMI SOL only** (fine for the Supermicro nodes).

### Two implementation options

- **(a) Suspend & hand over (k9s-style) — preferred for v1.**
  A key on a single machine suspends the TUI, runs `ipmitool … sol activate`
  attached to the real terminal (inherits stdio), the operator interacts, and on
  `~.` exit fishtank resumes the list.
  - Simple, full-fidelity, single machine at a time.
  - ratata already has `Tui::enter/exit` + `suspend/resume` building blocks; needs
    a small "run an interactive child to completion, then re-enter" hook driven
    from the component (e.g. a child→parent `Msg` asking the runtime to do it, or a
    dedicated path in `run_app`). Must `Tui::exit()` → run child inheriting stdio →
    `Tui::enter()` and force a full redraw.
  - Pre-flight `sol deactivate` to clear a stale session; surface non-zero exits.

- **(b) Embedded console pane — heavier, later/maybe.**
  Spawn SOL into a PTY, parse the vt100 stream, render in a ratatui pane
  (`portable-pty` + `tui-term`/`vt100`). Console lives inside the layout and could
  support multiple panes, but it's a real terminal-emulator integration — much more
  work, and it doesn't fit the selection-based model.

### Plan (option a)

- Single-machine only (not selection fan-out): act on the cursor row.
- Key (e.g. `C`) → confirm (press-and-hold, same as power) → suspend → exec → resume.
- IPMI-only: if the machine is Redfish/no-IPMI, show "console unavailable (IPMI only)".
- Password still via `IPMI_PASSWORD` + `-E`.

---

## Suggested order

1. **Event-log detail view** (Enter → Logs). Safe, read-only, architecturally
   clean; also unlocks the SEL-count column.
2. **SOL console** via suspend-and-exec (option a).
3. Defer the embedded console pane unless consoles-in-layout become a real need.
