use reqwest::Response;
use secrecy::zeroize::Zeroizing;

pub(super) const MAX_RESPONSE_BODY_BYTES: usize = 64 * 1024;

pub(super) async fn read_bounded(
    mut response: Response,
) -> Result<Zeroizing<Vec<u8>>, ResponseBodyError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BODY_BYTES as u64)
    {
        return Err(ResponseBodyError);
    }
    let capacity = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or(0);
    let mut body = Zeroizing::new(Vec::with_capacity(capacity));
    while let Some(chunk) = response.chunk().await.map_err(|_| ResponseBodyError)? {
        let length = body
            .len()
            .checked_add(chunk.len())
            .ok_or(ResponseBodyError)?;
        if length > MAX_RESPONSE_BODY_BYTES {
            return Err(ResponseBodyError);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ResponseBodyError;
