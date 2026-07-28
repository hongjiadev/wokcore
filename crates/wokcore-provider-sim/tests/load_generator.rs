use std::time::Duration;

use wokcore_provider_sim::{
    LoadConfig, LoadPayloadProfile, LoadProtocol, Protocol, ProtocolWeight, Scenario, Simulator,
    run_load,
};

#[test]
fn load_generator_config_is_exact_bounded_and_accepts_observation_scale_without_a_cap() {
    let config = LoadConfig::new("http://127.0.0.1:43123")
        .unwrap()
        .with_concurrency(1_000)
        .with_ramp(Duration::from_secs(3))
        .with_duration(Duration::from_secs(30))
        .with_protocol_mix(vec![
            ProtocolWeight::new(LoadProtocol::Responses, 6),
            ProtocolWeight::new(LoadProtocol::Chat, 3),
            ProtocolWeight::new(LoadProtocol::Anthropic, 1),
        ])
        .with_payload_profile(LoadPayloadProfile::LongReasoning)
        .with_cancellation_permyriad(1_250)
        .with_slow_consumer_delay(Duration::from_millis(17));

    config.validate().unwrap();
    assert_eq!(config.concurrency(), 1_000);
    assert_eq!(config.ramp(), Duration::from_secs(3));
    assert_eq!(config.duration(), Duration::from_secs(30));
    assert_eq!(config.cancellation_permyriad(), 1_250);
    assert_eq!(config.slow_consumer_delay(), Duration::from_millis(17));
    assert_eq!(
        config.protocol_for_worker(0),
        config.protocol_for_worker(10)
    );

    assert!(LoadConfig::new("https://api.openai.com/v1").is_err());
    assert!(LoadConfig::new("http://localhost:43123").is_err());
    assert!(LoadConfig::new("http://0.0.0.0:43123").is_err());
    assert!(config.clone().with_concurrency(0).validate().is_err());
    assert!(
        config
            .clone()
            .with_cancellation_permyriad(10_001)
            .validate()
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn load_generator_opens_five_hundred_uncapped_streams_with_bounded_evidence() {
    let simulator = Simulator::start(
        "127.0.0.1:0".parse().unwrap(),
        Scenario::standard(Protocol::OpenAiResponses)
            .with_event_count(8)
            .with_ttft(Duration::from_millis(500))
            .with_cadence(Duration::from_millis(5)),
    )
    .await
    .unwrap();
    let config = LoadConfig::new(simulator.url("/").as_str())
        .unwrap()
        .with_concurrency(500)
        .with_ramp(Duration::from_millis(100))
        .with_duration(Duration::from_secs(15))
        .with_protocol_mix(vec![ProtocolWeight::new(LoadProtocol::Responses, 1)])
        .with_payload_profile(LoadPayloadProfile::Standard32K);

    let report = run_load(config).await.unwrap();
    assert_eq!(report.started(), 500);
    assert_eq!(report.peak_active(), 500);
    assert_eq!(report.active(), 0);
    assert_eq!(report.errors(), 0, "{report:?}");
    assert_eq!(report.completed(), 500, "{report:?}");
    assert!(report.bytes_received() > 0);
    assert_eq!(report.protocol_started()["responses"], 500);
    let simulator_summary = simulator.summary();
    assert_eq!(simulator_summary.started(), 500);
    assert_eq!(simulator_summary.active(), 0);
    assert_eq!(simulator_summary.peak_active(), 500);
    assert_eq!(simulator_summary.completed(), 500);

    let json = serde_json::to_string(&report).unwrap();
    assert!(json.len() < 16 * 1024);
    for forbidden in [
        "authorization",
        "input",
        "messages",
        "synthetic_tool",
        "xxxxxxxx",
    ] {
        assert!(!json.contains(forbidden));
    }
    simulator.shutdown().await.unwrap();
}

#[tokio::test]
async fn load_generator_reports_deterministic_cancellation_and_safe_errors() {
    let simulator = Simulator::start(
        "127.0.0.1:0".parse().unwrap(),
        Scenario::from_toml(include_str!("../scenarios/rate-limit.toml")).unwrap(),
    )
    .await
    .unwrap();
    let config = LoadConfig::new(simulator.url("/").as_str())
        .unwrap()
        .with_concurrency(20)
        .with_duration(Duration::from_secs(2))
        .with_cancellation_permyriad(2_500);
    let report = run_load(config).await.unwrap();
    assert_eq!(report.started(), 20);
    assert_eq!(
        report.completed() + report.cancelled() + report.errors(),
        20
    );
    assert!(report.cancelled() > 0);
    assert!(report.errors() > 0);
    assert!(report.error_samples().len() <= 16);
    assert!(
        report
            .error_samples()
            .iter()
            .all(|sample| sample.code() == "http_status")
    );
    simulator.shutdown().await.unwrap();
}
