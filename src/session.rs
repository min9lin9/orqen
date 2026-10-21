//! Conversation identity.
//!
//! The identity is read from the first configured header that is present
//! (default order: `x-opencode-session`, `x-omo-session`, `x-session-id`,
//! `session_id`). It drives key affinity and is forwarded upstream as the
//! real per-conversation `x-opencode-session`; there is no global session.
//!
//! Missing identity policy (`session.missing`):
//! - `ephemeral` (default): a fresh unique ID is minted for that single
//!   request. It is never reused, so unrelated requests are never spliced
//!   into one upstream conversation; such requests have no key affinity.
//! - `reject`: the request fails with 400 before any upstream call.
//!
//! A present header must occur once with 1-256 visible ASCII characters;
//! anything else is rejected rather than guessed. A whitespace-only value
//! counts as absent.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;

pub const DEFAULT_SESSION_HEADERS: [&str; 4] = [
    "x-opencode-session",
    "x-omo-session",
    "x-session-id",
    "session_id",
];
const MAX_SESSION_LEN: usize = 256;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissingSessionPolicy {
    Reject,
    #[default]
    Ephemeral,
}

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub headers: Vec<HeaderName>,
    pub missing: MissingSessionPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Header(HeaderName),
    Ephemeral,
}

#[derive(Debug, Clone)]
pub struct Session {
    id: String,
    source: Source,
    header: HeaderValue,
}

impl Session {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn header_value(&self) -> &HeaderValue {
        &self.header
    }

    pub fn is_ephemeral(&self) -> bool {
        self.source == Source::Ephemeral
    }

    pub fn source_label(&self) -> &str {
        match &self.source {
            Source::Header(name) => name.as_str(),
            Source::Ephemeral => "ephemeral",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("missing conversation identity; send one of these headers: {0}")]
    Missing(String),
    #[error(
        "conversation identity header {0} must appear once with 1-256 visible ASCII characters"
    )]
    Invalid(HeaderName),
}

/// Header carrying the identity upstream.
pub fn forward_header() -> HeaderName {
    HeaderName::from_static("x-opencode-session")
}

/// Unique without randomness: process start time and process id separate
/// processes, the sequence number separates requests within one process.
fn ephemeral_id() -> String {
    static STARTED: OnceLock<u128> = OnceLock::new();
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let started = *STARTED.get_or_init(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos())
    });
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
    format!(
        "orqen-ephemeral-{started:x}-{:x}-{sequence:x}",
        std::process::id()
    )
}

pub fn resolve(headers: &HeaderMap, config: &SessionConfig) -> Result<Session, SessionError> {
    for name in &config.headers {
        let mut values = headers.get_all(name).iter();
        let Some(value) = values.next() else {
            continue;
        };
        if values.next().is_some() {
            return Err(SessionError::Invalid(name.clone()));
        }
        let id = value
            .to_str()
            .map_err(|_| SessionError::Invalid(name.clone()))?
            .trim();
        if id.is_empty() {
            continue;
        }
        if id.len() > MAX_SESSION_LEN || !id.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(SessionError::Invalid(name.clone()));
        }
        let header = HeaderValue::from_str(id).map_err(|_| SessionError::Invalid(name.clone()))?;
        return Ok(Session {
            id: id.to_owned(),
            source: Source::Header(name.clone()),
            header,
        });
    }
    match config.missing {
        MissingSessionPolicy::Reject => Err(SessionError::Missing(
            config
                .headers
                .iter()
                .map(HeaderName::as_str)
                .collect::<Vec<_>>()
                .join(", "),
        )),
        MissingSessionPolicy::Ephemeral => {
            let id = ephemeral_id();
            let header =
                HeaderValue::from_str(&id).map_err(|_| SessionError::Invalid(forward_header()))?;
            Ok(Session {
                id,
                source: Source::Ephemeral,
                header,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(missing: MissingSessionPolicy) -> SessionConfig {
        SessionConfig {
            headers: DEFAULT_SESSION_HEADERS
                .iter()
                .map(|name| HeaderName::from_static(name))
                .collect(),
            missing,
        }
    }

    #[test]
    fn first_configured_header_wins() {
        let mut headers = HeaderMap::new();
        headers.insert("session_id", HeaderValue::from_static("other"));
        headers.insert("x-opencode-session", HeaderValue::from_static("ses_abc"));
        let session = resolve(&headers, &config(MissingSessionPolicy::Reject)).expect("session");
        assert_eq!(session.id(), "ses_abc");
        assert_eq!(session.source_label(), "x-opencode-session");
        assert_eq!(session.header_value(), "ses_abc");
    }

    #[test]
    fn falls_back_to_later_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("x-opencode-session", HeaderValue::from_static("  "));
        headers.insert("session_id", HeaderValue::from_static("codex-1"));
        let session = resolve(&headers, &config(MissingSessionPolicy::Reject)).expect("session");
        assert_eq!(session.id(), "codex-1");
    }

    #[test]
    fn missing_identity_follows_policy() {
        let headers = HeaderMap::new();
        assert!(matches!(
            resolve(&headers, &config(MissingSessionPolicy::Reject)),
            Err(SessionError::Missing(_))
        ));
        let first = resolve(&headers, &config(MissingSessionPolicy::Ephemeral)).expect("first");
        let second = resolve(&headers, &config(MissingSessionPolicy::Ephemeral)).expect("second");
        assert!(first.is_ephemeral());
        assert_ne!(first.id(), second.id());
    }

    #[test]
    fn ambiguous_or_malformed_identity_is_rejected() {
        let mut duplicated = HeaderMap::new();
        duplicated.append("x-opencode-session", HeaderValue::from_static("a"));
        duplicated.append("x-opencode-session", HeaderValue::from_static("b"));
        assert!(matches!(
            resolve(&duplicated, &config(MissingSessionPolicy::Ephemeral)),
            Err(SessionError::Invalid(_))
        ));
        let mut spaced = HeaderMap::new();
        spaced.insert("x-opencode-session", HeaderValue::from_static("a b"));
        assert!(matches!(
            resolve(&spaced, &config(MissingSessionPolicy::Ephemeral)),
            Err(SessionError::Invalid(_))
        ));
    }
}
