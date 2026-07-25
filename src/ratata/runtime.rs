use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::{Terminal, backend::Backend};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::{Component, Event, Tui, spawn::spawn_child};

/// A request to suspend the TUI and hand the real terminal to an interactive
/// child process (k9s-style), then re-enter the TUI when it exits.
///
/// Used for things like a serial console (`ipmitool … sol activate`): the
/// runtime tears the alt-screen down, stops its own input pump so the child
/// owns stdin, runs the program to completion inheriting stdio, then restores
/// the TUI and forces a full repaint. Handled only by [`Runtime::run_local`].
pub struct ConsoleRequest {
    /// Program to exec (e.g. `"ipmitool"`).
    pub program: String,
    pub args: Vec<String>,
    /// Extra environment for the child (e.g. `IPMI_PASSWORD`), kept out of argv.
    pub envs: Vec<(String, String)>,
    /// One-line note printed before launch (e.g. how to exit the console).
    pub note: String,
    /// Delivered after the child exits: `Ok` on clean exit, `Err` otherwise.
    pub reply: oneshot::Sender<Result<(), String>>,
}

/// Drives the root component task and the on-demand render loop.
///
/// The runtime is intentionally thin: it owns the redraw [`Notify`] and the
/// FPS debounce, spawns the root via [`spawn_child`], forwards input events
/// from the caller-supplied source into the root's `event_tx`, and calls
/// `root.render(...)` whenever a redraw fires.
///
/// Terminal setup and event sourcing live outside the runtime — pass in a
/// `&mut Terminal` and a `&mut Receiver<Event>`. This keeps the framework
/// decoupled from any specific terminal backend or event-stream library.
pub struct Runtime<R: Component> {
    root: Arc<R>,
    max_fps: f64,
}

impl<R: Component> Runtime<R> {
    pub fn new(root: Arc<R>) -> Self {
        Self {
            root,
            max_fps: 60.0,
        }
    }

    /// Upper bound on render rate. The render loop only fires when something
    /// requests a redraw; this just caps the rate when many requests pile up.
    pub fn with_max_fps(mut self, fps: f64) -> Self {
        self.max_fps = fps;
        self
    }

    /// Run until the root component exits (returns from `run`).
    ///
    /// Cleanup: on exit (normal or error), cancels the root subtree and
    /// awaits the root's join handle so spawned tasks have a chance to
    /// observe cancellation. The terminal is left as the caller set it up —
    /// suspending/leaving the alt-screen is the caller's responsibility.
    ///
    /// Backend-agnostic entry point (SSH / tests). fishtank itself uses
    /// [`run_local`](Self::run_local), which adds console suspend-and-exec.
    #[allow(dead_code)]
    pub async fn run<B: Backend>(
        self,
        terminal: &mut Terminal<B>,
        events: &mut mpsc::UnboundedReceiver<Event>,
    ) -> color_eyre::Result<()> {
        let redraw = Arc::new(Notify::new());
        let root_cancel = CancellationToken::new();
        let mut handle = spawn_child(self.root, redraw.clone(), &root_cancel);

        let frame_min = Duration::from_secs_f64(1.0 / self.max_fps);
        let mut last_render = Instant::now() - frame_min; // allow immediate first draw

        // Initial render — shows whatever the root paints before the task
        // does any work, so the alt-screen isn't blank during startup.
        terminal.draw(|f| handle.component.render(f.area(), f))?;

        // Track whether we exited via the `join` arm — if so, `handle.join`
        // is already consumed and must not be polled again (tokio panics).
        let mut root_already_finished = false;

        let result: color_eyre::Result<()> = loop {
            tokio::select! {
                // Root finished. JoinHandle::Output is Result<(), JoinError>;
                // surface a panic, ignore a clean cancel.
                join_res = &mut handle.join => {
                    root_already_finished = true;
                    break match join_res {
                        Ok(()) => Ok(()),
                        Err(e) if e.is_panic() => Err(color_eyre::eyre::eyre!(
                            "root component panicked: {e}"
                        )),
                        Err(_) => Ok(()), // cancelled
                    };
                }

                // Redraw requested. Honor FPS cap, then call render on the
                // shared root Arc. `notify_one` coalesces — many requests
                // between draws still produce at most one frame.
                _ = redraw.notified() => {
                    let elapsed = last_render.elapsed();
                    if elapsed < frame_min {
                        tokio::time::sleep(frame_min - elapsed).await;
                    }
                    if let Err(e) = terminal.draw(|f| handle.component.render(f.area(), f)) {
                        break Err(e.into());
                    }
                    last_render = Instant::now();
                }

                // Input event from the caller. Forward to root. If the
                // event channel is full, `send` awaits — backpressure on
                // the producer.
                Some(ev) = events.recv() => {
                    let _ = handle.event_tx.send(ev).await;
                }
            }
        };

        // Cleanup: signal cancellation and let the root unwind, but only if
        // it hasn't already finished — re-polling a completed JoinHandle
        // panics.
        root_cancel.cancel();
        if !root_already_finished {
            let _ = tokio::time::timeout(Duration::from_millis(100), &mut handle.join).await;
        }
        result
    }

