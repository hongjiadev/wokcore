use std::{collections::BTreeMap, fmt};

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::canonical::GatewayError;

const MAX_MODEL_BYTES: usize = 256;
const MAX_PROMPT_BYTES: usize = 32 * 1024;
const MAX_TEXT_FIELD_BYTES: usize = 32 * 1024;
const MAX_METADATA_BYTES: usize = 64 * 1024;
const MAX_FIELDS: usize = 64;
const MAX_IMAGES: usize = 10;
const MAX_IMAGE_RESPONSE_BYTES: usize = 50 * 1024 * 1024;
const MAX_URL_BYTES: usize = 8 * 1024;

#[derive(Clone)]
pub struct ImageGenerationRequest {
    fields: Map<String, Value>,
    model: String,
    prompt_bytes: usize,
}

impl ImageGenerationRequest {
    pub fn decode(body: &[u8]) -> Result<Self, GatewayError> {
        let Value::Object(fields) =
            serde_json::from_slice(body).map_err(|_| GatewayError::invalid_request())?
        else {
            return Err(GatewayError::invalid_request());
        };
        if fields.is_empty() || fields.len() > MAX_FIELDS {
            return Err(GatewayError::invalid_request());
        }
        let model = required_bounded_string(&fields, "model", MAX_MODEL_BYTES)?.to_owned();
        let prompt = required_bounded_text(&fields, "prompt", MAX_PROMPT_BYTES)?;
        let prompt_bytes = prompt.len();
        validate_optional_count(&fields, "n")?;
        for name in [
            "size",
            "quality",
            "style",
            "response_format",
            "output_format",
            "background",
            "moderation",
            "user",
        ] {
            validate_optional_string(&fields, name, MAX_TEXT_FIELD_BYTES)?;
        }
        Ok(Self {
            fields,
            model,
            prompt_bytes,
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub const fn prompt_bytes(&self) -> usize {
        self.prompt_bytes
    }

    pub fn encode_with_model(&self, model: &str) -> Result<Vec<u8>, GatewayError> {
        validate_string(model, MAX_MODEL_BYTES)?;
        let mut fields = self.fields.clone();
        fields.insert("model".to_owned(), Value::String(model.to_owned()));
        serde_json::to_vec(&Value::Object(fields)).map_err(|_| GatewayError::invalid_request())
    }
}

impl fmt::Debug for ImageGenerationRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImageGenerationRequest")
            .field("model", &self.model)
            .field("prompt_bytes", &self.prompt_bytes)
            .field("field_count", &self.fields.len())
            .finish()
    }
}

#[derive(Clone)]
pub struct ImageEditMetadata {
    fields: BTreeMap<String, String>,
    model: String,
    prompt: String,
}

impl ImageEditMetadata {
    pub fn from_fields<'a>(
        fields: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<Self, GatewayError> {
        let mut parsed = BTreeMap::new();
        let mut total_bytes = 0_usize;
        for (name, value) in fields {
            if !EDIT_FIELDS.contains(&name)
                || parsed.len() >= MAX_FIELDS
                || parsed.contains_key(name)
                || value.len() > MAX_TEXT_FIELD_BYTES
                || value.contains('\0')
                || (name != "prompt" && value.chars().any(char::is_control))
            {
                return Err(GatewayError::invalid_request());
            }
            total_bytes = total_bytes
                .checked_add(name.len())
                .and_then(|length| length.checked_add(value.len()))
                .ok_or_else(GatewayError::invalid_request)?;
            if total_bytes > MAX_METADATA_BYTES {
                return Err(GatewayError::invalid_request());
            }
            parsed.insert(name.to_owned(), value.to_owned());
        }
        let model = parsed
            .get("model")
            .ok_or_else(GatewayError::invalid_request)?
            .to_owned();
        validate_string(&model, MAX_MODEL_BYTES)?;
        let prompt = parsed
            .get("prompt")
            .ok_or_else(GatewayError::invalid_request)?
            .to_owned();
        validate_text(&prompt, MAX_PROMPT_BYTES)?;
        if let Some(count) = parsed.get("n") {
            let count = count
                .parse::<u8>()
                .map_err(|_| GatewayError::invalid_request())?;
            if !(1..=10).contains(&count) {
                return Err(GatewayError::invalid_request());
            }
        }
        Ok(Self {
            fields: parsed,
            model,
            prompt,
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    pub fn fields(&self) -> &BTreeMap<String, String> {
        &self.fields
    }
}

impl fmt::Debug for ImageEditMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImageEditMetadata")
            .field("model", &self.model)
            .field("prompt_bytes", &self.prompt.len())
            .field("field_count", &self.fields.len())
            .finish()
    }
}

const EDIT_FIELDS: &[&str] = &[
    "background",
    "input_fidelity",
    "model",
    "n",
    "output_format",
    "prompt",
    "quality",
    "response_format",
    "size",
    "user",
];

#[derive(Deserialize)]
struct BorrowedImageResponse<'a> {
    #[serde(default)]
    created: Option<u64>,
    #[serde(borrow)]
    data: Vec<BorrowedImageData<'a>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BorrowedImageData<'a> {
    #[serde(default, borrow)]
    url: Option<&'a str>,
    #[serde(default, borrow)]
    b64_json: Option<&'a str>,
    #[serde(default, borrow)]
    revised_prompt: Option<&'a str>,
}

pub fn validate_image_response(body: &[u8]) -> Result<(), GatewayError> {
    if body.len() > MAX_IMAGE_RESPONSE_BYTES {
        return Err(GatewayError::invalid_request());
    }
    let response: BorrowedImageResponse<'_> =
        serde_json::from_slice(body).map_err(|_| GatewayError::invalid_request())?;
    let _ = response.created;
    if response.data.is_empty() || response.data.len() > MAX_IMAGES {
        return Err(GatewayError::invalid_request());
    }
    for image in response.data {
        match (image.url, image.b64_json) {
            (Some(url), None) => {
                validate_string(url, MAX_URL_BYTES)?;
            }
            (None, Some(encoded)) if valid_base64(encoded) => {}
            _ => return Err(GatewayError::invalid_request()),
        }
        if let Some(prompt) = image.revised_prompt {
            validate_text(prompt, MAX_PROMPT_BYTES)?;
        }
    }
    Ok(())
}

fn required_bounded_string<'a>(
    fields: &'a Map<String, Value>,
    name: &str,
    maximum_bytes: usize,
) -> Result<&'a str, GatewayError> {
    let value = fields
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(GatewayError::invalid_request)?;
    validate_string(value, maximum_bytes)?;
    Ok(value)
}

