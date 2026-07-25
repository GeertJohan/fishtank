# Minimal zsh rc for the AI-agent tmux session (see AGENT.justfile).
#
# AGENT.justfile starts tmux with ZDOTDIR pointed at this directory, so the
# pane's zsh reads THIS file instead of ~/.zshrc. That deliberately keeps the
# pane free of personal startup output (e.g. `unsolicited-advice`), which
# otherwise pollutes `tmux capture-pane` and can be misread as instructions.
#
# Keep this minimal. `cargo` is /usr/bin/cargo and is always on PATH, so the
# TUI builds/runs without anything here. mise is activated only as defensive
# insurance for future toolchain needs; it is skipped if mise is unavailable.

command -v mise >/dev/null 2>&1 && eval "$(mise activate zsh)"
