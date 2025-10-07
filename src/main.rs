use anyhow::Context;
use argh::FromArgs;
use tracing::debug;

use crate::build::BuildCmd;

mod build;

/// A blazing fast static site generator.
#[derive(FromArgs, Debug)]
struct Cli {
    /// be verbose
    #[argh(switch, short = 'v')]
    verbose: bool,

    #[argh(subcommand)]
    subcommand: SubCommand,
}

#[derive(FromArgs, Debug)]
#[argh(subcommand)]
enum SubCommand {
    Build(BuildCmd),
}

fn main() -> anyhow::Result<()> {
    let cli: Cli = argh::from_env();

    let log_level = if cli.verbose {
        tracing::Level::DEBUG
    } else {
        tracing::Level::INFO
    };

    tracing_subscriber::fmt().with_max_level(log_level).init();

    debug!(?cli, "Parsed CLI arguments");

    let context = format!("failed to execute subcommand '{:?}'", cli.subcommand);
    match cli.subcommand {
        SubCommand::Build(cmd) => build::build(cmd),
    }
    .context(context)
}
