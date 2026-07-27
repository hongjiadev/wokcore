use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::Duration,
};

use wokcore_provider_sim::{
    FrameMode, PayloadProfile, Protocol, Scenario, Simulator, validate_loopback_socket,
    validate_loopback_url,
};

#[test]
fn simulator_endpoints_must_be_literal_loopback_addresses() {
    assert!(validate_loopback_socket("127.0.0.1:0").is_ok());
    assert!(validate_loopback_socket("[::1]:43123").is_ok());
    assert!(validate_loopback_socket("0.0.0.0:43123").is_err());
    assert!(validate_loopback_socket("192.0.2.1:43123").is_err());
    assert!(validate_loopback_socket("localhost:43123").is_err());

    assert!(validate_loopback_url("http://127.0.0.1:43123/v1").is_ok());
    assert!(validate_loopback_url("http://[::1]:43123/v1").is_ok());
    assert!(validate_loopback_url("https://api.example.invalid/v1").is_err());
    assert!(validate_loopback_url("http://localhost:43123/v1").is_err());
}

#[test]
fn simulator_scenario_parsing_is_strict_bounded_and_inert() {
    let valid = r#"
protocol = "openai_responses"
stream = true
status = 200
ttft_ms = 10
cadence_ms = 2
jitter_ms = 1
event_count = 4
chunk_bytes = 128
terminal = true
seed = 42
frame_mode = "partial"
disconnect_after_chunks = 3
headers = [{ name = "x-ratelimit-remaining", value = "7" }]
"#;
    let scenario = Scenario::from_toml(valid).unwrap();
    assert_eq!(scenario.protocol(), Protocol::OpenAiResponses);
    assert_eq!(scenario.frame_mode(), FrameMode::Partial);

    assert!(Scenario::from_toml(&format!("{valid}\ncommand = \"calc.exe\"\n")).is_err());
    assert!(
        Scenario::from_toml(&valid.replace("event_count = 4", "event_count = 1000001")).is_err()
    );
    assert!(Scenario::from_toml(&valid.replace("chunk_bytes = 128", "chunk_bytes = 0")).is_err());
}

#[test]
fn simulator_fixed_seed_produces_byte_identical_protocol_schedules() {
    for protocol in [
        Protocol::OpenAiResponses,
        Protocol::OpenAiChat,
        Protocol::Anthropic,
        Protocol::Gemini,
        Protocol::AzureOpenAi,
    ] {
        let scenario = Scenario::standard(protocol)
            .with_event_count(6)
            .with_ttft(Duration::from_millis(7))
            .with_cadence(Duration::from_millis(3))
            .with_jitter(Duration::from_millis(2))
            .with_seed(9182);
        let first = scenario.schedule().unwrap();
        let second = scenario.schedule().unwrap();
        assert_eq!(first, second);
        assert!(!first.chunks().is_empty());
        assert!(first.total_bytes() > 0);
        assert!(first.chunks()[0].delay() >= Duration::from_millis(7));
    }
}

#[test]
fn simulator_frame_modes_cover_partial_coalesced_malformed_and_utf8_splits() {
    for mode in [
        FrameMode::Partial,
        FrameMode::Coalesced,
        FrameMode::Malformed,
        FrameMode::Utf8Split,
    ] {
        let schedule = Scenario::standard(Protocol::OpenAiResponses)
            .with_event_count(4)
            .with_frame_mode(mode)
            .schedule()
            .unwrap();
        assert!(!schedule.chunks().is_empty());
        assert!(schedule.total_bytes() > 0);
    }
}

#[tokio::test]
async fn simulator_serves_protocol_shapes_and_shuts_down_cleanly() {
    let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let simulator = Simulator::start(bind, Scenario::standard(Protocol::Anthropic))
        .await
        .unwrap();
    let response = reqwest::Client::new()
        .post(simulator.url("/v1/messages"))
        .body(r#"{"model":"synthetic","messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();
    assert!(body.contains("message_start"));

    let summary = simulator.summary();
    assert_eq!(summary.started(), 1);
    assert_eq!(summary.active(), 0);
    simulator.shutdown().await.unwrap();
}

#[test]
fn profile_fixtures_cover_bounded_standard_reasoning_tool_and_failure_shapes() {
    let standard = Scenario::from_toml(include_str!("../scenarios/standard.toml")).unwrap();
    let slow = Scenario::from_toml(include_str!("../scenarios/slow-stream.toml")).unwrap();
    let malformed = Scenario::from_toml(include_str!("../scenarios/malformed.toml")).unwrap();
    let cancellation = Scenario::from_toml(include_str!("../scenarios/cancellation.toml")).unwrap();

    assert_eq!(standard.protocol(), Protocol::OpenAiChat);
    assert_eq!(standard.payload_profile(), PayloadProfile::Standard);
    assert!(standard.schedule().unwrap().total_bytes() >= 32 * 1024);
    assert_eq!(slow.protocol(), Protocol::OpenAiChat);
    assert_eq!(slow.payload_profile(), PayloadProfile::Reasoning);
    assert!(slow.schedule().unwrap().total_bytes() >= 1024 * 1024);
    assert_eq!(cancellation.payload_profile(), PayloadProfile::Tool);
    assert!(cancellation.schedule().unwrap().chunks().len() <= 128);
    assert!(malformed.schedule().unwrap().chunks().iter().any(|chunk| {
        chunk
            .bytes()
            .windows(10)
            .any(|value| value == b"{malformed")
    }));
}

#[test]
fn chat_streams_emit_usage_before_the_terminal_done_marker() {
    let schedule = Scenario::standard(Protocol::OpenAiChat)
        .with_event_count(2)
        .schedule()
        .unwrap();
    let bytes = schedule
        .chunks()
        .iter()
        .flat_map(|chunk| chunk.bytes())
        .copied()
        .collect::<Vec<_>>();
    let rendered = String::from_utf8(bytes).unwrap();
    let usage = rendered.find("\"usage\"").unwrap();
    let done = rendered.find("data: [DONE]").unwrap();

    assert!(usage < done);
    assert!(rendered.contains("\"choices\":[]"));
}

#[tokio::test]
async fn profile_rate_limit_and_failover_are_protocol_shaped_and_deterministic() {
    let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let rate_limit = Simulator::start(
        bind,
        Scenario::from_toml(include_str!("../scenarios/rate-limit.toml")).unwrap(),
    )
    .await
    .unwrap();
    let response = reqwest::Client::new()
        .post(rate_limit.url("/v1/chat/completions"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 429);
    assert_eq!(response.headers()["retry-after"], "2");
    assert!(response.text().await.unwrap().contains("synthetic_error"));
    rate_limit.shutdown().await.unwrap();

    let failover = Simulator::start(
        bind,
        Scenario::from_toml(include_str!("../scenarios/failover.toml")).unwrap(),
    )
    .await
    .unwrap();
    let client = reqwest::Client::new();
    let first = client
        .post(failover.url("/v1/responses"))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 500);
    let second = client
        .post(failover.url("/v1/responses"))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 200);
    assert!(second.text().await.unwrap().contains("response.created"));
    assert_eq!(failover.summary().started(), 2);
    failover.shutdown().await.unwrap();
}
