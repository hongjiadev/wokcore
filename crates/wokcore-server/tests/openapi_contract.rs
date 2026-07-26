use std::{collections::BTreeMap, fs, path::PathBuf};

use serde_json::{Value, json};

#[test]
fn openapi_31_matches_the_exact_control_plane_contract_without_secret_examples() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../openapi/wokcore-v1.json");
    let bytes = fs::read(&path).unwrap();
    let document: Value = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(document["openapi"], "3.1.0");
    assert_eq!(
        document["servers"][0]["variables"]["port"]["default"],
        "43127"
    );
    assert_eq!(
        document["components"]["securitySchemes"]["managementBearer"],
        json!({"type":"http","scheme":"bearer","bearerFormat":"WokCore management token"})
    );
    let expected = BTreeMap::from([
        ("/wokcore/v1/capabilities", "get"),
        ("/wokcore/v1/clients/authorize", "post"),
        (
            "/wokcore/v1/clients/{client_id}/tokens/{token_id}",
            "delete",
        ),
        ("/wokcore/v1/health", "get"),
        ("/wokcore/v1/service/drain", "post"),
        ("/wokcore/v1/service/drain/cancel", "post"),
        ("/wokcore/v1/service/status", "get"),
        ("/wokcore/v1/service/stop", "post"),
    ]);
    let paths = document["paths"].as_object().unwrap();
    assert_eq!(paths.len(), expected.len());
    for (path, method) in expected {
        let operation = &paths[path][method];
        assert!(operation.is_object(), "missing {method} {path}");
        let security = operation["security"].clone();
        if matches!(path, "/wokcore/v1/health" | "/wokcore/v1/capabilities") {
            assert_eq!(security, json!([]));
        } else {
            assert_eq!(security, json!([{"managementBearer":[]}]));
        }
        assert_eq!(
            operation["responses"]["default"]["$ref"],
            "#/components/responses/Error"
        );
        let success = operation["responses"]
            .as_object()
            .unwrap()
            .iter()
            .find(|(status, _)| status.starts_with('2'))
            .unwrap()
            .1;
        assert_safe_headers(&success["headers"]);
    }
    assert_safe_headers(&document["components"]["responses"]["Error"]["headers"]);
    assert_eq!(
        document["components"]["responses"]["Error"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/ErrorEnvelope"
    );
    assert_eq!(
        document["components"]["schemas"]["ErrorEnvelope"]["required"],
        json!(["error"])
    );
    assert_eq!(
        document["components"]["schemas"]["Lifecycle"]["properties"]["phase"]["enum"],
        json!([
            "starting",
            "running",
            "draining",
            "awaiting_cancellation",
            "stopping"
        ])
    );
    assert_eq!(
        document["components"]["schemas"]["AuthorizeResponse"]["properties"]["token"]["readOnly"],
        true
    );
    let rendered = String::from_utf8(bytes).unwrap().to_ascii_lowercase();
    for forbidden in [
        "wok_admin_v1_",
        "wok_proxy_v1_",
        "\"example\"",
        "\"examples\"",
        "authorization:",
        "cookie",
    ] {
        assert!(!rendered.contains(forbidden), "found {forbidden}");
    }
}

fn assert_safe_headers(headers: &Value) {
    assert_eq!(
        headers,
        &json!({
            "X-Request-Id":{"$ref":"#/components/headers/XRequestId"},
            "Cache-Control":{"$ref":"#/components/headers/CacheControl"},
            "X-Content-Type-Options":{"$ref":"#/components/headers/XContentTypeOptions"}
        })
    );
}
