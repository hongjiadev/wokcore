use std::path::{Path, PathBuf};

use wokcore_platform::{
    sessions::{
        SessionEnvironment, SessionError, SessionRootOverrides, SessionRoots, SessionSourceKind,
    },
    system::paths::Platform,
};

#[test]
fn source_labels_and_debug_output_never_expose_absolute_roots() {
    let environment = SessionEnvironment::new(
        Platform::Linux,
        [
            ("CODEX_HOME", "/private/synthetic/codex"),
            ("CLAUDE_CONFIG_DIR", "/private/synthetic/claude"),
            ("GEMINI_CLI_HOME", "/private/synthetic/gemini"),
        ],
    );
    let roots = SessionRoots::resolve(environment, SessionRootOverrides::new()).unwrap();

    assert_eq!(SessionSourceKind::Codex.label(), "codex");
    assert_eq!(SessionSourceKind::ClaudeCode.label(), "claude_code");
    assert_eq!(SessionSourceKind::GeminiCli.label(), "gemini_cli");
    let debug = format!("{roots:?}");
    assert!(debug.contains("codex"));
    assert!(debug.contains("claude_code"));
    assert!(debug.contains("gemini_cli"));
    assert!(!debug.contains("/private/synthetic"));
}

#[test]
fn explicit_roots_precede_absolute_environment_and_home_fallbacks() {
    for fixture in platform_fixtures() {
        let environment = SessionEnvironment::new(
            fixture.platform,
            [
                ("HOME", fixture.home),
                ("USERPROFILE", fixture.home),
                ("CODEX_HOME", fixture.codex_environment),
                ("CLAUDE_CONFIG_DIR", fixture.claude_environment),
                ("GEMINI_CLI_HOME", fixture.gemini_environment),
            ],
        );
        let overrides = SessionRootOverrides::new()
            .with_root(SessionSourceKind::Codex, fixture.codex_explicit)
            .with_root(SessionSourceKind::ClaudeCode, fixture.claude_explicit)
            .with_root(SessionSourceKind::GeminiCli, fixture.gemini_explicit);

        let roots = SessionRoots::resolve(environment, overrides).unwrap();

        assert_eq!(
            roots.path(SessionSourceKind::Codex),
            Path::new(fixture.codex_explicit)
        );
        assert_eq!(
            roots.path(SessionSourceKind::ClaudeCode),
            Path::new(fixture.claude_explicit)
        );
        assert_eq!(
            roots.path(SessionSourceKind::GeminiCli),
            Path::new(fixture.gemini_explicit)
        );
    }
}

#[test]
fn absolute_environment_roots_precede_platform_home_fallbacks() {
    for fixture in platform_fixtures() {
        let environment = SessionEnvironment::new(
            fixture.platform,
            [
                ("HOME", fixture.home),
                ("USERPROFILE", fixture.home),
                ("CODEX_HOME", fixture.codex_environment),
                ("CLAUDE_CONFIG_DIR", fixture.claude_environment),
                ("GEMINI_CLI_HOME", fixture.gemini_environment),
            ],
        );

        let roots = SessionRoots::resolve(environment, SessionRootOverrides::new()).unwrap();

        assert_eq!(
            roots.path(SessionSourceKind::Codex),
            Path::new(fixture.codex_environment)
        );
        assert_eq!(
            roots.path(SessionSourceKind::ClaudeCode),
            Path::new(fixture.claude_environment)
        );
        assert_eq!(
            roots.path(SessionSourceKind::GeminiCli),
            Path::new(fixture.gemini_environment)
        );
    }
}

#[test]
fn relative_environment_roots_are_ignored_in_favor_of_platform_home_fallbacks() {
    for fixture in platform_fixtures() {
        let environment = SessionEnvironment::new(
            fixture.platform,
            [
                ("HOME", fixture.home),
                ("USERPROFILE", fixture.home),
                ("CODEX_HOME", "relative/codex"),
                ("CLAUDE_CONFIG_DIR", r"relative\claude"),
                ("GEMINI_CLI_HOME", "relative/gemini"),
            ],
        );

        let roots = SessionRoots::resolve(environment, SessionRootOverrides::new()).unwrap();

        assert_eq!(
            roots.path(SessionSourceKind::Codex),
            expected_home_root(&fixture, ".codex")
        );
        assert_eq!(
            roots.path(SessionSourceKind::ClaudeCode),
            expected_home_root(&fixture, ".claude")
        );
        assert_eq!(
            roots.path(SessionSourceKind::GeminiCli),
            expected_home_root(&fixture, ".gemini")
        );
    }
}

