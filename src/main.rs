use argh::FromArgs;
use tracing::{debug, info};

/// A blazing fast static site generator.
#[derive(FromArgs, Debug)]
struct Cli {
    /// be verbose
    #[argh(switch, short = 'v')]
    verbose: bool,
}

fn main() {
    let cli: Cli = argh::from_env();

    let log_level = if cli.verbose {
        tracing::Level::DEBUG
    } else {
        tracing::Level::INFO
    };

    tracing_subscriber::fmt().with_max_level(log_level).init();

    info!("Starting up...");
    debug!("This is a debug message and will only show in verbose mode.");

    info!("All done!");
}
