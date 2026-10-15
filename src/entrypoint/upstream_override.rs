//! `X-Consolette-Upstream`: a per-request header that forces one configured upstream.
//! The proxy consumes it; it is removed from the headers before dispatch so no provider
//! can ever forward it.

use axum::http::HeaderMap;

use crate::providers::ProviderError;

pub const UPSTREAM_OVERRIDE_HEADER: &str = "x-consolette-upstream";

/// Removes the override header from `headers` and returns the requested upstream name.
///
/// # Errors
///
/// A 400 [`ProviderError::Validation`] when the header is repeated, empty, or not UTF-8.
pub fn take_upstream_override(headers: &mut HeaderMap) -> Result<Option<String>, ProviderError> {
    let values: Vec<_> = headers.get_all(UPSTREAM_OVERRIDE_HEADER).iter().collect();
    let parsed = match values.as_slice() {
        [] => return Ok(None),
        [one] => one
            .to_str()
            .ok()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned),
        _ => None,
    };
    headers.remove(UPSTREAM_OVERRIDE_HEADER);
    parsed.map(Some).ok_or_else(|| {
        ProviderError::Validation(
            "X-Consolette-Upstream must be one non-empty UTF-8 upstream name".to_string(),
            400,
        )
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn with(values: &[&[u8]]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for v in values {
            headers.append(
                UPSTREAM_OVERRIDE_HEADER,
                HeaderValue::from_bytes(v).unwrap(),
            );
        }
        headers.insert("x-other", HeaderValue::from_static("kept"));
        headers
    }

    #[test]
    fn absent_header_should_mean_no_override() {
        let mut headers = with(&[]);
        assert_eq!(take_upstream_override(&mut headers).unwrap(), None);
        assert!(headers.contains_key("x-other"));
    }

    #[test]
    fn named_header_should_be_returned_trimmed_and_removed() {
        let mut headers = with(&[b"  anthropic "]);
        assert_eq!(
            take_upstream_override(&mut headers).unwrap().as_deref(),
            Some("anthropic")
        );
        assert!(!headers.contains_key(UPSTREAM_OVERRIDE_HEADER));
        assert!(headers.contains_key("x-other"));
    }

    #[test]
    fn empty_repeated_or_non_utf8_header_should_be_a_400_and_still_be_removed() {
        for values in [
            &[b"" as &[u8]][..],
            &[b"  "],
            &[b"a", b"b"],
            &[&[0xff, 0xfe]],
        ] {
            let mut headers = with(values);
            let err = take_upstream_override(&mut headers).unwrap_err();
            assert!(matches!(err, ProviderError::Validation(_, 400)), "{err:?}");
            assert!(!headers.contains_key(UPSTREAM_OVERRIDE_HEADER));
        }
    }
}