#[test]
fn a_relative_explicit_root_fails_instead_of_falling_through() {
    for fixture in platform_fixtures() {
        let environment = SessionEnvironment::new(
            fixture.platform,
            [
                ("HOME", fixture.home),
                ("USERPROFILE", fixture.home),
                ("CODEX_HOME", fixture.codex_environment),
            ],
        );
        let overrides =
            SessionRootOverrides::new().with_root(SessionSourceKind::Codex, "relative/root");

        assert!(matches!(
            SessionRoots::resolve(environment, overrides),
            Err(SessionError::UnsafePath)
        ));
    }
}

#[test]
fn locale_and_timezone_values_do_not_change_session_roots() {
    let first = SessionEnvironment::new(
        Platform::Linux,
        [
            ("HOME", "/synthetic/home"),
            ("LANG", "zh_CN.UTF-8"),
            ("TZ", "Asia/Shanghai"),
        ],
    );
    let second = SessionEnvironment::new(
        Platform::Linux,
        [
            ("HOME", "/synthetic/home"),
            ("LANG", "en_US.UTF-8"),
            ("TZ", "America/St_Johns"),
        ],
    );

    let first = SessionRoots::resolve(first, SessionRootOverrides::new()).unwrap();
    let second = SessionRoots::resolve(second, SessionRootOverrides::new()).unwrap();

    for kind in SessionSourceKind::ALL {
        assert_eq!(first.path(kind), second.path(kind));
    }
}

#[test]
fn missing_absolute_home_and_source_roots_fail_closed() {
    for platform in [Platform::Windows, Platform::Macos, Platform::Linux] {
        let environment = SessionEnvironment::new(
            platform,
            [
                ("HOME", "relative/home"),
                ("USERPROFILE", "relative/profile"),
            ],
        );

        assert!(matches!(
            SessionRoots::resolve(environment, SessionRootOverrides::new()),
            Err(SessionError::MissingPlatformData {
                name: "home directory"
            })
        ));
    }
}

struct PlatformFixture {
    platform: Platform,
    home: &'static str,
    codex_environment: &'static str,
    claude_environment: &'static str,
    gemini_environment: &'static str,
    codex_explicit: &'static str,
    claude_explicit: &'static str,
    gemini_explicit: &'static str,
}

fn platform_fixtures() -> [PlatformFixture; 3] {
    [
        PlatformFixture {
            platform: Platform::Windows,
            home: r"C:\Users\synthetic",
            codex_environment: r"D:\codex-env",
            claude_environment: r"D:\claude-env",
            gemini_environment: r"D:\gemini-env",
            codex_explicit: r"E:\codex-explicit",
            claude_explicit: r"E:\claude-explicit",
            gemini_explicit: r"E:\gemini-explicit",
        },
        PlatformFixture {
            platform: Platform::Macos,
            home: "/Users/synthetic",
            codex_environment: "/Volumes/synthetic/codex-env",
            claude_environment: "/Volumes/synthetic/claude-env",
            gemini_environment: "/Volumes/synthetic/gemini-env",
            codex_explicit: "/private/synthetic/codex-explicit",
            claude_explicit: "/private/synthetic/claude-explicit",
            gemini_explicit: "/private/synthetic/gemini-explicit",
        },
        PlatformFixture {
            platform: Platform::Linux,
            home: "/home/synthetic",
            codex_environment: "/srv/synthetic/codex-env",
            claude_environment: "/srv/synthetic/claude-env",
            gemini_environment: "/srv/synthetic/gemini-env",
            codex_explicit: "/opt/synthetic/codex-explicit",
            claude_explicit: "/opt/synthetic/claude-explicit",
            gemini_explicit: "/opt/synthetic/gemini-explicit",
        },
    ]
}

fn expected_home_root(fixture: &PlatformFixture, name: &str) -> PathBuf {
    let separator = match fixture.platform {
        Platform::Windows => '\\',
        Platform::Macos | Platform::Linux => '/',
    };
    PathBuf::from(format!("{}{separator}{name}", fixture.home))
}
