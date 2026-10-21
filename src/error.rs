//! Gateway-generated errors, rendered in the caller's protocol shape.
//!
//! Upstream error responses are relayed verbatim and never pass through
//! here; responses built here carry `x-orqen-error: 1`.

use std::time::Duration;

use axum::http::header::RETRY_AFTER;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::retry_after;
use crate::session::SessionError;

#[derive(Debug, Clone, Copy)]
pub enum ErrorShape {
    OpenAi,
    Anthropic,
}

#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    #[error("request body exceeds {limit} bytes")]
    BodyTooLarge { limit: usize },
    #[error("request body could not be read")]
    BodyRead,
    #[error("request body was not received within {0:?}")]
    BodyTimeout(Duration),
    #[error("request body must be a JSON object whose \"model\" is a string")]
    InvalidJson,
    #[error("request body needs a non-empty \"model\" string of at most 256 bytes")]
    MissingModel,
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error("no OpenCode Go credential is configured")]
    NoCredentials,
    #[error("every credential eligible for model {model} is cooling down")]
    Exhausted {
        model: String,
        retry_after: Duration,
    },
    #[error("could not connect to the upstream")]
    UpstreamConnect,
    #[error("upstream did not return response headers within {0:?}")]
    UpstreamTimeout(Duration),
    #[error("upstream request failed before a response head")]
    Upstream,
    #[error("service is shutting down")]
    ShuttingDown,
}

impl ProxyError {
    pub fn status(&self) -> StatusCode {
        match self {
            Self::BodyTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            Self::BodyRead | Self::InvalidJson | Self::MissingModel | Self::Session(_) => {
                StatusCode::BAD_REQUEST
            }
            Self::BodyTimeout(_) => StatusCode::REQUEST_TIMEOUT,
            Self::NoCredentials | Self::ShuttingDown => StatusCode::SERVICE_UNAVAILABLE,
            Self::Exhausted { .. } => StatusCode::TOO_MANY_REQUESTS,
            Self::UpstreamConnect | Self::Upstream => StatusCode::BAD_GATEWAY,
            Self::UpstreamTimeout(_) => StatusCode::GATEWAY_TIMEOUT,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::BodyTooLarge { .. }
            | Self::BodyRead
            | Self::BodyTimeout(_)
            | Self::InvalidJson
            | Self::MissingModel
            | Self::Session(_) => "invalid_request_error",
            Self::Exhausted { .. } => "rate_limit_error",
            Self::NoCredentials | Self::ShuttingDown => "overloaded_error",
            Self::UpstreamConnect | Self::UpstreamTimeout(_) | Self::Upstream => "api_error",
        }
    }

    pub fn into_response(self, shape: ErrorShape) -> Response {
        let status = self.status();
        let message = self.to_string();
        let kind = self.kind();
        let body = match shape {
            ErrorShape::OpenAi => json!({
                "error": {"message": message, "type": kind, "code": null, "param": null}
            }),
            ErrorShape::Anthropic => json!({
                "type": "error",
                "error": {"type": kind, "message": message}
            }),
        };
        let mut response = (status, axum::Json(body)).into_response();
        if let Self::Exhausted { retry_after, .. } = &self {
            response
                .headers_mut()
                .insert(RETRY_AFTER, retry_after::header_value(*retry_after));
        }
        response
            .headers_mut()
            .insert("x-orqen-error", HeaderValue::from_static("1"));
        response
    }
}
