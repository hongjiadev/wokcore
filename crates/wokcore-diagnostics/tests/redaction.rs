use wokcore_diagnostics::{
    event::{
        FailoverDecision, ModelId, PlatformCategory, ProviderProtocol, RetryDecision, StageCode,
    },
    redaction::{
        SensitiveValue, SensitiveValues, StructuralSummaryInput, build_structural_summary,
    },
};

fn fixed_input() -> StructuralSummaryInput {
    StructuralSummaryInput::new(
        ProviderProtocol::AnthropicMessages,
        StageCode::Response,
        RetryDecision::NotRetried,
        FailoverDecision::NotSelected,
        true,
    )
}

fn sensitive<'a>(values: impl IntoIterator<Item = SensitiveValue<'a>>) -> SensitiveValues<'a> {
    values
        .into_iter()
        .fold(SensitiveValues::new(), |set, value| {
            set.push(value).unwrap()
        })
}

#[test]
fn forbidden_low_entropy_values_have_byte_identical_absence() {
    let first = build_structural_summary(
        fixed_input(),
        sensitive([
            SensitiveValue::authorization("Basic YQ=="),
            SensitiveValue::cookie("sid=0"),
            SensitiveValue::api_key("a"),
            SensitiveValue::body(b"{}"),
        ]),
    )
    .unwrap();
    let second = build_structural_summary(
        fixed_input(),
        sensitive([
            SensitiveValue::authorization("Bearer a-very-different-value"),
            SensitiveValue::cookie("sid=999999999999999999999"),
            SensitiveValue::api_key("different-length-secret"),
            SensitiveValue::body(b"{\"prompt\":\"different\"}"),
        ]),
    )
    .unwrap();

    assert_eq!(first, second);
    assert_eq!(
        serde_json::to_vec(first.summary()).unwrap(),
        serde_json::to_vec(second.summary()).unwrap()
    );
}

#[cfg(windows)]
fn non_utf8_os_string() -> std::ffi::OsString {
    use std::os::windows::ffi::OsStringExt;
    std::ffi::OsString::from_wide(&[0xd800])
}

#[cfg(unix)]
fn non_utf8_os_string() -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(vec![0xff])
}

#[test]
fn forbidden_material_is_removed_before_bound_or_hash() {
    let invalid_os_a = non_utf8_os_string();
    let invalid_os_b = non_utf8_os_string();
    let first = build_structural_summary(
        fixed_input(),
        sensitive([
            SensitiveValue::authorization("Basic YQ=="),
            SensitiveValue::proxy_authorization("Negotiate low-entropy"),
            SensitiveValue::cookie("sid=0"),
            SensitiveValue::set_cookie("sid=0; HttpOnly"),
            SensitiveValue::api_key("sk-a"),
            SensitiveValue::oauth_token("oauth-a"),
            SensitiveValue::token("eyJhbGciOiJub25lIn0.e30."),
            SensitiveValue::prompt("提示\r\n\0\u{1b}\u{0085}\u{202e}"),
            SensitiveValue::response("respuesta العربية 👩‍💻"),
            SensitiveValue::tool_json("{\"tool_payload\":\"秘密\"}"),
            SensitiveValue::sse_frame(b"data: token-a\r\n\r\n"),
            SensitiveValue::account_name("alice@example.test"),
            SensitiveValue::backend_error("credential backend: password=a"),
            SensitiveValue::credential("secret-a"),
            SensitiveValue::path(std::ffi::OsStr::new(r"C:\Users\Alice\.key")),
            SensitiveValue::path(std::ffi::OsStr::new(r"\\server\share\secret")),
            SensitiveValue::path(std::ffi::OsStr::new(r"\\?\C:\very\secret")),
            SensitiveValue::path(std::ffi::OsStr::new(r"\\.\PhysicalDrive0")),
            SensitiveValue::path(std::ffi::OsStr::new("/home/alice/.ssh/id")),
            SensitiveValue::path(std::ffi::OsStr::new(
                "file://user:pass@example.test/a%2Fb?q=secret",
            )),
            SensitiveValue::path(&invalid_os_a),
        ]),
    )
    .unwrap();
    let second = build_structural_summary(
        fixed_input(),
        sensitive([
            SensitiveValue::authorization("Bearer completely-different"),
            SensitiveValue::proxy_authorization("Basic YmJi"),
            SensitiveValue::cookie("sid=999999"),
            SensitiveValue::set_cookie("other=long-value; Secure"),
            SensitiveValue::api_key("sk-different-and-long"),
            SensitiveValue::oauth_token("oauth-different"),
            SensitiveValue::token("header.payload.signature"),
            SensitiveValue::prompt("prompt-different"),
            SensitiveValue::response("response-different"),
            SensitiveValue::tool_json("{\"different\":true}"),
            SensitiveValue::sse_frame(b"event: done\ndata: different\n\n"),
            SensitiveValue::account_name("bob"),
            SensitiveValue::backend_error("backend error B"),
            SensitiveValue::credential("credential-b"),
            SensitiveValue::path(std::ffi::OsStr::new(r"D:\Other")),
            SensitiveValue::path(std::ffi::OsStr::new(r"\\other\share")),
            SensitiveValue::path(std::ffi::OsStr::new(r"\\?\D:\other")),
            SensitiveValue::path(std::ffi::OsStr::new(r"\\.\Tape0")),
            SensitiveValue::path(std::ffi::OsStr::new("/var/lib/other")),
            SensitiveValue::path(std::ffi::OsStr::new(
                "https://other:pass@example.test/x?y=z",
            )),
            SensitiveValue::path(&invalid_os_b),
        ]),
    )
    .unwrap();

    assert_eq!(first, second);
    let visible = serde_json::to_string(first.summary()).unwrap();
    for canary in [
        "Basic YQ==",
        "sid=0",
        "sk-a",
        "oauth-a",
        "eyJhbGci",
        "提示",
        "العربية",
        "👩‍💻",
        "Alice",
        "server",
        "PhysicalDrive",
        "/home/",
        "%2F",
        "secret",
    ] {
        assert!(!visible.contains(canary), "{canary:?} leaked");
    }
}