fn required_bounded_text<'a>(
    fields: &'a Map<String, Value>,
    name: &str,
    maximum_bytes: usize,
) -> Result<&'a str, GatewayError> {
    let value = fields
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(GatewayError::invalid_request)?;
    validate_text(value, maximum_bytes)?;
    Ok(value)
}

fn validate_optional_string(
    fields: &Map<String, Value>,
    name: &str,
    maximum_bytes: usize,
) -> Result<(), GatewayError> {
    if let Some(value) = fields.get(name) {
        let value = value.as_str().ok_or_else(GatewayError::invalid_request)?;
        validate_string(value, maximum_bytes)?;
    }
    Ok(())
}

fn validate_optional_count(fields: &Map<String, Value>, name: &str) -> Result<(), GatewayError> {
    if let Some(value) = fields.get(name) {
        let value = value.as_u64().ok_or_else(GatewayError::invalid_request)?;
        if !(1..=10).contains(&value) {
            return Err(GatewayError::invalid_request());
        }
    }
    Ok(())
}

fn validate_string(value: &str, maximum_bytes: usize) -> Result<(), GatewayError> {
    if value.is_empty() || value.len() > maximum_bytes || value.chars().any(char::is_control) {
        return Err(GatewayError::invalid_request());
    }
    Ok(())
}

fn validate_text(value: &str, maximum_bytes: usize) -> Result<(), GatewayError> {
    if value.is_empty() || value.len() > maximum_bytes || value.contains('\0') {
        return Err(GatewayError::invalid_request());
    }
    Ok(())
}

fn valid_base64(value: &str) -> bool {
    if value.is_empty() || value.len() > MAX_IMAGE_RESPONSE_BYTES || !value.len().is_multiple_of(4)
    {
        return false;
    }
    let mut padding_started = false;
    let mut padding = 0_usize;
    for byte in value.bytes() {
        if byte == b'=' {
            padding_started = true;
            padding += 1;
            if padding > 2 {
                return false;
            }
        } else if padding_started || !(byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/'))
        {
            return false;
        }
    }
    true
}
