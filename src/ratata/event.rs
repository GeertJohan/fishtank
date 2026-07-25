use crossterm::event::{KeyEvent, MouseEvent};

/// Input events delivered to the component tree.
///
/// Renders are on-demand (via [`Notify`](tokio::sync::Notify)), so there is
/// no synthetic Tick/Render variant — only real input. The default source is
/// [`Tui`](super::Tui), but any [`mpsc::Receiver<Event>`](tokio::sync::mpsc::Receiver)
/// works with [`Runtime::run`](super::Runtime::run).
// The full input model is part of ratata's framework surface; a given app need
// not consume every variant (e.g. fishtank ignores mouse/paste and reacts to a
// resize without reading its dimensions), so the carried data may go unread.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub enum Event {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize(u16, u16),
    Paste(String),
    FocusGained,
    FocusLost,
}
