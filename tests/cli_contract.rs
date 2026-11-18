use std::path::PathBuf;

use clap::{CommandFactory, Parser, error::ErrorKind};
use siphon::cli::{Cli, Command, CreatorCommand, JobCommand, PresetCommand, UploadCommand};
use siphon::config::{RuntimeConfig, default_db_path_from_env};
use siphon::domain::{CookieSource, CreatorId, PresetName, UploadId};
use siphon::download::{DownloadTargetInput, PresetSelection};
use siphon::jobs::JobId;
use siphon::refresh::RefreshCookiePolicy;

#[test]
fn clap_debug_assert_passes() {
    Cli::command().debug_assert();
}

#[test]
fn parses_all_readme_commands() {
    let cli = Cli::try_parse_from(["siphon", "creator", "add", "https://space.bilibili.com/123"])
        .unwrap();
    assert_eq!(
        cli.command,
        Command::Creator(siphon::cli::CreatorCommandArgs {
            command: CreatorCommand::Add {
                url: "https://space.bilibili.com/123".to_owned()
            }
        })
    );

    let cli = Cli::try_parse_from(["siphon", "creator", "list"]).unwrap();
    assert!(matches!(
        cli.command,
        Command::Creator(c) if c.command == CreatorCommand::List
    ));

    let cli = Cli::try_parse_from(["siphon", "creator", "remove", "7"]).unwrap();
    assert_eq!(
        cli.command,
        Command::Creator(siphon::cli::CreatorCommandArgs {
            command: CreatorCommand::Remove {
                id: CreatorId::new(7).unwrap()
            }
        })
    );

    let cli = Cli::try_parse_from(["siphon", "refresh"]).unwrap();
    match cli.command {
        Command::Refresh(args) => {
            assert_eq!(args.creator_id, None);
            assert_eq!(
                args.cookie_policy().unwrap(),
                RefreshCookiePolicy::UseSavedDefault
            );
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let cli = Cli::try_parse_from(["siphon", "refresh", "4"]).unwrap();
    match cli.command {
        Command::Refresh(args) => assert_eq!(args.creator_id, Some(CreatorId::new(4).unwrap())),
        other => panic!("unexpected command: {other:?}"),
    }

    let cli = Cli::try_parse_from(["siphon", "upload", "list"]).unwrap();
    assert!(matches!(
        cli.command,
        Command::Upload(u) if u.command == UploadCommand::List
    ));

    let cli = Cli::try_parse_from(["siphon", "upload", "ignore", "19"]).unwrap();
    assert_eq!(
        cli.command,
        Command::Upload(siphon::cli::UploadCommandArgs {
            command: UploadCommand::Ignore {
                id: UploadId::new(19).unwrap()
            }
        })
    );

    let cli = Cli::try_parse_from(["siphon", "download", "https://www.bilibili.com/video/BV1xx"])
        .unwrap();
    match cli.command {
        Command::Download(args) => {
            let plan = args.to_plan_input().unwrap();
            assert_eq!(
                plan.target,
                DownloadTargetInput::Url("https://www.bilibili.com/video/BV1xx".to_owned())
            );
            assert_eq!(plan.preset, PresetSelection::Default);
            assert_eq!(plan.cookies, None);
            assert!(plan.extra_args.is_empty());
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let cli = Cli::try_parse_from(["siphon", "download", "--upload", "42"]).unwrap();
    match cli.command {
        Command::Download(args) => {
            let plan = args.to_plan_input().unwrap();
            assert_eq!(
                plan.target,
                DownloadTargetInput::Upload(UploadId::new(42).unwrap())
            );
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let cli = Cli::try_parse_from([
        "siphon",
        "preset",
        "add",
        "audio",
        "--",
        "-x",
        "--audio-format",
        "mp3",
    ])
    .unwrap();
    assert_eq!(
        cli.command,
        Command::Preset(siphon::cli::PresetCommandArgs {
            command: PresetCommand::Add {
                name: PresetName::new("audio").unwrap(),
                args: vec![
                    "-x".to_owned(),
                    "--audio-format".to_owned(),
                    "mp3".to_owned(),
                ],
            }
        })
    );

    let cli = Cli::try_parse_from(["siphon", "preset", "list"]).unwrap();
    assert!(matches!(
        cli.command,
        Command::Preset(p) if p.command == PresetCommand::List
    ));

    let cli = Cli::try_parse_from(["siphon", "preset", "remove", "audio"]).unwrap();
    assert_eq!(
        cli.command,
        Command::Preset(siphon::cli::PresetCommandArgs {
            command: PresetCommand::Remove {
                name: PresetName::new("audio").unwrap()
            }
        })
    );

    let cli = Cli::try_parse_from(["siphon", "preset", "set-default", "video"]).unwrap();
    assert_eq!(
        cli.command,
        Command::Preset(siphon::cli::PresetCommandArgs {
            command: PresetCommand::SetDefault {
                name: PresetName::builtin_video()
            }
        })
    );

    let bg_refresh = Cli::try_parse_from(["siphon", "refresh", "--background"]).unwrap();
    match bg_refresh.command {
        Command::Refresh(args) => assert!(args.background),
        other => panic!("unexpected command: {other:?}"),
    }

    let bg_download = Cli::try_parse_from([
        "siphon",
        "download",
        "--background",
        "https://www.bilibili.com/video/BV1xx",
        "--",
        "--background",
    ])
    .unwrap();
    match bg_download.command {
        Command::Download(args) => {
            assert!(args.background);
            assert_eq!(args.extra_args, vec!["--background"]);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let passthrough_bg = Cli::try_parse_from([
        "siphon",
        "download",
        "https://www.bilibili.com/video/BV1xx",
        "--",
        "--background",
    ])
    .unwrap();
    match passthrough_bg.command {
        Command::Download(args) => {
            assert!(!args.background);
            assert_eq!(args.extra_args, vec!["--background"]);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let job_status = Cli::try_parse_from(["siphon", "job", "status", "9"]).unwrap();
    assert_eq!(
        job_status.command,
        Command::Job(siphon::cli::JobCommandArgs {
            command: JobCommand::Status {
                id: JobId::new(9).unwrap(),
            }
        })
    );
}

#[test]
fn download_passthrough_and_flags_parse_cleanly() {
    let cli = Cli::try_parse_from([
        "siphon",
        "--db",
        "/tmp/custom.db",
        "download",
        "https://www.bilibili.com/video/BV1xx",
        "--preset",
        "audio",
        "--cookies-from-browser",
        "firefox:default",
        "--",
        "-f",
        "bestaudio",
        "--db",
        "ignored-by-siphon",
        "--help",
    ])
    .unwrap();

    assert_eq!(cli.db, Some(PathBuf::from("/tmp/custom.db")));
    match cli.command {
        Command::Download(args) => {
            let plan = args.to_plan_input().unwrap();
            assert_eq!(
                plan.preset,
                PresetSelection::Named(PresetName::new("audio").unwrap())
            );
            assert_eq!(
                plan.cookies,
                Some(CookieSource::BrowserProfile("firefox:default".to_owned()))
            );
            assert_eq!(
                plan.extra_args,
                vec!["-f", "bestaudio", "--db", "ignored-by-siphon", "--help",]
            );
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let cli = Cli::try_parse_from([
        "siphon",
        "download",
        "--upload",
        "9",
        "--no-preset",
        "--cookies",
        "/tmp/cookies.txt",
        "--",
        "--write-subs",
        "--skip-download",
    ])
    .unwrap();
    match cli.command {
        Command::Download(args) => {
            let plan = args.to_plan_input().unwrap();
            assert_eq!(
                plan.target,
                DownloadTargetInput::Upload(UploadId::new(9).unwrap())
            );
            assert_eq!(plan.preset, PresetSelection::None);
            assert_eq!(
                plan.cookies,
                Some(CookieSource::File(PathBuf::from("/tmp/cookies.txt")))
            );
            assert_eq!(plan.extra_args, vec!["--write-subs", "--skip-download"]);
        }
        other => panic!("unexpected command: {other:?}"),
    }
}

#[test]
fn refresh_cookie_flag_combinations_parse_and_validate() {
    let save = Cli::try_parse_from([
        "siphon",
        "refresh",
        "3",
        "--cookies",
        "cookies.txt",
        "--save-cookies",
    ])
    .unwrap();
    match save.command {
        Command::Refresh(args) => {
            assert_eq!(
                args.cookie_policy().unwrap(),
                RefreshCookiePolicy::Explicit {
                    source: CookieSource::File(PathBuf::from("cookies.txt")),
                    save_as_default: true,
                }
            );
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let clear = Cli::try_parse_from(["siphon", "refresh", "--clear-cookies"]).unwrap();
    match clear.command {
        Command::Refresh(args) => {
            assert_eq!(
                args.cookie_policy().unwrap(),
                RefreshCookiePolicy::ClearSavedDefault
            );
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let ignore = Cli::try_parse_from(["siphon", "refresh", "--no-cookies"]).unwrap();
    match ignore.command {
        Command::Refresh(args) => {
            assert_eq!(
                args.cookie_policy().unwrap(),
                RefreshCookiePolicy::IgnoreSavedDefault
            );
        }
        other => panic!("unexpected command: {other:?}"),
    }
}

#[test]
fn rejects_invalid_arguments_and_conflicts() {
    let missing_subcommands = [
        vec!["siphon"],
        vec!["siphon", "creator"],
        vec!["siphon", "upload"],
        vec!["siphon", "preset"],
        vec!["siphon", "job"],
    ];
    for argv in missing_subcommands {
        let err = Cli::try_parse_from(argv.clone()).unwrap_err();
        assert_eq!(
            err.kind(),
            ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand,
            "expected help error for {argv:?}"
        );
    }

    let error_cases = [
        vec!["siphon", "download"],
        vec!["siphon", "download", "https://example.com", "--upload", "1"],
        vec!["siphon", "download", "--", "-f", "best"],
        vec!["siphon", "download", "https://example.com", "-f", "best"],
        vec![
            "siphon",
            "download",
            "https://example.com",
            "--preset",
            "audio",
            "--no-preset",
        ],
        vec![
            "siphon",
            "download",
            "https://example.com",
            "--cookies",
            "a.txt",
            "--cookies-from-browser",
            "firefox",
        ],
        vec!["siphon", "preset", "add", "audio"],
        vec!["siphon", "preset", "add", "audio", "--"],
        vec!["siphon", "preset", "add", "audio", "-x"],
        vec!["siphon", "refresh", "--save-cookies"],
        vec!["siphon", "refresh", "--clear-cookies", "--cookies", "a.txt"],
        vec![
            "siphon",
            "refresh",
            "--no-cookies",
            "--cookies-from-browser",
            "firefox",
        ],
        vec!["siphon", "creator", "remove", "0"],
        vec!["siphon", "creator", "remove", "abc"],
        vec!["siphon", "upload", "ignore", "0"],
        vec!["siphon", "download", "--upload", "0"],
        vec!["siphon", "job", "status", "0"],
        vec!["siphon", "job", "status", "abc"],
        vec!["siphon", "creator", "add", "   "],
        vec!["siphon", "--db", "  ", "creator", "list"],
        vec!["siphon", "preset", "add", "", "--", "-x"],
        vec!["siphon", "preset", "add", "bad name", "--", "-x"],
        vec!["siphon", "preset", "add", "-bad", "--", "-x"],
    ];

    for argv in error_cases {
        let res = Cli::try_parse_from(argv.clone());
        assert!(res.is_err(), "expected parse failure for {argv:?}");
    }
}

#[test]
fn preset_add_captures_help_flag_after_double_dash() {
    let cli = Cli::try_parse_from(["siphon", "preset", "add", "inspect", "--", "--help"]).unwrap();
    assert_eq!(
        cli.command,
        Command::Preset(siphon::cli::PresetCommandArgs {
            command: PresetCommand::Add {
                name: PresetName::new("inspect").unwrap(),
                args: vec!["--help".to_owned()],
            }
        })
    );
}

#[test]
fn runtime_config_resolves_override_and_platform_defaults() {
    let overridden = RuntimeConfig::resolve(Some(PathBuf::from("/custom/siphon.db")));
    assert_eq!(overridden.db_path, PathBuf::from("/custom/siphon.db"));

    let home_only = default_db_path_from_env(|key| match key {
        "HOME" => Some(PathBuf::from("/home/tester")),
        _ => None,
    });
    assert!(home_only.ends_with("siphon.db"));
}
