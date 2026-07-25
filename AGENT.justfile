# Agent-specific commands for AI debugging
# Run with: just -f AGENT.justfile <task>

session := "claude-fishtank"
width := "160"
height := "40"
tui_args := "--demo"
gdb_log := justfile_directory() / "tmux-gdb.txt"
extkeys_save := justfile_directory() / ".tmux-extkeys.save"

# === TUI Session ===

# Start TUI in tmux (with auto-login)
tmux-start:
    cargo build
    tmux new-session -d -s {{session}} -e ZDOTDIR={{justfile_directory()}}/agent-zsh -e FISHTANK_FORCE_KEYBOARD_ENHANCEMENT=1 -x {{width}} -y {{height}}
    tmux set-option -t {{session}} window-size manual
    just -f {{justfile()}} _tmux-extkeys-on
    tmux send-keys -t {{session}} 'cd {{justfile_directory()}} && RUST_LOG=debug cargo run -- {{tui_args}}' Enter

# Capture screen (with colors, shows gdb.txt hint if present)
tmux-capture:
    #!/usr/bin/env bash
    tmux capture-pane -t {{session}} -p -e
    if [[ -s {{gdb_log}} ]]; then
        echo "---"
        echo "tmux-gdb.txt: $(wc -c < {{gdb_log}}) bytes (use tmux-gdb-log to view)"
    fi

# Send text input
tmux-send TEXT:
    tmux send-keys -t {{session}} '{{TEXT}}'

# Send special key (Tab, Enter, Escape, Up, Down, Left, Right, BSpace, C-c)
tmux-key KEY:
    tmux send-keys -t {{session}} {{KEY}}

# Kill session and clean up
tmux-kill:
    -just -f {{justfile()}} _tmux-extkeys-off
    tmux kill-session -t {{session}}
    rm -f {{gdb_log}}

# Enable kitty-protocol passthrough for the debug session, saving the prior
# server values so tmux-kill can restore them (extended-keys is server-scoped,
# so we must not leave the user's tmux server permanently changed).
_tmux-extkeys-on:
    #!/usr/bin/env bash
    set -euo pipefail
    tmux show-options -sv extended-keys > {{extkeys_save}} 2>/dev/null || echo off > {{extkeys_save}}
    tmux show-options -sv extended-keys-format >> {{extkeys_save}} 2>/dev/null || echo xterm >> {{extkeys_save}}
    tmux set-option -s extended-keys always
    tmux set-option -s extended-keys-format csi-u

# Restore the server values saved by _tmux-extkeys-on (no-op if not saved).
_tmux-extkeys-off:
    #!/usr/bin/env bash
    set -euo pipefail
    [[ -f {{extkeys_save}} ]] || exit 0
    mapfile -t v < {{extkeys_save}}
    tmux set-option -s extended-keys "${v[0]:-off}" 2>/dev/null || true
    tmux set-option -s extended-keys-format "${v[1]:-xterm}" 2>/dev/null || true
    rm -f {{extkeys_save}}

# === GDB Debugging ===

# Start TUI under gdb in tmux (paused, ready for breakpoints, logging to gdb.txt)
tmux-gdb-start:
    cargo build
    rm -f {{gdb_log}}
    tmux new-session -d -s {{session}} -e ZDOTDIR={{justfile_directory()}}/agent-zsh -e FISHTANK_FORCE_KEYBOARD_ENHANCEMENT=1 -x {{width}} -y {{height}}
    tmux set-option -t {{session}} window-size manual
    just -f {{justfile()}} _tmux-extkeys-on
    tmux send-keys -t {{session}} 'cd {{justfile_directory()}} && gdb -ex "set pagination off" -ex "set logging file {{gdb_log}}" -ex "set logging enabled on" -ex "set args {{tui_args}}" ./target/debug/fishtank' Enter

# Send gdb command and show log output
tmux-gdb-cmd CMD:
    tmux send-keys -t {{session}} '{{CMD}}' Enter
    sleep 0.5
    cat {{gdb_log}}

# Read gdb log
tmux-gdb-log:
    cat {{gdb_log}}