#[test]
fn only_allowlisted_typed_configuration_and_platform_fields_survive() {
    let summary = build_structural_summary(
        fixed_input()
            .with_platform(PlatformCategory::Network)
            .with_model(ModelId::parse("模型-安全").unwrap()),
        sensitive([SensitiveValue::prompt(
            "自由文本\r\n\0\u{1b}\u{0085}\u{202e}👩‍💻",
        )]),
    )
    .unwrap();
    let value = serde_json::to_value(summary.summary()).unwrap();
    let text = value["text"].as_str().unwrap();
    assert!(text.contains("protocol=anthropic_messages"));
    assert!(text.contains("stage=response"));
    assert!(text.contains("retry=not_retried"));
    assert!(text.contains("failover=not_selected"));
    assert!(text.contains("streaming=true"));
    assert!(text.contains("platform=network"));
    assert!(text.contains("model=模型-安全"));
    assert!(!text.contains("自由文本"));
    assert_eq!(value["truncated"], false);
    assert!(ModelId::parse("unsafe\u{0000}").is_err());
    assert!(ModelId::parse("unsafe\u{202e}").is_err());
}

#[test]
fn transient_sensitive_inputs_have_constant_debug_and_no_display_leak() {
    let first = SensitiveValue::raw_credential_bytes(b"\0secret-a");
    let second = SensitiveValue::raw_credential_bytes(b"\xffdifferent-secret-b");
    assert_eq!(format!("{first:?}"), format!("{second:?}"));
    assert_eq!(format!("{first:?}"), "SensitiveValue([redacted])");
    assert_eq!(
        format!("{:?}", sensitive([first])),
        format!("{:?}", sensitive([second]))
    );

    let first_result = build_structural_summary(fixed_input(), sensitive([first])).unwrap();
    let second_result = build_structural_summary(fixed_input(), sensitive([second])).unwrap();
    assert_eq!(format!("{first_result:?}"), format!("{second_result:?}"));

    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/redaction.rs"),
    )
    .unwrap();
    assert!(!source.contains("impl fmt::Display for SensitiveValue"));
    assert!(!source.contains("impl std::fmt::Display for SensitiveValue"));
}

#[test]
fn production_summary_api_has_no_padding_or_detached_redaction_escape() {
    let input = StructuralSummaryInput::new(
        ProviderProtocol::OpenAiResponses,
        StageCode::Routing,
        RetryDecision::NotApplicable,
        FailoverDecision::NotApplicable,
        false,
    )
    .with_platform(PlatformCategory::Network)
    .with_model(ModelId::parse("模型-安全").unwrap());
    let bound = build_structural_summary(
        input,
        sensitive([SensitiveValue::authorization("Bearer credential-canary")]),
    )
    .unwrap();
    let visible = serde_json::to_string(bound.summary()).unwrap();
    assert!(visible.contains("模型-安全"));
    assert!(!visible.contains("credential-canary"));

    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/redaction.rs"),
    )
    .unwrap();
    for escape in [
        "SafeSummaryGlyph",
        "padding_units",
        "pub fn into_parts",
        "pub fn counts(",
    ] {
        assert!(!source.contains(escape), "{escape}");
    }
    let event_source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/event.rs"),
    )
    .unwrap();
    assert!(!event_source.contains("pub fn with_summaries"));
    assert!(!event_source.contains("pub fn with_redaction_counts"));
}
