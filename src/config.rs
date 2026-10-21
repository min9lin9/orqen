//! TOML configuration: listen address, upstream, limits, session policy and
//! credential key files.
//!
//! The config file never holds secrets. Each `[[keys]]` entry names a key
//! file; its content is read once at startup into sensitive header values
//! and never logged or echoed in errors.

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::http::HeaderName;
use serde::Deserialize;

use crate::pool::{CooldownPolicy, Credential, KeySpec, RateLimitScope};
use crate::session::{DEFAULT_SESSION_HEADERS, MissingSessionPolicy, SessionConfig};

pub const DEFAULT_UPSTREAM: &str = "https://opencode.ai/inference/go";
pub const DEFAULT_LISTEN: &str = "127.0.0.1:8790";
const MAX_COOLDOWN_CEILING_SECS: u64 = 7 * 24 * 60 * 60;
const MAX_REQUEST_BYTES_CEILING: usize = 256 << 20;
const MAX_ID_LEN: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("read config {}: {source}", .path.display())]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("parse config {}: {message}", .path.display())]
    Parse { path: PathBuf, message: String },
    #[error("invalid config: {0}")]
    Invalid(String),
    #[error("read key file for key {id} ({}): {source}", .path.display())]
    KeyFile {
        id: String,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("key file for key {id} ({}) {problem}", .path.display())]
    KeyContent {
        id: String,
        path: PathBuf,
        problem: &'static str,
    },
}

fn invalid(message: impl Into<String>) -> ConfigError {
    ConfigError::Invalid(message.into())
}

/// Validated configuration.
#[derive(Debug)]
pub struct Config {
    pub listen: SocketAddr,
    pub upstream: UpstreamConfig,
    pub limits: RequestLimits,
    pub cooldown: CooldownPolicy,
    pub session: SessionConfig,
    pub keys: Vec<KeySpec>,
}

#[derive(Debug, Clone)]
pub struct UpstreamConfig {
    /// Base URL without trailing slash; route paths are appended verbatim.
    /// Injectable so loopback tests can point at a local stub.
    pub base_url: String,
    pub connect_timeout: Duration,
    pub response_header_timeout: Duration,
    pub stream_idle_timeout: Duration,
    pub max_attempts: usize,
    /// Loopback upstream (test stub): bypass any environment HTTP proxy.
    pub direct: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct RequestLimits {
    pub max_request_bytes: usize,
    pub request_read_timeout: Duration,
    pub shutdown_drain: Duration,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default = "default_listen")]
    listen: String,
    #[serde(default)]
    upstream: RawUpstream,
    #[serde(default)]
    limits: RawLimits,
    #[serde(default)]
    session: RawSession,
    #[serde(default)]
    keys: Vec<RawKey>,
}

