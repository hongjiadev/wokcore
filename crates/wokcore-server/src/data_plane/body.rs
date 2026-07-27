use axum::{
    body::Bytes,
    extract::{FromRequest, Multipart, Request},
    http::StatusCode,
    response::Response,
};

use crate::api::RequestId;

use super::{
    ClientProtocol,
    response::{invalid_body, invalid_request, payload_too_large},
};

pub(crate) const JSON_BODY_LIMIT: usize = 16 * 1024 * 1024;
pub(crate) const IMAGE_PART_BODY_LIMIT: usize = 20 * 1024 * 1024;
pub(crate) const IMAGE_MULTIPART_BODY_LIMIT: usize = 50 * 1024 * 1024;

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

pub(crate) struct ValidatedImageEditBody;

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
        loop {
            let field = multipart
                .next_field()
                .await
                .map_err(|error| multipart_error(error.status(), request_id, protocol))?;
            let Some(mut field) = field else {
                break;
            };
            let mut field_bytes = 0_usize;
            while let Some(chunk) = field
                .chunk()
                .await
                .map_err(|error| multipart_error(error.status(), request_id, protocol))?
            {
                field_bytes = field_bytes.saturating_add(chunk.len());
                if field_bytes > IMAGE_PART_BODY_LIMIT {
                    return Err(payload_too_large(request_id, protocol));
                }
            }
        }
        Ok(Self)
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
