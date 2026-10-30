//! `sapphire-agent`, the headless edition: the command line of
//! [`sapphire_agent_cli`] and nothing else. The desktop app's binary runs the
//! same commands.

use anyhow::Result;
use clap::Parser as _;
use sapphire_agent_cli::{Cli, Edition};

fn main() -> Result<()> {
    let cli = Cli::parse();
    sapphire_agent_cli::run(cli.options, cli.command, Edition::Headless)
}