fn default_listen() -> String {
    DEFAULT_LISTEN.to_owned()
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawUpstream {
    base_url: String,
    connect_timeout_secs: u64,
    response_header_timeout_secs: u64,
    stream_idle_timeout_secs: u64,
    max_attempts: usize,
}

impl Default for RawUpstream {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_UPSTREAM.to_owned(),
            connect_timeout_secs: 10,
            response_header_timeout_secs: 600,
            stream_idle_timeout_secs: 300,
            max_attempts: 3,
        }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawLimits {
    max_request_bytes: usize,
    request_read_timeout_secs: u64,
    shutdown_drain_secs: u64,
    default_cooldown_secs: u64,
    auth_failure_cooldown_secs: u64,
    max_cooldown_secs: u64,
    rate_limit_scope: RateLimitScope,
    max_model_cooldowns: usize,
}

impl Default for RawLimits {
    fn default() -> Self {
        Self {
            max_request_bytes: 32 << 20,
            request_read_timeout_secs: 60,
            shutdown_drain_secs: 30,
            default_cooldown_secs: 60,
            auth_failure_cooldown_secs: 600,
            max_cooldown_secs: 86_400,
            rate_limit_scope: RateLimitScope::Account,
            max_model_cooldowns: 1024,
        }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawSession {
    headers: Vec<String>,
    missing: MissingSessionPolicy,
}

impl Default for RawSession {
    fn default() -> Self {
        Self {
            headers: DEFAULT_SESSION_HEADERS.map(str::to_owned).to_vec(),
            missing: MissingSessionPolicy::Ephemeral,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawKey {
    id: String,
    key_file: PathBuf,
    #[serde(default)]
    scope: Option<String>,
}

/// Load and validate `path`. Relative key paths resolve against the config
/// file's directory.
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let raw: RawConfig = toml::from_str(&text).map_err(|error| ConfigError::Parse {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    let base = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    build(raw, &base)
}

fn build(raw: RawConfig, base: &Path) -> Result<Config, ConfigError> {
    let listen: SocketAddr = raw
        .listen
        .parse()
        .map_err(|_| invalid(format!("listen {:?} is not an IP:port address", raw.listen)))?;
    if !listen.ip().is_loopback() {
        return Err(invalid(format!(
            "listen {listen} is not a loopback address; Orqen is a local-only service"
        )));
    }
    let upstream = build_upstream(raw.upstream)?;
    let (limits, cooldown) = build_limits(raw.limits)?;
    let session = build_session(raw.session)?;
    let keys = build_keys(raw.keys, base)?;
    if keys.is_empty() {
        return Err(invalid("configure at least one [[keys]] entry"));
    }
    Ok(Config {
        listen,
        upstream,
        limits,
        cooldown,
        session,
        keys,
    })
}

fn positive_secs(name: &str, secs: u64) -> Result<Duration, ConfigError> {
    if secs == 0 {
        return Err(invalid(format!("{name} must be at least 1")));
    }
    Ok(Duration::from_secs(secs))
}

fn build_upstream(raw: RawUpstream) -> Result<UpstreamConfig, ConfigError> {
    let trimmed = raw.base_url.trim().trim_end_matches('/');
    let url = reqwest::Url::parse(trimmed)
        .map_err(|_| invalid(format!("upstream.base_url {trimmed:?} is not a URL")))?;
    if url.query().is_some() || url.fragment().is_some() {
        return Err(invalid(
            "upstream.base_url must not carry a query or fragment",
        ));
    }
    let host = url.host_str().unwrap_or_default();
    let loopback_host = host == "localhost"
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    match url.scheme() {
        "https" => {}
        // Plain HTTP is accepted only for loopback stubs so keys never
        // cross the network in clear text.
        "http" if loopback_host => {}
        _ => {
            return Err(invalid(
                "upstream.base_url must use https (http is allowed only for loopback hosts)",
            ));
        }
    }
    if !(1..=16).contains(&raw.max_attempts) {
        return Err(invalid("upstream.max_attempts must be between 1 and 16"));
    }
    Ok(UpstreamConfig {
        base_url: trimmed.to_owned(),
        connect_timeout: positive_secs("upstream.connect_timeout_secs", raw.connect_timeout_secs)?,
        response_header_timeout: positive_secs(
            "upstream.response_header_timeout_secs",
            raw.response_header_timeout_secs,
        )?,
        stream_idle_timeout: positive_secs(
            "upstream.stream_idle_timeout_secs",
            raw.stream_idle_timeout_secs,
        )?,
        max_attempts: raw.max_attempts,
        direct: loopback_host,
    })
}

fn build_limits(raw: RawLimits) -> Result<(RequestLimits, CooldownPolicy), ConfigError> {
    if !(1024..=MAX_REQUEST_BYTES_CEILING).contains(&raw.max_request_bytes) {
        return Err(invalid(format!(
            "limits.max_request_bytes must be between 1024 and {MAX_REQUEST_BYTES_CEILING}"
        )));
    }
    if raw.max_cooldown_secs > MAX_COOLDOWN_CEILING_SECS {
        return Err(invalid(format!(
            "limits.max_cooldown_secs must not exceed {MAX_COOLDOWN_CEILING_SECS}"
        )));
    }
    let max_cooldown = positive_secs("limits.max_cooldown_secs", raw.max_cooldown_secs)?;
    let default_cooldown =
        positive_secs("limits.default_cooldown_secs", raw.default_cooldown_secs)?;
    let auth_failure_cooldown = positive_secs(
        "limits.auth_failure_cooldown_secs",
        raw.auth_failure_cooldown_secs,
    )?;
    if default_cooldown > max_cooldown || auth_failure_cooldown > max_cooldown {
        return Err(invalid(
            "limits.default_cooldown_secs and limits.auth_failure_cooldown_secs must not exceed limits.max_cooldown_secs",
        ));
    }
    if !(1..=100_000).contains(&raw.max_model_cooldowns) {
        return Err(invalid(
            "limits.max_model_cooldowns must be between 1 and 100000",
        ));
    }
    let limits = RequestLimits {
        max_request_bytes: raw.max_request_bytes,
        request_read_timeout: positive_secs(
            "limits.request_read_timeout_secs",
            raw.request_read_timeout_secs,
        )?,
        shutdown_drain: positive_secs("limits.shutdown_drain_secs", raw.shutdown_drain_secs)?,
    };
    let cooldown = CooldownPolicy {
        default_cooldown,
        auth_failure_cooldown,
        max_cooldown,
        rate_limit_scope: raw.rate_limit_scope,
        max_model_cooldowns: raw.max_model_cooldowns,
    };
    Ok((limits, cooldown))
}

fn build_session(raw: RawSession) -> Result<SessionConfig, ConfigError> {
    let mut headers: Vec<HeaderName> = Vec::with_capacity(raw.headers.len());
    for name in raw.headers {
        let lowered = name.trim().to_ascii_lowercase();
        let header = HeaderName::from_bytes(lowered.as_bytes()).map_err(|_| {
            invalid(format!(
                "session.headers entry {name:?} is not a valid header name"
            ))
        })?;
        if matches!(
            header.as_str(),
            "authorization" | "x-api-key" | "cookie" | "proxy-authorization"
        ) {
            return Err(invalid(format!(
                "session.headers must not use the credential header {header}"
            )));
        }
        if !headers.contains(&header) {
            headers.push(header);
        }
    }
    if headers.is_empty() && raw.missing == MissingSessionPolicy::Reject {
        return Err(invalid(
            "session.missing = \"reject\" requires at least one session.headers entry",
        ));
    }
    Ok(SessionConfig {
        headers,
        missing: raw.missing,
    })
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ID_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn resolve(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn build_keys(raw: Vec<RawKey>, base: &Path) -> Result<Vec<KeySpec>, ConfigError> {
    let mut ids = HashSet::new();
    let mut specs: Vec<KeySpec> = Vec::with_capacity(raw.len());
    for key in raw {
        if !valid_id(&key.id) {
            return Err(invalid(format!(
                "key id {:?} must be 1-{MAX_ID_LEN} characters of [A-Za-z0-9._-]",
                key.id
            )));
        }
        if !ids.insert(key.id.clone()) {
            return Err(invalid(format!("duplicate key id {}", key.id)));
        }
        // A key without an explicit scope is its own account: it never
        // shares cooldowns with another key.
        let scope = key.scope.unwrap_or_else(|| key.id.clone());
        if !valid_id(&scope) {
            return Err(invalid(format!(
                "scope {scope:?} of key {} must be 1-{MAX_ID_LEN} characters of [A-Za-z0-9._-]",
                key.id
            )));
        }
        let path = resolve(base, &key.key_file);
        let credential = read_credential(&key.id, &path)?;
        if let Some(other) = specs
            .iter()
            .find(|spec| spec.credential.same_secret(&credential))
        {
            return Err(invalid(format!(
                "keys {} and {} hold the same credential; list each credential once",
                other.id, key.id
            )));
        }
        specs.push(KeySpec {
            id: key.id,
            scope,
            credential,
        });
    }
    Ok(specs)
}

fn read_credential(id: &str, path: &Path) -> Result<Credential, ConfigError> {
    let content_error = |problem: &'static str| ConfigError::KeyContent {
        id: id.to_owned(),
        path: path.to_path_buf(),
        problem,
    };
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::KeyFile {
        id: id.to_owned(),
        path: path.to_path_buf(),
        source,
    })?;
    let secret = text.trim();
    if secret.is_empty() {
        return Err(content_error("is empty"));
    }
    let credential = Credential::from_secret(secret)
        .ok_or_else(|| content_error("must hold exactly one printable ASCII token"))?;
    warn_if_shared(id, path);
    Ok(credential)
}

#[cfg(unix)]
fn warn_if_shared(id: &str, path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = std::fs::metadata(path)
        && metadata.permissions().mode() & 0o077 != 0
    {
        tracing::warn!(
            key = id,
            path = %path.display(),
            "key file is accessible to group/others; chmod 600 is recommended"
        );
    }
}

#[cfg(not(unix))]
fn warn_if_shared(_id: &str, _path: &Path) {}
