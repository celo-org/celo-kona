//! RPC timeout layer for alloy transports.
//!
//! Provides a custom Tower layer that adds timeout functionality to RPC requests
//! while properly mapping errors to alloy's TransportError type.

use alloy_json_rpc::{RequestPacket, ResponsePacket};
use alloy_transport::{TransportError, TransportErrorKind};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::time::Duration;
use tower::{Layer, Service};

/// Custom timeout layer that properly maps errors for alloy transports
#[derive(Debug, Clone)]
pub(crate) struct RpcTimeoutLayer {
    timeout: Duration,
}

impl RpcTimeoutLayer {
    /// Create a new RpcTimeoutLayer with the specified timeout duration
    pub(crate) fn new(timeout: Duration) -> Self {
        Self { timeout }
    }
}

impl<S> Layer<S> for RpcTimeoutLayer {
    type Service = RpcTimeoutService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RpcTimeoutService { inner, timeout: self.timeout }
    }
}

/// Service that wraps an inner service with timeout functionality
#[derive(Debug, Clone)]
pub(crate) struct RpcTimeoutService<S> {
    inner: S,
    timeout: Duration,
}

impl<S> Service<RequestPacket> for RpcTimeoutService<S>
where
    S: Service<RequestPacket, Response = ResponsePacket, Error = TransportError> + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: RequestPacket) -> Self::Future {
        let timeout = self.timeout;
        let fut = self.inner.call(request);

        // heap allocation and pin memory
        Box::pin(async move {
            match tokio::time::timeout(timeout, fut).await {
                Ok(result) => result,
                Err(_) => Err(TransportErrorKind::custom_str(&format!(
                    "RPC request timed out after {:?}",
                    timeout
                ))),
            }
        })
    }
}
