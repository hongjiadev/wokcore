use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

const CURSOR_VERSION: u8 = 1;
const CURSOR_TAG_BYTES: usize = 16;
const MAX_CURSOR_BYTES: usize = 8 * 1024;

#[derive(Deserialize, Serialize)]
struct CursorEnvelope<T> {
    version: u8,
    route: String,
    binding: String,
    payload: T,
}

pub(super) fn encode<T: Serialize>(
    key: &[u8; 32],
    route: &str,
    binding: &str,
    payload: T,
) -> Result<String, CursorError> {
    let envelope = CursorEnvelope {
        version: CURSOR_VERSION,
        route: route.to_owned(),
        binding: binding.to_owned(),
        payload,
    };
    let payload = serde_json::to_vec(&envelope).map_err(|_| CursorError)?;
    if payload.len() > MAX_CURSOR_BYTES.saturating_sub(CURSOR_TAG_BYTES) {
        return Err(CursorError);
    }
    let tag = cursor_tag(key, &payload);
    let mut token = Vec::with_capacity(payload.len() + CURSOR_TAG_BYTES);
    token.extend_from_slice(&payload);
    token.extend_from_slice(&tag[..CURSOR_TAG_BYTES]);
    Ok(URL_SAFE_NO_PAD.encode(token))
}

pub(super) fn decode<T: DeserializeOwned>(
    key: &[u8; 32],
    route: &str,
    binding: &str,
    token: &str,
) -> Result<T, CursorError> {
    if token.is_empty() || token.len() > MAX_CURSOR_BYTES * 2 {
        return Err(CursorError);
    }
    let decoded = URL_SAFE_NO_PAD.decode(token).map_err(|_| CursorError)?;
    if decoded.len() <= CURSOR_TAG_BYTES || decoded.len() > MAX_CURSOR_BYTES {
        return Err(CursorError);
    }
    let (payload, provided_tag) = decoded.split_at(decoded.len() - CURSOR_TAG_BYTES);
    let expected_tag = cursor_tag(key, payload);
    if provided_tag
        .ct_eq(&expected_tag[..CURSOR_TAG_BYTES])
        .unwrap_u8()
        != 1
    {
        return Err(CursorError);
    }
    let envelope = serde_json::from_slice::<CursorEnvelope<T>>(payload).map_err(|_| CursorError)?;
    if envelope.version != CURSOR_VERSION
        || envelope.route.as_bytes() != route.as_bytes()
        || envelope.binding.as_bytes() != binding.as_bytes()
    {
        return Err(CursorError);
    }
    Ok(envelope.payload)
}

fn cursor_tag(key: &[u8; 32], payload: &[u8]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"wokcore.control-plane-cursor.v1");
    digest.update(key);
    digest.update(payload);
    digest.update(key);
    digest.finalize().into()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CursorError;

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    use super::{decode, encode};

    #[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
    struct Payload {
        key: String,
    }

    #[test]
    fn cursor_rejects_tampering_and_cross_query_reuse() {
        let key = [7_u8; 32];
        let cursor = encode(
            &key,
            "sessions",
            "source=codex",
            Payload {
                key: "opaque".to_owned(),
            },
        )
        .unwrap();
        assert_eq!(
            decode::<Payload>(&key, "sessions", "source=codex", &cursor).unwrap(),
            Payload {
                key: "opaque".to_owned()
            }
        );
        assert!(decode::<Payload>(&key, "usage", "source=codex", &cursor).is_err());
        assert!(decode::<Payload>(&key, "sessions", "source=claude", &cursor).is_err());

        let mut tampered = cursor.into_bytes();
        let index = tampered.len() / 2;
        tampered[index] = if tampered[index] == b'A' { b'B' } else { b'A' };
        let tampered = String::from_utf8(tampered).unwrap();
        assert!(decode::<Payload>(&key, "sessions", "source=codex", &tampered).is_err());
    }
}
