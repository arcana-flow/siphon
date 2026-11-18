use std::process::ExitCode;

use clap::Parser;
use siphon::app::{AppServices, execute};
use siphon::cli::Cli;
use siphon::config::RuntimeConfig;
use siphon::download::UnimplementedDownloader;
use siphon::refresh::UnimplementedRefreshSource;
use siphon::storage::UnimplementedStorage;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let _config = RuntimeConfig::resolve(cli.db.clone());

    let storage = UnimplementedStorage;
    let refresh_source = UnimplementedRefreshSource;
    let downloader = UnimplementedDownloader;
    let services = AppServices {
        storage: &storage,
        refresh_source: &refresh_source,
        downloader: &downloader,
    };

    match execute(cli, &services) {
        Ok(output) => {
            print!("{}", output.stdout);
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}
