# Contributing to fishtank

Thanks for your interest! Contributions are welcome.

## Getting started

- Install Rust **1.87+** and `ipmitool` (only needed to talk to
  real IPMI BMCs; not required for `--demo` or the tests).
- Run the UI without any hardware: `cargo run -- --demo`.
- [`AGENTS.md`](AGENTS.md) is the full guide to the codebase and the tmux-based
  interaction harness; [`src/ratata/DESIGN.md`](src/ratata/DESIGN.md) explains
  the component framework, so read it before touching `src/ratata/`.

## Before opening a PR

CI runs these; please make sure they pass locally:

```sh
cargo fmt --all --check
cargo clippy --all-targets    # treated as errors in CI
cargo test
```

## Guidelines

- Keep the ratata framework (`src/ratata/`) generic; app-specific logic lives in
  `src/app/`.
- Match the surrounding style: comment density, naming, and idioms.
- New BMC-facing parsing should come with a unit test (see `src/bmc/ipmi.rs`).
- Never commit real inventories or credentials; `fishtank-machines.json*` is
  git-ignored for that reason.

By contributing you agree that your contributions are licensed under the
project's [MIT license](LICENSE).
