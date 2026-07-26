use std::{
    path::PathBuf,
    process::{Command, Output},
};

use wokcore::{
    ExitCode,
    cli::{Command as CliCommand, parse_command},
};

const ROOT_HELP: &str = "\
Independent local provider gateway for the Wok product family

Usage: wokcore <COMMAND>

Commands:
  serve      Run the local WokCore service
  status     Report local WokCore service status
  stop       Gracefully stop the local WokCore service
  doctor     Diagnose the local WokCore service
  authorize  Issue a one-time proxy token for a client
  help       Print this message or the help of the given subcommand(s)

Options:
  -h, --help     Print help
  -V, --version  Print version
";

fn run(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wokcore"))
        .args(arguments)
        .output()
        .expect("wokcore binary should start")
}

#[test]
fn root_help_and_version_are_exact_and_deterministic() {
    let help = run(&["--help"]);
    assert!(help.status.success());
    assert_eq!(String::from_utf8(help.stdout).unwrap(), ROOT_HELP);
    assert!(help.stderr.is_empty());

    let version = run(&["--version"]);
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8(version.stdout).unwrap(),
        concat!("wokcore ", env!("CARGO_PKG_VERSION"), "\n")
    );
    assert!(version.stderr.is_empty());
}

#[test]
fn only_the_five_documented_commands_and_options_are_accepted() {
    let cases = [
        (
            "serve",
            "Run the local WokCore service\n\nUsage: wokcore serve [OPTIONS]\n\nOptions:\n      --json  Emit stable JSON output\n  -h, --help  Print help\n",
        ),
        (
            "status",
            "Report local WokCore service status\n\nUsage: wokcore status [OPTIONS]\n\nOptions:\n      --json  Emit stable JSON output\n  -h, --help  Print help\n",
        ),
        (
            "stop",
            "Gracefully stop the local WokCore service\n\nUsage: wokcore stop [OPTIONS]\n\nOptions:\n      --json  Emit stable JSON output\n  -h, --help  Print help\n",
        ),
        (
            "doctor",
            "Diagnose the local WokCore service\n\nUsage: wokcore doctor [OPTIONS]\n\nOptions:\n      --json  Emit stable JSON output\n  -h, --help  Print help\n",
        ),
        (
            "authorize",
            "Issue a one-time proxy token for a client\n\nUsage: wokcore authorize --client <ID> --json\n\nOptions:\n      --client <ID>  Client identifier to authorize\n      --json         Emit the one-time token in a stable JSON object\n  -h, --help         Print help\n",
        ),
    ];

    for (command, expected) in cases {
        let output = run(&[command, "--help"]);
        assert!(output.status.success(), "{command}");
        assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
        assert!(output.stderr.is_empty(), "{command}");
    }

    for rejected in [
        &["start"][..],
        &["serve", "--port", "10101"],
        &["status", "--token", "secret"],
        &["stop", "--authorization", "secret"],
        &["doctor", "--credential-path", "secret"],
        &["authorize", "--client", "wokrouter", "--token", "secret"],
    ] {
        assert!(!run(rejected).status.success(), "{rejected:?}");
    }
}

#[test]
fn authorize_requires_json_before_any_command_side_effect_can_run() {
    let output = run(&["authorize", "--client", "wokrouter"]);

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("the following required arguments were not provided:"));
    assert!(stderr.contains("--json"));
}

#[test]
fn public_exit_codes_have_one_stable_numeric_mapping() {
    assert_eq!(
        [
            ExitCode::Success.as_u8(),
            ExitCode::InternalFailure.as_u8(),
            ExitCode::InvalidInput.as_u8(),
            ExitCode::NotRunning.as_u8(),
            ExitCode::AlreadyRunning.as_u8(),
            ExitCode::PortOccupied.as_u8(),
            ExitCode::AuthenticationFailure.as_u8(),
            ExitCode::StorageCorruption.as_u8(),
        ],
        [0, 1, 2, 3, 4, 5, 6, 7]
    );
}

#[test]
fn parse_failure_prevents_the_injected_command_side_effect() {
    let mut side_effect_calls = 0;
    let result = parse_command(["wokcore", "authorize", "--client", "wokrouter"]).map(|command| {
        side_effect_calls += 1;
        assert!(matches!(command, CliCommand::Authorize(_)));
    });

    let error = result.unwrap_err();
    assert_eq!(error.exit_code(), ExitCode::InvalidInput.as_i32());
    assert_eq!(side_effect_calls, 0);
}

#[test]
fn production_main_uses_discovered_paths_and_returns_the_command_exit_code() {
    let root = std::env::temp_dir().join(format!("wokcore-cli-main-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_wokcore"))
        .args(["status", "--json"])
        .env("APPDATA", root.join("roaming"))
        .env("LOCALAPPDATA", root.join("local"))
        .env("HOME", root.join("home"))
        .env("USERPROFILE", root.join("profile"))
        .env("XDG_CONFIG_HOME", root.join("xdg-config"))
        .env("XDG_STATE_HOME", root.join("xdg-state"))
        .env("XDG_RUNTIME_DIR", root.join("xdg-runtime"))
        .output()
        .expect("wokcore binary should start");

    assert_eq!(output.status.code(), Some(ExitCode::NotRunning.as_i32()));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "{\"code\":\"not_running\"}\n"
    );
    assert!(output.stderr.is_empty());
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);

    remove_test_directory(root);
}

fn remove_test_directory(path: PathBuf) {
    if path.starts_with(std::env::temp_dir()) {
        std::fs::remove_dir_all(path).unwrap();
    }
}
