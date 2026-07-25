# fishtank

A [k9s](https://k9scli.io/)-style terminal UI for inspecting and controlling
bare-metal **BMCs** (baseboard management controllers) in parallel, over **IPMI**
and **Redfish**.

[![crates.io](https://img.shields.io/crates/v/fishtank.svg)](https://crates.io/crates/fishtank)
[![CI](https://github.com/GeertJohan/fishtank/actions/workflows/ci.yml/badge.svg)](https://github.com/GeertJohan/fishtank/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

fishtank shows a live, concurrently-polled list of machines with their protocol,
BMC host/MAC, serial, power state, health, fault/identify flags, boot override,
and reachability. From there you can drive power, set boot overrides, open a
per-machine detail page (overview, users, event log), and start a serial
console. It's built for watching a lot of BMCs at once.

```text
 fishtank   j/k nav · Space sel · p power · b boot · Enter detail · u users · l logs · c console · q quit
┌ Machines (8) ────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│  Name                        Proto   BMC Host        MAC               Serial           Power Healt Flags  Boot  State       │
│  dc1-r01-node01              ipmi    10.130.128.10   90:5a:08:17:ae:01 OD381957S        on    CRIT  ·      PXE   ok          │
│  dc1-r01-node02              ipmi    10.130.128.11   90:5a:08:17:ae:02 OD381958S        off   WARN  ID     -     slow        │
│  dc1-r01-node03              ipmi    10.130.128.12   90:5a:08:17:ae:03 OD381959S        on    WARN  ·      -     ok          │
│  dc1-r02-node01              redfish 10.130.129.10   90:5a:08:17:bf:01 OD411748S        off   OK    FAULT  -     slow        │
│  dc1-r02-node02              redfish 10.130.129.11   90:5a:08:17:bf:02 OD411749S        on    CRIT  ·      PXE   ok          │
│  dc2-r01-node01              ipmi    10.131.128.10   90:5a:08:18:ae:01 OD192292S        off   CRIT  FAULT  PXE   slow        │
│  dc2-r01-node02              ipmi    10.131.128.13   90:5a:08:18:ae:02 -                -     -     -      -     unreachable │
│  dc2-r01-node03              ipmi    10.131.128.14   90:5a:08:18:ae:03 OD192296S        on    OK    FAULT  -     MAC MISMATCH│
│                                                                                                                              │
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

Try it without hardware using `fishtank --demo` (colours, omitted above, encode
power/health/faults), or `cargo run -- --demo` from a checkout.

## Features

- **Parallel polling** of many BMCs over IPMI (`ipmitool`) and Redfish (HTTPS),
  split into a fast power/boot-state cadence and a slower health (sensor) one.
- **Power control**: on, soft-off, force-off, cycle, or reset, applied to a
  multi-selection and confirmed with a press-and-hold so a stray keypress can't
  reboot a rack.
- **Boot overrides**: set the next boot (or make it persistent) to PXE, disk,
  BIOS, or CD, or clear the override.
- **Per-machine detail page** with Overview (vendor, model, firmware, CPU,
  memory, BMC network), Users (accounts and privileges), and Logs (the SEL
  event log, newest first, severity-coloured).
- **Serial console** (SOL) that suspends the TUI and hands the terminal to
  `ipmitool sol activate`, k9s-style.
- **MAC verification**: the configured MAC is checked against the one the BMC
  reports; on a mismatch the row is flagged and power/boot actions are refused,
  so you don't act on the wrong node.
- **No built-in secret handling**: reads a plain JSON5 inventory from a file or
  a pipe, so credentials can stay wherever you already keep them.

## Requirements

- Rust 1.87 or newer.
- `ipmitool` on `PATH` for IPMI machines. Redfish needs nothing extra.

## Install

```sh
# From crates.io
cargo install fishtank

# Or from source
git clone https://github.com/GeertJohan/fishtank
cd fishtank
cargo build --release   # binary at target/release/fishtank
```

## Usage

```sh
fishtank --machines path/to/fishtank-machines.json5
fishtank --demo                       # built-in fake inventory, no BMCs needed
some-tool | fishtank --machines -     # read the inventory from stdin
```

- `-m`, `--machines <PATH>`: inventory path. `-` or `/dev/stdin` reads from a
  pipe. If omitted, fishtank looks for `fishtank-machines.json5` then
  `fishtank-machines.json` in the current directory and the config directory.
- `--demo`: ignore the inventory and render a built-in fake one.

### Inventory format

A plain **JSON5** document (JSON is a subset, so plain JSON works too). Unknown
fields are rejected, so typos surface early.

```json5
{
  // Optional; folded into every machine unless overridden.
  defaults: {
    username: "ADMIN",
    poll_interval_secs: 30,     // power/boot-state cadence
    health_interval_secs: 120,  // sensor cadence (slower; expensive over IPMI)
    insecure: false,            // accept self-signed Redfish TLS certs
  },
  machines: [
    {
      name: "dc1-r01-node01",
      protocol: "ipmi",           // "ipmi" | "redfish"
      host: "10.130.128.10",      // BMC IP or hostname
      mac: "90:5a:08:17:ae:01",   // optional; verified against the BMC
      username: "ADMIN",          // optional; falls back to defaults / "ADMIN"
      password: "secret",         // optional; falls back to defaults
      // serial: "..."            // optional; otherwise discovered by polling
    },
    {
      name: "dc1-r02-node01",
      protocol: "redfish",
      host: "10.130.129.10",
      scheme: "https",            // optional (default https)
      port: 443,                  // optional
      insecure: true,             // optional (default false)
      username: "admin",
      password: "secret",
    },
  ],
}
```

fishtank does no decryption itself, so the usual way to feed it secrets is to
generate the inventory on the fly and pipe it in, keeping the plaintext off disk:

```sh
sops -d bmc-credentials.enc.yaml | your-transform | fishtank --machines /dev/stdin
```

## Keybindings

| Context | Keys | Action |
|---|---|---|
| List | `j`/`k`, `Up`/`Down` | move cursor |
| List | `Space` | toggle a machine into the selection |
| List | `p` / `b` | open the **power** / **boot** modal |
| List | `Enter` / `u` / `l` | open detail on **Overview** / **Users** / **Logs** |
| List | `c` | open serial console (cursor row) |
| List | `r` | re-poll now |
| List | `q`, `Ctrl+C` | quit |
| Modal | `j`/`k` | move the `>` cursor |
| Modal | hotkey (`o`/`s`/`f`/`c`/`r`, `p`/`d`/`b`/`c`/`n`) | select that row **and** start its hold |
| Modal | hold hotkey / hold `Enter` (~1.5s) | confirm (release cancels) |
| Modal | `t` (boot) | toggle Once / Persistent |
| Modal | `Esc` | cancel |
| Detail | `F1`/`F2`/`F3` | switch tab (Overview/Users/Logs) |
| Detail | `j`/`k`, `g`/`G`, PgUp/PgDn | scroll |
| Detail | `r` | refetch the tab |
| Detail | `q` / `Esc` | back to the list |

Power and boot are confirmed with a press-and-hold. On terminals with the kitty
keyboard protocol a coloured bar fills over about 1.5s and releasing cancels; on
other terminals a single press or `Enter` commits. A quick tap never fires.

## Security model

- **Credentials never appear in `ps`/argv.** The IPMI password is passed to
  `ipmitool` via the `IPMI_PASSWORD` environment variable with `-E`; Redfish uses
  HTTP basic auth over TLS.
- **fishtank does no encryption of its own, on purpose.** It reads a plaintext
  inventory; keep secrets in your existing store (SOPS, Vault, a secret manager)
  and pipe the generated inventory in via stdin so nothing lands on disk. Don't
  commit a real `fishtank-machines.json*` (it's git-ignored here).
- **MAC verification** guards against a wrong host-to-BMC mapping: if the
  configured MAC doesn't match what the BMC reports, the row shows `MAC MISMATCH`
  and power/boot actions are refused.
- **Serial console** needs a BMC account with at least the SOL privilege
  (typically OPERATOR).

## Development

See [AGENTS.md](AGENTS.md) for the full contributor guide, including the
tmux-based interaction harness. Common checks:

```sh
cargo fmt --check
cargo clippy
cargo test
cargo run -- --demo
```

Logs go to `~/.local/share/fishtank/fishtank.log` (`RUST_LOG=debug` for verbose).

## Roadmap

Not done yet, roughly in priority order:

- [ ] Sensors tab (`sdr`) in the detail page.
- [ ] User management from the Users tab (add / enable / disable / set
      privilege); it's read-only for now.
- [ ] Clear the event log (`sel clear` / Redfish `ClearLog`), behind a
      press-and-hold confirm.
- [ ] SEL-count column on the machine list.
- [ ] Open the serial console from the detail page, and surface console errors
      in the UI.
- [ ] Fetch the BMC MAC over Redfish so MAC verification covers Redfish machines
      too (IPMI-only today).
- [ ] UEFI boot-override option (`options=efiboot`).
- [ ] Optional per-machine cipher/privilege (`-C` / `-L`) in the inventory for
      stricter BMCs.
- [ ] Redfish serial-console fallback over SSH where the BMC advertises it.

## License

MIT, © GeertJohan. See [LICENSE](LICENSE).
