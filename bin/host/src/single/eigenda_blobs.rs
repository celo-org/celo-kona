//! Eigen DA blobs provider

use alloy_primitives::Bytes;
use reqwest;

/// Fetches blobs from EigenDA via an eigenda-proxy instance.
#[derive(Debug, Clone)]
pub struct OnlineEigenDABlobProvider {
    /// The base url of the Eigen DA Proxy
    base: String,
    /// The inner reqwest client
    client: reqwest::Client,
}

impl OnlineEigenDABlobProvider {
    const GET_METHOD: &'static str = "get";

    /// new creates a new instance of the [OnlineEigenDABlobProvider].
    pub fn new(base: String) -> Self {
        let client = reqwest::Client::new();
        Self { base, client }
    }

    /// fetch_eigenda_blob fetches a blob from EigenDA using the provided certificate.
    pub async fn fetch_eigenda_blob(
        &self,
        cert: &Bytes,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let url = format!("{}/{}/{}", self.base, Self::GET_METHOD, cert);
        self.client.get(url).send().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Bytes;
    use tokio;

    #[tokio::test]
    async fn test_fetch_eigenda_blob_success() {
        let mut server = mockito::Server::new_async().await;
        let base_url = server.url();

        let mock_cert = "mock blob cert";
        let mock_response = "mock blob data";
        let endpoint = format!("/get/0x{}", alloy_primitives::hex::encode(mock_cert));

        let mock = server
            .mock("GET", endpoint.as_str())
            .with_status(200)
            .with_header("content-type", "application/octet-stream")
            .with_body(mock_response)
            .create_async()
            .await;

        let provider = OnlineEigenDABlobProvider::new(base_url);

        let result = provider
            .fetch_eigenda_blob(&Bytes::from_static(mock_cert.as_bytes()))
            .await;

        assert!(result.is_ok());

        let response = result.unwrap();
        assert_eq!(response.status(), 200);

        let body = response.text().await.unwrap();
        assert_eq!(body, mock_response);

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_fetch_eigenda_blob_error() {
        let mut server = mockito::Server::new_async().await;
        let base_url = server.url();

        let mock_cert = "mock blob cert";
        let endpoint = format!("/get/0x{}", alloy_primitives::hex::encode(mock_cert));

        let mock = server
            .mock("GET", endpoint.as_str())
            .with_status(500)
            .with_header("content-type", "application/json")
            .with_body(r#"{"error": "internal server error"}"#)
            .create_async()
            .await;

        let provider = OnlineEigenDABlobProvider::new(base_url);
        let cert_bytes = Bytes::from_static(mock_cert.as_bytes());

        let result = provider.fetch_eigenda_blob(&cert_bytes).await;

        assert!(result.is_ok());
        let response = result.unwrap();
        assert_eq!(response.status(), 500);

        mock.assert_async().await;
    }
}
