//! The fishtank application built on ratata: an [`AppRoot`] shell hosting the
//! [`MachineList`](machine_list::MachineList) view.

mod machine_detail;
mod machine_list;
mod root;

pub use root::AppRoot;

use tokio::sync::mpsc;

use crate::inventory::Inventory;
use crate::ratata::{ConsoleRequest, Runtime, Tui};

/// Spin up the terminal and drive the runtime against the app root.
pub async fn run_app(inventory: Inventory, demo: bool) -> color_eyre::Result<()> {
    let mut tui = Tui::new()?;
    tui.enter()?;
    let keyboard_mode = tui.keyboard_mode();

    // Console (SOL) suspend-and-exec requests flow from the component tree up to
    // the runtime, which owns the terminal lifecycle.
    let (console_tx, mut console_rx) = mpsc::channel::<ConsoleRequest>(4);

    let root = AppRoot::new(keyboard_mode, inventory, demo, console_tx);
    // Renders are on-demand (idle = 0 CPU); this only caps the burst rate.
    let runtime = Runtime::new(root).with_max_fps(60.0);
    let result = runtime.run_local(&mut tui, &mut console_rx).await;

    tui.exit()?;
    result
}
