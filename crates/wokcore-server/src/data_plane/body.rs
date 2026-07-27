use axum::{
    body::Bytes,
    extract::{FromRequest, Multipart, Request},
    http::StatusCode,
    response::Response,
};
use tokio::io::AsyncWriteExt;
use wokcore_protocols::images::ImageEditMetadata;

use crate::api::RequestId;

use super::{
    ClientProtocol, ImageEditRequest, ImageInputFile,
    response::{invalid_body, invalid_request, payload_too_large},
};

pub(crate) const JSON_BODY_LIMIT: usize = 16 * 1024 * 1024;
pub(crate) const IMAGE_PART_BODY_LIMIT: usize = 20 * 1024 * 1024;
pub(crate) const IMAGE_MULTIPART_BODY_LIMIT: usize = 50 * 1024 * 1024;
pub(crate) const IMAGE_MULTIPART_WIRE_LIMIT: usize = 51 * 1024 * 1024;

pub(crate) struct ValidatedJsonBody(Bytes);

impl ValidatedJsonBody {
    pub(crate) fn into_bytes(self) -> Bytes {
        self.0
    }
}

impl<S> FromRequest<S> for ValidatedJsonBody
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let (request_id, protocol) = request_context(&request);
        let body = Bytes::from_request(request, state)
            .await
            .map_err(|rejection| {
                if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
                    payload_too_large(request_id, protocol)
                } else {
                    invalid_body(request_id, protocol)
                }
            })?;
        Ok(Self(body))
    }
}

pub(crate) struct ValidatedEmptyBody;

impl<S> FromRequest<S> for ValidatedEmptyBody
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let (request_id, protocol) = request_context(&request);
        let body = Bytes::from_request(request, state)
            .await
            .map_err(|_| invalid_body(request_id, protocol))?;
        if !body.is_empty() {
            return Err(invalid_request(request_id, protocol));
        }
        Ok(Self)
    }
}

pub(crate) struct ValidatedImageEditBody(ImageEditRequest);

impl ValidatedImageEditBody {
    pub(crate) fn into_request(self) -> ImageEditRequest {
        self.0
    }
}

impl<S> FromRequest<S> for ValidatedImageEditBody
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let (request_id, protocol) = request_context(&request);
        let mut multipart = Multipart::from_request(request, state)
            .await
            .map_err(|_| invalid_request(request_id, protocol))?;
        let mut total_bytes = 0_usize;
        let mut text_fields = Vec::new();
        let mut files = Vec::new();
        loop {
            let field = multipart
                .next_field()
                .await
                .map_err(|error| multipart_error(error.status(), request_id, protocol))?;
            let Some(mut field) = field else {
                break;
            };
            let name = field
                .name()
                .filter(|name| !name.is_empty() && name.len() <= 128)
                .ok_or_else(|| invalid_request(request_id, protocol))?
                .to_owned();
            let file_name = field.file_name().map(sanitize_file_name);
            let content_type = field
                .content_type()
                .map(str::to_owned)
                .unwrap_or_else(|| "application/octet-stream".to_owned());
            let mut field_bytes = 0_usize;
            if let Some(file_name) = file_name {
                let named = tempfile::Builder::new()
                    .prefix("wokcore-image-")
                    .tempfile()
                    .map_err(|_| invalid_body(request_id, protocol))?;
                let (file, path) = named.into_parts();
                let mut file = tokio::fs::File::from_std(file);
                while let Some(chunk) = field
                    .chunk()
                    .await
                    .map_err(|error| multipart_error(error.status(), request_id, protocol))?
                {
                    if !account_chunk(chunk.len(), &mut field_bytes, &mut total_bytes) {
                        return Err(payload_too_large(request_id, protocol));
                    }
                    file.write_all(&chunk)
                        .await
                        .map_err(|_| invalid_body(request_id, protocol))?;
                }
                file.flush()
                    .await
                    .map_err(|_| invalid_body(request_id, protocol))?;
                drop(file);
                files.push(ImageInputFile::new(
                    normalize_file_field(&name),
                    file_name,
                    content_type,
                    u64::try_from(field_bytes)
                        .map_err(|_| payload_too_large(request_id, protocol))?,
                    path,
                ));
            } else {
                let mut value = Vec::new();
                while let Some(chunk) = field
                    .chunk()
                    .await
                    .map_err(|error| multipart_error(error.status(), request_id, protocol))?
                {
                    if !account_chunk(chunk.len(), &mut field_bytes, &mut total_bytes) {
                        return Err(payload_too_large(request_id, protocol));
                    }
                    if field_bytes > 64 * 1024 {
                        return Err(payload_too_large(request_id, protocol));
                    }
                    value.extend_from_slice(&chunk);
                }
                let value =
                    String::from_utf8(value).map_err(|_| invalid_request(request_id, protocol))?;
                text_fields.push((name, value));
            }
        }
        let metadata = ImageEditMetadata::from_fields(
            text_fields
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        )
        .map_err(|_| invalid_request(request_id, protocol))?;
        let request = ImageEditRequest::new(metadata, files)
            .map_err(|_| invalid_request(request_id, protocol))?;
        Ok(Self(request))
    }
}

fn account_chunk(chunk_bytes: usize, field_bytes: &mut usize, total_bytes: &mut usize) -> bool {
    *field_bytes = field_bytes.saturating_add(chunk_bytes);
    *total_bytes = total_bytes.saturating_add(chunk_bytes);
    *field_bytes <= IMAGE_PART_BODY_LIMIT && *total_bytes <= IMAGE_MULTIPART_BODY_LIMIT
}

fn sanitize_file_name(value: &str) -> String {
    let leaf = value
        .rsplit(['/', '\\'])
        .next()
        .filter(|leaf| !leaf.is_empty())
        .unwrap_or("image");
    let sanitized = leaf
        .chars()
        .filter(|character| character.is_ascii_graphic() && !matches!(character, '"' | '\\'))
        .take(128)
        .collect::<String>();
    if sanitized.is_empty() {
        "image".to_owned()
    } else {
        sanitized
    }
}

fn normalize_file_field(name: &str) -> String {
    if name == "image[]" {
        "image".to_owned()
    } else {
        name.to_owned()
    }
}

fn multipart_error(
    status: StatusCode,
    request_id: RequestId,
    protocol: ClientProtocol,
) -> Response {
    if status == StatusCode::PAYLOAD_TOO_LARGE {
        payload_too_large(request_id, protocol)
    } else {
        invalid_body(request_id, protocol)
    }
}

fn request_context(request: &Request) -> (RequestId, ClientProtocol) {
    let request_id = *request
        .extensions()
        .get::<RequestId>()
        .expect("request ID middleware runs first");
    let protocol = *request
        .extensions()
        .get::<ClientProtocol>()
        .expect("security middleware classifies every data-plane route");
    (request_id, protocol)
}
