use clap::Parser;
use cli::Cli;

mod app;
mod bmc;
mod cli;
mod config;
mod errors;
mod inventory;
mod logging;
mod ratata;

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    crate::errors::init()?;
    crate::logging::init()?;

    let args = Cli::parse();

    // In demo mode we synthesize a fake inventory so the UI (and the agent
    // harness) works without any config file or live BMCs.
    let inventory = if args.demo {
        inventory::Inventory::demo()
    } else {
        inventory::load(args.machines.as_deref())?
    };

    app::run_app(inventory, args.demo).await?;
    Ok(())
}
