//! `Retry-After` parsing (delta-seconds and HTTP-date) and formatting.

use std::time::{Duration, SystemTime};

use axum::http::header::RETRY_AFTER;
use axum::http::{HeaderMap, HeaderValue};

use crate::pool::ceil_secs;

/// Upstream wait hint: `retry-after-ms` (integer milliseconds) when present,
/// else `Retry-After` as delta-seconds or an HTTP date relative to `now`.
pub fn parse(headers: &HeaderMap, now: SystemTime) -> Option<Duration> {
    let millis = headers
        .get("retry-after-ms")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
    if let Some(millis) = millis {
        return Some(Duration::from_millis(millis));
    }
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?;
    parse_value(value, now)
}

/// Parse one `Retry-After` value. Oversized delta-seconds saturate (the pool
/// clamps to `max_cooldown`); dates in the past mean "now".
pub fn parse_value(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Some(
            value
                .parse::<u64>()
                .map_or(Duration::MAX, Duration::from_secs),
        );
    }
    let at = httpdate::parse_http_date(value).ok()?;
    Some(at.duration_since(now).unwrap_or(Duration::ZERO))
}

/// Whole seconds, rounded up, never below one.
pub fn header_value(duration: Duration) -> HeaderValue {
    HeaderValue::from(ceil_secs(duration).max(1))
}

#[cfg(test)]
mod tests {
    use std::time::UNIX_EPOCH;

    use super::*;

    // Sun, 06 Nov 1994 08:49:37 GMT
    fn reference() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(784_111_777)
    }

    #[test]
    fn parses_delta_seconds() {
        assert_eq!(
            parse_value("120", reference()),
            Some(Duration::from_secs(120))
        );
        assert_eq!(parse_value(" 0 ", reference()), Some(Duration::ZERO));
        assert_eq!(
            parse_value("99999999999999999999999", reference()),
            Some(Duration::MAX)
        );
    }

    #[test]
    fn parses_http_dates() {
        assert_eq!(
            parse_value("Sun, 06 Nov 1994 08:50:37 GMT", reference()),
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            parse_value("Sun, 06 Nov 1994 08:48:37 GMT", reference()),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_value("soon", reference()), None);
        assert_eq!(parse_value("-5", reference()), None);
        assert_eq!(parse_value("", reference()), None);
    }

    #[test]
    fn prefers_millisecond_header() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("30"));
        headers.insert("retry-after-ms", HeaderValue::from_static("1500"));
        assert_eq!(
            parse(&headers, reference()),
            Some(Duration::from_millis(1500))
        );
    }

    #[test]
    fn formats_rounded_up_seconds() {
        assert_eq!(header_value(Duration::from_millis(1500)), "2");
        assert_eq!(header_value(Duration::ZERO), "1");
    }
}