    /// Like [`run`](Self::run), but driving a local [`Tui`] and additionally
    /// servicing [`ConsoleRequest`]s — suspend-and-exec for interactive child
    /// processes (serial consoles). Because the runtime owns the `Tui` here, it
    /// can leave/re-enter the alt-screen and stop/restart the input pump around
    /// the child; the generic [`run`](Self::run) stays backend-agnostic.
    pub async fn run_local(
        self,
        tui: &mut Tui,
        console_rx: &mut mpsc::Receiver<ConsoleRequest>,
    ) -> color_eyre::Result<()> {
        let redraw = Arc::new(Notify::new());
        let root_cancel = CancellationToken::new();
        let mut handle = spawn_child(self.root, redraw.clone(), &root_cancel);

        let frame_min = Duration::from_secs_f64(1.0 / self.max_fps);
        let mut last_render = Instant::now() - frame_min;

        tui.terminal
            .draw(|f| handle.component.render(f.area(), f))?;

        let mut root_already_finished = false;

        let result: color_eyre::Result<()> = loop {
            tokio::select! {
                join_res = &mut handle.join => {
                    root_already_finished = true;
                    break match join_res {
                        Ok(()) => Ok(()),
                        Err(e) if e.is_panic() => Err(color_eyre::eyre::eyre!(
                            "root component panicked: {e}"
                        )),
                        Err(_) => Ok(()),
                    };
                }

                _ = redraw.notified() => {
                    let elapsed = last_render.elapsed();
                    if elapsed < frame_min {
                        tokio::time::sleep(frame_min - elapsed).await;
                    }
                    if let Err(e) = tui.terminal.draw(|f| handle.component.render(f.area(), f)) {
                        break Err(e.into());
                    }
                    last_render = Instant::now();
                }

                Some(ev) = tui.event_rx.recv() => {
                    let _ = handle.event_tx.send(ev).await;
                }

                // Suspend the TUI, hand the terminal to the child, then restore.
                Some(req) = console_rx.recv() => {
                    let _ = tui.exit();
                    let reply = run_console(&req).await;
                    let _ = tui.enter();
                    let _ = tui.terminal.clear();
                    last_render = Instant::now() - frame_min;
                    if let Err(e) = tui.terminal.draw(|f| handle.component.render(f.area(), f)) {
                        let _ = req.reply.send(reply);
                        break Err(e.into());
                    }
                    let _ = req.reply.send(reply);
                }
            }
        };

        root_cancel.cancel();
        if !root_already_finished {
            let _ = tokio::time::timeout(Duration::from_millis(100), &mut handle.join).await;
        }
        result
    }
}

/// Run a [`ConsoleRequest`]'s program to completion with the controlling
/// terminal (`/dev/tty`) as its stdio.
///
/// fishtank's own stdin may be a pipe (e.g. `producer | fishtank --machines
/// /dev/stdin`), so inheriting it would give an interactive tool like
/// `ipmitool sol activate` a non-tty stdin → "tcgetattr: Inappropriate ioctl
/// for device". The TUI reads keys from /dev/tty for exactly this reason, so the
/// console child does too. The TUI must already be suspended.
#[cfg(unix)]
async fn run_console(req: &ConsoleRequest) -> Result<(), String> {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::process::Stdio;

    let open_tty = || OpenOptions::new().read(true).write(true).open("/dev/tty");

    if !req.note.is_empty()
        && let Ok(mut tty) = open_tty()
    {
        // Explicit CR+LF and a leading CR: the previous program (ipmitool) may
        // leave the cursor mid-line / ONLCR off, which otherwise "staircases"
        // our text and can clip the first column.
        let _ = write!(tty, "\r\n{}\r\n\r\n", req.note);
    }

    let (sin, sout, serr) = match (open_tty(), open_tty(), open_tty()) {
        (Ok(i), Ok(o), Ok(e)) => (i, o, e),
        _ => return Err("cannot open /dev/tty for the console".to_string()),
    };

    let mut cmd = tokio::process::Command::new(&req.program);
    cmd.args(&req.args)
        .stdin(Stdio::from(sin))
        .stdout(Stdio::from(sout))
        .stderr(Stdio::from(serr));
    for (k, v) in &req.envs {
        cmd.env(k, v);
    }

    match cmd.status().await {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => {
            let msg = format!("console exited ({s})");
            wait_enter(msg.clone()).await;
            Err(msg)
        }
        Err(e) => {
            let msg = format!("console error: {e}");
            wait_enter(msg.clone()).await;
            Err(msg)
        }
    }
}

/// Print an error on the controlling terminal and block (off-thread) until the
/// operator presses Enter, so they can read it before the TUI repaints over it.
#[cfg(unix)]
async fn wait_enter(msg: String) {
    let _ = tokio::task::spawn_blocking(move || {
        use std::fs::OpenOptions;
        use std::io::{BufRead, BufReader, Write};
        let Ok(tty) = OpenOptions::new().read(true).write(true).open("/dev/tty") else {
            return;
        };
        if let Ok(mut w) = tty.try_clone() {
            let _ = write!(w, "\r\n{msg} — press Enter to return…\r\n");
        }
        let mut line = String::new();
        let _ = BufReader::new(tty).read_line(&mut line);
    })
    .await;
}

/// Fallback for non-unix: inherit stdio (no `/dev/tty`).
#[cfg(not(unix))]
async fn run_console(req: &ConsoleRequest) -> Result<(), String> {
    if !req.note.is_empty() {
        println!("{}\n", req.note);
    }
    let mut cmd = tokio::process::Command::new(&req.program);
    cmd.args(&req.args);
    for (k, v) in &req.envs {
        cmd.env(k, v);
    }
    match cmd.status().await {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("console exited ({s})")),
        Err(e) => Err(e.to_string()),
    }
}
