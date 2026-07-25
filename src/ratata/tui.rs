//! Default local-terminal source: crossterm + ratatui's `Terminal`,
//! with kitty keyboard, mouse, and paste support.
//!
//! Produces [`Event`]s directly so consumers can hand `Tui::event_rx`
//! straight to [`Runtime::run`](super::Runtime::run) without translation.
//!
//! For non-stdout backends (SSH, tests), skip this entirely and wire your
//! own `Terminal` + `mpsc::Receiver<Event>` into `Runtime::run`.

#![allow(dead_code)]

use std::{
    io::{Stdout, stdout},
    ops::{Deref, DerefMut},
    time::Duration,
};

use crossterm::{
    cursor,
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event as CrosstermEvent, EventStream, KeyEventKind, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    terminal::{EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::{FutureExt, StreamExt};
use ratatui::backend::CrosstermBackend as Backend;
use tokio::{
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tracing::error;

use super::Event;

/// Which keyboard input encoding is in effect for this session.
///
/// The two modes encode the same physical keypress differently, so key
/// handling must branch on this rather than guess:
/// - `Legacy`: Shift+letter arrives as the capital `Char`, with no modifier
///   bit, and `KeyEvent.state` is always empty (no Caps Lock disambiguation).
/// - `Enhanced`: kitty keyboard protocol — base `Char` plus explicit modifier
///   bits, and `KeyEvent.state` (Caps Lock, etc.) is populated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardMode {
    Legacy,
    Enhanced,
}

/// Local-terminal wrapper: owns the ratatui `Terminal`, the crossterm event
/// stream task, and the [`Event`] mpsc both produces.
///
/// Typical use:
/// ```ignore
/// let mut tui = Tui::new()?;
/// tui.enter()?;
/// let runtime = Runtime::new(root);
/// runtime.run(&mut tui.terminal, &mut tui.event_rx).await?;
/// tui.exit()?;
/// ```
pub struct Tui {
    pub terminal: ratatui::Terminal<Backend<Stdout>>,
    pub task: JoinHandle<()>,
    pub cancellation_token: CancellationToken,
    pub event_rx: UnboundedReceiver<Event>,
    pub event_tx: UnboundedSender<Event>,
    pub mouse: bool,
    pub paste: bool,
    /// Whether we successfully pushed kitty keyboard-enhancement flags in
    /// `enter()` (so `exit()` knows to pop them). Set per enter/exit cycle.
    keyboard_enhanced: bool,
}

impl Tui {
    pub fn new() -> color_eyre::Result<Self> {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        Ok(Self {
            terminal: ratatui::Terminal::new(Backend::new(stdout()))?,
            task: tokio::spawn(async {}),
            cancellation_token: CancellationToken::new(),
            event_rx,
            event_tx,
            mouse: false,
            paste: false,
            keyboard_enhanced: false,
        })
    }

    pub fn mouse(mut self, mouse: bool) -> Self {
        self.mouse = mouse;
        self
    }

    pub fn paste(mut self, paste: bool) -> Self {
        self.paste = paste;
        self
    }

    /// The keyboard input mode negotiated in `enter()`. Valid after `enter()`.
    pub fn keyboard_mode(&self) -> KeyboardMode {
        if self.keyboard_enhanced {
            KeyboardMode::Enhanced
        } else {
            KeyboardMode::Legacy
        }
    }

    pub fn start(&mut self) {
        self.cancel(); // Cancel any existing task
        self.cancellation_token = CancellationToken::new();
        let event_loop = Self::event_loop(self.event_tx.clone(), self.cancellation_token.clone());
        self.task = tokio::spawn(async {
            event_loop.await;
        });
    }

    /// Pure crossterm-event pump. Emits [`Event`]s directly. Crossterm errors
    /// are logged and skipped; stream-closed exits the loop. No synthetic
    /// Init/Tick/Render — ratata drives rendering on demand via [`Notify`].
    ///
    /// [`Notify`]: tokio::sync::Notify
    async fn event_loop(event_tx: UnboundedSender<Event>, cancellation_token: CancellationToken) {
        let mut event_stream = EventStream::new();
        loop {
            let event = tokio::select! {
                _ = cancellation_token.cancelled() => {
                    break;
                }
                crossterm_event = event_stream.next().fuse() => match crossterm_event {
                    Some(Ok(event)) => match event {
                        // Forward Press, Repeat and Release. Repeat/Release power
                        // press-and-hold interactions (auto-repeat acts as a "still
                        // held" heartbeat; release cancels where the terminal sends it).
                        CrosstermEvent::Key(key) if matches!(
                            key.kind,
                            KeyEventKind::Press | KeyEventKind::Repeat | KeyEventKind::Release
                        ) =>
                        {
                            Event::Key(key)
                        }
                        CrosstermEvent::Mouse(mouse) => Event::Mouse(mouse),
                        CrosstermEvent::Resize(x, y) => Event::Resize(x, y),
                        CrosstermEvent::FocusLost => Event::FocusLost,
                        CrosstermEvent::FocusGained => Event::FocusGained,
                        CrosstermEvent::Paste(s) => Event::Paste(s),
                        _ => continue, // other crossterm events not in ratata::Event
                    }
                    Some(Err(e)) => {
                        error!("crossterm event stream error: {e}");
                        continue;
                    }
                    None => break, // stream ended
                },
            };
            if event_tx.send(event).is_err() {
                // the receiver has been dropped, so there's no point in continuing the loop
                break;
            }
        }
        cancellation_token.cancel();
    }

    pub fn stop(&self) -> color_eyre::Result<()> {
        self.cancel();
        let mut counter = 0;
        while !self.task.is_finished() {
            std::thread::sleep(Duration::from_millis(1));
            counter += 1;
            if counter > 50 {
                self.task.abort();
            }
            if counter > 100 {
                error!("Failed to abort task in 100 milliseconds for unknown reason");
                break;
            }
        }
        Ok(())
    }

    pub fn enter(&mut self) -> color_eyre::Result<()> {
        crossterm::terminal::enable_raw_mode()?;
        // Opt into the kitty keyboard protocol.
        // DISAMBIGUATE_ESCAPE_CODES is the documented gate for KeyEvent.state
        // (which carries CAPS_LOCK). REPORT_EVENT_TYPES makes the terminal
        // send press/repeat/release; the event loop in run() already keeps
        // only KeyEventKind::Press, so release/repeat are ignored for now.
        //
        // Normally we gate on supports_keyboard_enhancement(), which does a
        // CSI ? u query/response round-trip and falls back to legacy input
        // when unsupported. That query has no responder inside a *detached*
        // tmux pane (nothing answers CSI ? u), so FISHTANK_FORCE_KEYBOARD_
        // ENHANCEMENT lets the AGENT harness skip the query and push the
        // flags unconditionally (tmux still relays the CSI-u keys via
        // extended-keys). Never set this for a terminal that lacks support.
        let force = std::env::var_os("FISHTANK_FORCE_KEYBOARD_ENHANCEMENT").is_some();
        let supported = force
            || matches!(
                crossterm::terminal::supports_keyboard_enhancement(),
                Ok(true)
            );
        self.keyboard_enhanced = supported
            && crossterm::execute!(
                stdout(),
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                )
            )
            .is_ok();
        tracing::debug!(
            "keyboard enhancement enabled: {} (forced: {})",
            self.keyboard_enhanced,
            force
        );
        crossterm::execute!(stdout(), EnterAlternateScreen, cursor::Hide)?;
        if self.mouse {
            crossterm::execute!(stdout(), EnableMouseCapture)?;
        }
        if self.paste {
            crossterm::execute!(stdout(), EnableBracketedPaste)?;
        }
        self.start();
        Ok(())
    }

    pub fn exit(&mut self) -> color_eyre::Result<()> {
        self.stop()?;
        // Restore the terminal defensively: never gate undoing the kitty
        // keyboard enhancement on raw-mode state (otherwise a stray
        // is_raw_mode_enabled() == false would leave the shell echoing CSI-u
        // escape codes), and never let one failing escape sequence early-return
        // and skip the rest — best-effort each step.
        let _ = self.flush();
        if self.keyboard_enhanced {
            let _ = crossterm::execute!(stdout(), PopKeyboardEnhancementFlags);
            self.keyboard_enhanced = false;
        }
        if self.paste {
            let _ = crossterm::execute!(stdout(), DisableBracketedPaste);
        }
        if self.mouse {
            let _ = crossterm::execute!(stdout(), DisableMouseCapture);
        }
        let _ = crossterm::execute!(stdout(), LeaveAlternateScreen, cursor::Show);
        if crossterm::terminal::is_raw_mode_enabled().unwrap_or(false) {
            let _ = crossterm::terminal::disable_raw_mode();
        }
        Ok(())
    }

    /// Best-effort terminal restore for panic / abnormal-exit paths, where the
    /// live `Tui` (and its flags) isn't available. Everything is unconditional
    /// and ignores errors — it just tries to undo every terminal mode we ever
    /// enable so the shell isn't left in raw / alt-screen / kitty-CSI-u state.
    pub fn force_restore() {
        let _ = crossterm::execute!(stdout(), PopKeyboardEnhancementFlags);
        let _ = crossterm::execute!(stdout(), DisableBracketedPaste);
        let _ = crossterm::execute!(stdout(), DisableMouseCapture);
        let _ = crossterm::execute!(stdout(), LeaveAlternateScreen, cursor::Show);
        let _ = crossterm::terminal::disable_raw_mode();
    }

    pub fn cancel(&self) {
        self.cancellation_token.cancel();
    }

    pub fn suspend(&mut self) -> color_eyre::Result<()> {
        self.exit()?;
        #[cfg(not(windows))]
        signal_hook::low_level::raise(signal_hook::consts::signal::SIGTSTP)?;
        Ok(())
    }

    pub fn resume(&mut self) -> color_eyre::Result<()> {
        self.enter()?;
        Ok(())
    }

    pub async fn next_event(&mut self) -> Option<Event> {
        self.event_rx.recv().await
    }

    /// Try to get the next event without blocking. Returns None if no event is available.
    pub fn try_next_event(&mut self) -> Option<Event> {
        self.event_rx.try_recv().ok()
    }
}

impl Deref for Tui {
    type Target = ratatui::Terminal<Backend<Stdout>>;

    fn deref(&self) -> &Self::Target {
        &self.terminal
    }
}

impl DerefMut for Tui {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.terminal
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        self.exit().unwrap();
    }
}
