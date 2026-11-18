use std::process::ExitCode;

use clap::Parser;
use siphon::app::{AppServices, execute};
use siphon::cli::Cli;
use siphon::config::{RuntimeConfig, resolve_bilibili_api_base};
use siphon::download::YtDlpDownloader;
use siphon::platform::bilibili::{BilibiliClientConfig, BilibiliRefreshSource};
use siphon::storage::SqliteStorage;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let config = RuntimeConfig::resolve(cli.db.clone());

    let api_base = match resolve_bilibili_api_base() {
        Ok(base) => base,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };

    let storage = match SqliteStorage::open(&config.db_path) {
        Ok(storage) => storage,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };

    let mut bili_config = BilibiliClientConfig::default();
    if let Some(base_url) = api_base {
        bili_config = bili_config.with_api_base_url(base_url);
    }
    let refresh_source = BilibiliRefreshSource::with_config(bili_config);
    let downloader = YtDlpDownloader::new();
    let services = AppServices {
        storage: &storage,
        refresh_source: &refresh_source,
        downloader: &downloader,
    };

    match execute(cli, &services) {
        Ok(output) => {
            print!("{}", output.stdout);
            eprint!("{}", output.stderr);
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}
