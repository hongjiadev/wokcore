use std::{fs, path::PathBuf};

use serde_json::{Value, json};
use wokcore_core::secret::SecretRef;
use wokcore_server::providers::ProviderCandidate;

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
    let expected = [
        ("/wokcore/v1/capabilities", "get"),
        ("/wokcore/v1/clients/authorize", "post"),
        ("/wokcore/v1/clients/{client_id}/tokens/{token_id}", "get"),
        (
            "/wokcore/v1/clients/{client_id}/tokens/{token_id}",
            "delete",
        ),
        ("/wokcore/v1/health", "get"),
        ("/wokcore/v1/diagnostics/export", "get"),
        ("/wokcore/v1/logs", "get"),
        ("/wokcore/v1/service/drain", "post"),
        ("/wokcore/v1/service/drain/cancel", "post"),
        ("/wokcore/v1/service/status", "get"),
        ("/wokcore/v1/service/stop", "post"),
        ("/wokcore/v1/sessions", "get"),
        ("/wokcore/v1/sessions/{session_key}/messages", "get"),
        ("/wokcore/v1/usage", "get"),
        ("/wokcore/v1/providers/catalog", "get"),
        ("/wokcore/v1/providers/runtime", "get"),
        ("/wokcore/v1/providers/models", "get"),
        ("/wokcore/v1/providers/config/validate", "post"),
        ("/wokcore/v1/providers/config", "put"),
        ("/wokcore/v1/providers/reload", "post"),
        ("/wokcore/v1/provider-secrets", "post"),
        ("/wokcore/v1/provider-secrets/{secret_ref}", "put"),
        ("/wokcore/v1/provider-secrets/{secret_ref}", "delete"),
        ("/v1/responses", "post"),
        ("/v1/chat/completions", "post"),
        ("/v1/messages", "post"),
        ("/v1/messages/count_tokens", "post"),
        ("/v1/models", "get"),
        ("/v1/images/generations", "post"),
        ("/v1/images/edits", "post"),
    ];
    let paths = document["paths"].as_object().unwrap();
    assert_eq!(paths.len(), 28);
    for (path, method) in expected {
        let operation = &paths[path][method];
        assert!(operation.is_object(), "missing {method} {path}");
        let security = operation["security"].clone();
        if path.starts_with("/v1/") {
            assert_eq!(security, json!([{"clientBearer":[]}]));
            assert_eq!(operation["x-wokcore-required-scope"], "proxy.use");
        } else if matches!(path, "/wokcore/v1/health" | "/wokcore/v1/capabilities") {
            assert_eq!(security, json!([]));
        } else {
            assert_eq!(
                security,
                json!([{"managementBearer":[]},{"clientBearer":[]}])
            );
            let required_scope = match path {
                "/wokcore/v1/service/status" => "service.read",
                "/wokcore/v1/service/drain"
                | "/wokcore/v1/service/drain/cancel"
                | "/wokcore/v1/service/stop" => "service.control",
                "/wokcore/v1/clients/authorize"
                | "/wokcore/v1/clients/{client_id}/tokens/{token_id}" => "clients.manage",
                "/wokcore/v1/sessions" | "/wokcore/v1/sessions/{session_key}/messages" => {
                    "sessions.read"
                }
                "/wokcore/v1/usage" => "usage.read",
                "/wokcore/v1/logs" => "diagnostics.read",
                "/wokcore/v1/diagnostics/export" => "diagnostics.export",
                "/wokcore/v1/providers/catalog"
                | "/wokcore/v1/providers/runtime"
                | "/wokcore/v1/providers/models" => "providers.read",
                "/wokcore/v1/providers/config/validate"
                | "/wokcore/v1/providers/config"
                | "/wokcore/v1/providers/reload"
                | "/wokcore/v1/provider-secrets"
                | "/wokcore/v1/provider-secrets/{secret_ref}" => "providers.write",
                _ => panic!("missing required scope for {method} {path}"),
            };
            assert_eq!(operation["x-wokcore-required-scope"], required_scope);
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
        if path == "/wokcore/v1/diagnostics/export" {
            assert_export_headers(&success["headers"]);
        } else {
            assert_safe_headers(&success["headers"]);
        }
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
    assert_eq!(
        document["components"]["schemas"]["ClientTokenScopes"],
        json!({
            "type": "array",
            "minItems": 1,
            "maxItems": 10,
            "uniqueItems": true,
            "items": {
                "type": "string",
                "enum": [
                    "proxy.use",
                    "sessions.read",
                    "usage.read",
                    "diagnostics.read",
                    "diagnostics.export",
                    "service.read",
                    "service.control",
                    "providers.read",
                    "providers.write",
                    "clients.manage"
                ]
            }
        })
    );
    assert_eq!(
        document["components"]["schemas"]["ProviderSecretCreate"]["properties"]["secret"]["writeOnly"],
        true
    );
    assert_eq!(
        document["components"]["schemas"]["ProviderCandidate"]["additionalProperties"],
        false
    );
    assert_eq!(
        document["components"]["schemas"]["ProviderInstance"]["required"],
        json!(["id", "catalog_id", "enabled", "allow_private_network"])
    );
    assert_eq!(
        document["components"]["schemas"]["ProviderAccountAuth"]["oneOf"][1]["required"],
        json!(["kind", "access"])
    );
    assert_eq!(
        document["components"]["schemas"]["RouteRule"]["required"],
        json!(["target"])
    );
    assert_eq!(
        document["components"]["schemas"]["RoutingConfig"]["required"],
        json!(["aliases", "rules"])
    );
    assert_eq!(
        document["components"]["schemas"]["SecretRef"]["oneOf"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    serde_json::from_value::<ProviderCandidate>(json!({
        "providers": {
            "instances": [{
                "id": "primary",
                "catalog_id": "openai-apikey",
                "enabled": true,
                "allow_private_network": false
            }],
            "accounts": [{
                "id": "primary",
                "provider": "primary",
                "enabled": true,
                "auth": {
                    "kind": "oauth",
                    "access": "secret:00000000-0000-0000-0000-000000000001"
                }
            }]
        },
        "routing": {
            "aliases": [],
            "rules": [{
                "target": {"provider": "primary", "model": "gpt-5.6"}
            }]
        }
    }))
    .expect("serde accepts the OpenAPI-optional Provider fields when absent");
    for secret_ref in [
        "secret:00000000000000000000000000000001",
        "secret:00000000-0000-0000-0000-000000000001",
        "secret:{00000000-0000-0000-0000-000000000001}",
        "secret:urn:uuid:00000000-0000-0000-0000-000000000001",
    ] {
        SecretRef::parse(secret_ref).expect("OpenAPI SecretRef representation must parse");
    }
    assert_eq!(
        document["paths"]["/wokcore/v1/sessions"]["get"]["parameters"][3]["schema"],
        json!({"type":"integer","minimum":1,"maximum":200,"default":50})
    );
    assert_eq!(
        document["paths"]["/wokcore/v1/sessions/{session_key}/messages"]["get"]["parameters"][3]["schema"],
        json!({"type":"integer","minimum":4096,"maximum":1048576,"default":262144})
    );
    assert_eq!(
        document["paths"]["/wokcore/v1/diagnostics/export"]["get"]["parameters"][6]["schema"],
        json!({"type":"integer","minimum":65536,"maximum":67108864,"default":16777216})
    );
    for path in ["/v1/responses", "/v1/chat/completions", "/v1/messages"] {
        let operation = &document["paths"][path]["post"];
        assert_eq!(operation["x-wokcore-max-body-bytes"], 16 * 1024 * 1024);
        assert!(operation["responses"]["200"]["content"]["application/json"].is_object());
        assert!(operation["responses"]["200"]["content"]["text/event-stream"].is_object());
    }
    assert_eq!(
        document["paths"]["/v1/messages/count_tokens"]["post"]["x-wokcore-max-body-bytes"],
        16 * 1024 * 1024
    );
    assert_eq!(
        document["paths"]["/v1/images/generations"]["post"]["x-wokcore-max-body-bytes"],
        16 * 1024 * 1024
    );
    assert_eq!(
        document["paths"]["/v1/images/edits"]["post"]["x-wokcore-max-body-bytes"],
        51 * 1024 * 1024
    );
    assert_eq!(
        document["components"]["schemas"]["ImageEditRequest"]["properties"]["image"]["maxLength"],
        20 * 1024 * 1024
    );
    assert_eq!(
        document["components"]["schemas"]["ImageEditRequest"]["x-wokcore-max-image-bytes"],
        20 * 1024 * 1024
    );
    assert_eq!(
        document["components"]["schemas"]["ImageEditRequest"]["x-wokcore-max-total-file-bytes"],
        50 * 1024 * 1024
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

fn assert_export_headers(headers: &Value) {
    assert_eq!(
        headers,
        &json!({
            "X-Request-Id":{"$ref":"#/components/headers/XRequestId"},
            "Cache-Control":{"$ref":"#/components/headers/CacheControl"},
            "X-Content-Type-Options":{"$ref":"#/components/headers/XContentTypeOptions"},
            "Content-Disposition":{"$ref":"#/components/headers/ContentDisposition"}
        })
    );
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
