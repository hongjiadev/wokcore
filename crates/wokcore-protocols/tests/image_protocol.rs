use serde_json::{Value, json};
use wokcore_protocols::images::{
    ImageEditMetadata, ImageGenerationRequest, validate_image_response,
};

#[test]
fn generation_request_validates_and_rewrites_only_the_routed_model() {
    let request = ImageGenerationRequest::decode(
        br#"{
            "model":"public-image",
            "prompt":"draw a tiny lighthouse",
            "n":2,
            "size":"1024x1024",
            "response_format":"b64_json",
            "vendor_extension":{"keep":true}
        }"#,
    )
    .unwrap();

    assert_eq!(request.model(), "public-image");
    assert_eq!(request.prompt_bytes(), 22);
    let encoded = request.encode_with_model("upstream-image").unwrap();
    let encoded: Value = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(encoded["model"], "upstream-image");
    assert_eq!(encoded["prompt"], "draw a tiny lighthouse");
    assert_eq!(encoded["vendor_extension"], json!({"keep": true}));
}

#[test]
fn generation_request_rejects_missing_or_unbounded_fields() {
    assert!(ImageGenerationRequest::decode(br#"{"model":"image-1"}"#).is_err());
    assert!(ImageGenerationRequest::decode(br#"{"model":"image-1","prompt":"x","n":11}"#).is_err());
    let oversized_model = format!(r#"{{"model":"{}","prompt":"x"}}"#, "m".repeat(257));
    assert!(ImageGenerationRequest::decode(oversized_model.as_bytes()).is_err());
}

#[test]
fn edit_metadata_accepts_openai_fields_and_rejects_unknown_or_duplicate_fields() {
    let metadata = ImageEditMetadata::from_fields([
        ("model", "public-image"),
        ("prompt", "remove the background"),
        ("n", "1"),
        ("size", "1024x1024"),
        ("response_format", "url"),
        ("user", "local-user"),
    ])
    .unwrap();

    assert_eq!(metadata.model(), "public-image");
    assert_eq!(metadata.prompt(), "remove the background");
    assert_eq!(metadata.fields().len(), 6);
    assert!(
        ImageEditMetadata::from_fields([
            ("model", "image-1"),
            ("prompt", "x"),
            ("unexpected", "secret"),
        ])
        .is_err()
    );
    assert!(
        ImageEditMetadata::from_fields([
            ("model", "image-1"),
            ("model", "image-2"),
            ("prompt", "x"),
        ])
        .is_err()
    );
}

#[test]
fn image_response_validation_borrows_large_payloads_and_checks_shape() {
    validate_image_response(
        br#"{"created":1722000000,"data":[{"url":"https://example.invalid/image.png","revised_prompt":"safe"}]}"#,
    )
    .unwrap();
    validate_image_response(br#"{"data":[{"b64_json":"aGVsbG8="}]}"#).unwrap();

    assert!(validate_image_response(br#"{"data":[]}"#).is_err());
    assert!(validate_image_response(br#"{"data":[{"url":"x","b64_json":"eA=="}]}"#).is_err());
    assert!(validate_image_response(br#"{"data":[{"unexpected":"x"}]}"#).is_err());
}

#[test]
fn image_debug_output_never_contains_prompt_or_response_bytes() {
    let request =
        ImageGenerationRequest::decode(br#"{"model":"image-1","prompt":"private prompt"}"#)
            .unwrap();
    let debug = format!("{request:?}");

    assert!(!debug.contains("private prompt"));
    assert!(debug.contains("prompt_bytes"));
}
