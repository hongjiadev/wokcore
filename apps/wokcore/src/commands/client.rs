use std::time::Duration;

use reqwest::{Response, header::HOST};
use secrecy::{SecretString, zeroize::Zeroizing};
use serde::Serialize;
use wokcore_platform::{DiscoveryRecord, DiscoveryStore, PlatformError};
use wokcore_storage::{ReadOnlyStateStore, StorageError};

use crate::RunDependencies;

use super::{
    response::read_bounded,
    status::{validated_authority, verify_identity},
};

const MANAGEMENT_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const MANAGEMENT_REQUEST_TIMEOUT: Duration = Duration::from_secs(40);
const MANAGEMENT_READ_TIMEOUT: Duration = Duration::from_secs(40);

pub(super) struct ControlClient {
    record: DiscoveryRecord,
    authority: String,
    client: reqwest::Client,
}

impl ControlClient {
    pub(super) async fn connect(
        dependencies: &RunDependencies,
    ) -> Result<Self, ControlClientError> {
        let store = DiscoveryStore::new(&dependencies.paths).map_err(map_platform)?;
        let record = store.read().map_err(map_platform)?;
        if !dependencies.process.is_running(record.pid) {
            return Err(ControlClientError::NotRunning);
        }
        verify_identity(&record)
            .await
            .map_err(|_| ControlClientError::IdentityMismatch)?;
        let authority = validated_authority(&record.base_url)
            .map_err(|_| ControlClientError::InvalidRuntime)?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(MANAGEMENT_CONNECT_TIMEOUT)
            .timeout(MANAGEMENT_REQUEST_TIMEOUT)
            .read_timeout(MANAGEMENT_READ_TIMEOUT)
            .build()
            .map_err(|_| ControlClientError::Internal)?;
        Ok(Self {
            record,
            authority,
            client,
        })
    }

    pub(super) async fn management_secret(
        &self,
        dependencies: &RunDependencies,
    ) -> Result<SecretString, ControlClientError> {
        let state =
            ReadOnlyStateStore::open_live(&dependencies.paths.state_db).map_err(map_storage)?;
        let binding = state
            .runtime_secret_binding("management")
            .map_err(map_storage)?
            .ok_or(ControlClientError::Authentication)?;
        dependencies
            .secrets
            .get(&binding.secret_ref)
            .await
            .map_err(|_| ControlClientError::Authentication)
    }

    pub(super) async fn post_json<T: Serialize + ?Sized>(
        &self,
        path: &str,
        management: &SecretString,
        body: Option<&T>,
    ) -> Result<Response, ControlClientError> {
        use secrecy::ExposeSecret;

        let mut request = self
            .client
            .post(format!("{}{path}", self.record.base_url))
            .header(HOST, &self.authority)
            .bearer_auth(management.expose_secret());
        if let Some(body) = body {
            request = request.json(body);
        }
        request
            .send()
            .await
            .map_err(|_| ControlClientError::NotRunning)
    }

    pub(super) async fn get(
        &self,
        path: &str,
        management: &SecretString,
    ) -> Result<Response, ControlClientError> {
        use secrecy::ExposeSecret;

        self.client
            .get(format!("{}{path}", self.record.base_url))
            .header(HOST, &self.authority)
            .bearer_auth(management.expose_secret())
            .send()
            .await
            .map_err(|_| ControlClientError::NotRunning)
    }

    pub(super) async fn put_json<T: Serialize + ?Sized>(
        &self,
        path: &str,
        management: &SecretString,
        body: &T,
    ) -> Result<Response, ControlClientError> {
        use secrecy::ExposeSecret;

        self.client
            .put(format!("{}{path}", self.record.base_url))
            .header(HOST, &self.authority)
            .bearer_auth(management.expose_secret())
            .json(body)
            .send()
            .await
            .map_err(|_| ControlClientError::NotRunning)
    }

    pub(super) async fn delete(
        &self,
        path: &str,
        management: &SecretString,
    ) -> Result<Response, ControlClientError> {
        use secrecy::ExposeSecret;

        self.client
            .delete(format!("{}{path}", self.record.base_url))
            .header(HOST, &self.authority)
            .bearer_auth(management.expose_secret())
            .send()
            .await
            .map_err(|_| ControlClientError::NotRunning)
    }
}

pub(super) async fn response_body(
    response: Response,
) -> Result<Zeroizing<Vec<u8>>, ControlClientError> {
    read_bounded(response)
        .await
        .map_err(|_| ControlClientError::Internal)
}

fn map_platform(error: PlatformError) -> ControlClientError {
    match error {
        PlatformError::Io { source } if source.kind() == std::io::ErrorKind::NotFound => {
            ControlClientError::NotRunning
        }
        PlatformError::UnsafeRuntimePath
        | PlatformError::InvalidDiscovery
        | PlatformError::DiscoveryTooLarge => ControlClientError::InvalidRuntime,
        _ => ControlClientError::Internal,
    }
}

fn map_storage(error: StorageError) -> ControlClientError {
    match error {
        StorageError::StateDatabaseCorrupt { .. } => ControlClientError::StorageCorruption,
        StorageError::Io { source } if source.kind() == std::io::ErrorKind::NotFound => {
            ControlClientError::StorageCorruption
        }
        _ => ControlClientError::Internal,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ControlClientError {
    NotRunning,
    InvalidRuntime,
    IdentityMismatch,
    Authentication,
    StorageCorruption,
    InvalidInput,
    Internal,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        time::timeout,
    };

    use super::{ControlClientError, response_body};
    use crate::commands::response::MAX_RESPONSE_BODY_BYTES;

    #[tokio::test]
    async fn management_body_rejects_overflow_before_stream_completion() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n10001\r\n",
                )
                .await
                .unwrap();
            stream
                .write_all(&vec![b'x'; MAX_RESPONSE_BODY_BYTES + 1])
                .await
                .unwrap();
            stream.write_all(b"\r\n").await.unwrap();
            std::future::pending::<()>().await;
        });
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap();

        let result = timeout(Duration::from_secs(1), response_body(response))
            .await
            .expect("bounded reader must reject before the stream terminates");

        assert_eq!(result.unwrap_err(), ControlClientError::Internal);
        server.abort();
    }

    #[tokio::test]
    async fn management_body_accepts_exactly_the_limit() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n10000\r\n",
                )
                .await
                .unwrap();
            stream
                .write_all(&vec![b'x'; MAX_RESPONSE_BODY_BYTES])
                .await
                .unwrap();
            stream.write_all(b"\r\n0\r\n\r\n").await.unwrap();
        });
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap();

        let body = response_body(response).await.unwrap();

        assert_eq!(body.len(), MAX_RESPONSE_BODY_BYTES);
        server.await.unwrap();
    }
}
