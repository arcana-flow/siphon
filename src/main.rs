use std::ffi::OsString;
use std::process::ExitCode;

use clap::Parser;
use clap::error::ErrorKind as ClapErrorKind;
use serde::Serialize;
use siphon::app::{AppExecuteOptions, AppServices, execute_with_runtime};
use siphon::background::{BackgroundRuntime, run_worker_entrypoint};
use siphon::cli::{Cli, Command, has_json_flag};
use siphon::config::{RuntimeConfig, resolve_bilibili_api_base};
use siphon::download::{DownloadExecutionOptions, YtDlpDownloader};
use siphon::error::AppError;
use siphon::output::{
    CommandResult, HelpData, HumanRenderOptions, JsonError, JsonResponse, VersionData,
    format_human_error,
};
use siphon::platform::bilibili::{BilibiliClientConfig, BilibiliRefreshSource};
use siphon::storage::SqliteStorage;

fn main() -> ExitCode {
    let raw_args: Vec<OsString> = std::env::args_os().collect();
    let cli = match Cli::try_parse_from(&raw_args) {
        Ok(cli) => cli,
        Err(clap_err) => {
            if !has_json_flag(&raw_args) {
                let _ = clap_err.print();
                return ExitCode::from(u8::try_from(clap_err.exit_code()).unwrap_or(2));
            }
            match clap_err.kind() {
                ClapErrorKind::DisplayHelp => {
                    emit_json_response(&JsonResponse::ok(HelpData::from_clap_help_error(
                        &clap_err,
                    )));
                    return ExitCode::SUCCESS;
                }
                ClapErrorKind::DisplayVersion => {
                    emit_json_response(&JsonResponse::ok(VersionData::current()));
                    return ExitCode::SUCCESS;
                }
                _ => {
                    emit_json_response(&JsonResponse::err(JsonError::from_clap_error(&clap_err)));
                    return ExitCode::from(u8::try_from(clap_err.exit_code()).unwrap_or(2));
                }
            }
        }
    };

    if matches!(cli.command, Command::InternalWorker) {
        return run_worker_entrypoint();
    }

    let json_mode = cli.json;
    let render_options = if json_mode {
        HumanRenderOptions::plain()
    } else {
        HumanRenderOptions::detect()
    };

    let config = RuntimeConfig::resolve(cli.db.clone());
    let api_base = match resolve_bilibili_api_base() {
        Ok(base) => base,
        Err(err) => {
            emit_bootstrap_error(
                json_mode,
                render_options.stderr_color,
                JsonError::invalid_config(&err),
                &err,
            );
            return ExitCode::FAILURE;
        }
    };

    let storage = match SqliteStorage::open(&config.db_path) {
        Ok(storage) => storage,
        Err(err) => {
            let app_err = AppError::Storage(err);
            emit_bootstrap_error(
                json_mode,
                render_options.stderr_color,
                JsonError::from_app_error(&app_err, None),
                &app_err.to_string(),
            );
            return ExitCode::FAILURE;
        }
    };

    let mut bili_config = BilibiliClientConfig::default();
    if let Some(base_url) = &api_base {
        bili_config = bili_config.with_api_base_url(base_url);
    }
    let refresh_source = BilibiliRefreshSource::with_config(bili_config);
    let downloader = YtDlpDownloader::new();
    let services = AppServices {
        storage: &storage,
        refresh_source: &refresh_source,
        downloader: &downloader,
    };

    let exec_options = AppExecuteOptions {
        download: if json_mode {
            DownloadExecutionOptions::json_mode()
        } else {
            DownloadExecutionOptions::passthrough()
        },
    };
    let background_runtime = BackgroundRuntime::with_api_base(api_base);
    let execution = execute_with_runtime(cli, &services, exec_options, &background_runtime);

    if json_mode {
        if let Some(err) = execution.error {
            let partial_refresh = match &execution.result {
                Some(CommandResult::Refreshed(summary)) => Some(summary),
                _ => None,
            };
            emit_json_response(&JsonResponse::err(JsonError::from_app_error(
                &err,
                partial_refresh,
            )));
            return ExitCode::FAILURE;
        }
        if let Some(result) = execution.result {
            match result.to_json_line() {
                Ok(line) => print!("{line}"),
                Err(err) => {
                    emit_json_response(&JsonResponse::err(JsonError::new(
                        "serialization_error",
                        err.to_string(),
                    )));
                    return ExitCode::FAILURE;
                }
            }
        }
        return ExitCode::SUCCESS;
    }

    if let Some(result) = execution.result {
        let output = result.render_human(render_options);
        print!("{}", output.stdout);
        eprint!("{}", output.stderr);
    }
    if let Some(err) = execution.error {
        eprint!(
            "{}",
            format_human_error(&err.to_string(), render_options.stderr_color)
        );
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn emit_json_response<T: Serialize>(response: &JsonResponse<T>) {
    match response.to_json_line() {
        Ok(line) => print!("{line}"),
        Err(err) => {
            let fallback = format!(
                "{{\"ok\":false,\"error\":{{\"code\":\"serialization_error\",\"message\":{}}}}}\n",
                serde_json::to_string(&err.to_string())
                    .unwrap_or_else(|_| "\"serialization failed\"".to_owned())
            );
            print!("{fallback}");
        }
    }
}

fn emit_bootstrap_error(json_mode: bool, stderr_color: bool, json_err: JsonError, human_msg: &str) {
    if json_mode {
        emit_json_response(&JsonResponse::err(json_err));
    } else {
        eprint!("{}", format_human_error(human_msg, stderr_color));
    }
}
