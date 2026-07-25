# Default recipe
default:
	@just --list

# Watch the AI agent's TUI session, read-only (safe: cannot type into it)
watch-agent-tui: (_watch-agent-tui "ro")

# Watch the AI agent's TUI session, read-write (can interact with it)
watch-agent-tui-rw: (_watch-agent-tui "rw")

# Shared: wait for the tmux session to appear, then attach (mode = ro|rw)
_watch-agent-tui mode:
	@case "{{mode}}" in \
		ro) flag="-r" ;; \
		rw) flag="" ;; \
		*) echo "error: mode must be 'ro' or 'rw', got '{{mode}}'" >&2; exit 1 ;; \
	esac; \
	echo "Waiting for claude-fishtank tmux session..."; \
	while true; do \
		if tmux has-session -t claude-fishtank 2>/dev/null; then \
			printf '\a'; \
			tmux attach $flag -t claude-fishtank; \
		fi; \
		sleep 1; \
	done
