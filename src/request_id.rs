//! Request IDs: one per request, logged with every event of that request,
//! returned in the `X-Request-Id` response header and passed to PHP as
//! `$_SERVER['HTTP_X_REQUEST_ID']`.

use std::{
    hash::{BuildHasher, RandomState},
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use hyper::header::{HeaderMap, HeaderName, HeaderValue};

pub const HEADER: HeaderName = HeaderName::from_static("x-request-id");

/// Longest client-supplied ID that is kept.
const MAX_LEN: usize = 128;

/// The ID for a request: the client's (or upstream proxy's) `X-Request-Id`
/// if it is a single, short, plain token, otherwise a fresh one. Anything
/// else (control characters, spaces, quotes, very long values) is replaced so
/// a client cannot inject text into logs.
pub fn for_request(headers: &HeaderMap) -> HeaderValue {
    let mut supplied = headers.get_all(&HEADER).iter();
    match (supplied.next(), supplied.next()) {
        (Some(value), None) if is_valid(value.as_bytes()) => value.clone(),
        _ => generate(),
    }
}

/// A new ID, unique within this process and unlikely to collide across
/// processes: a random per-process prefix and a counter, e.g.
/// `3f9a0c1d2b4e5f60-000000000000002a`.
pub fn generate() -> HeaderValue {
    static PREFIX: OnceLock<u64> = OnceLock::new();
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    // `RandomState` is seeded from the OS RNG; no extra dependency needed.
    let prefix = *PREFIX.get_or_init(|| RandomState::new().hash_one(std::process::id()));
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    HeaderValue::try_from(format!("{prefix:016x}-{n:016x}"))
        .expect("hex digits are a valid header value")
}

fn is_valid(id: &[u8]) -> bool {
    !id.is_empty()
        && id.len() <= MAX_LEN
        && id.iter().all(|&b| {
            b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':' | b'/' | b'+' | b'=')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(values: &[&str]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for v in values {
            map.append(&HEADER, HeaderValue::from_str(v).unwrap());
        }
        map
    }

    #[test]
    fn generated_ids_are_unique_and_well_formed() {
        let a = generate();
        let b = generate();
        assert_ne!(a, b);
        assert_eq!(a.len(), 33);
        assert!(is_valid(a.as_bytes()));
    }

    #[test]
    fn keeps_a_plain_client_id() {
        let id = "a1b2c3d4-e5f6-4711-8000-123456789abc";
        assert_eq!(for_request(&headers(&[id])), id);
        // Common formats: base64 trace IDs, "Root=..." style.
        assert_eq!(for_request(&headers(&["abc+/=="])), "abc+/==");
    }

    #[test]
    fn replaces_missing_unsafe_long_or_repeated_ids() {
        for bad in [
            headers(&[]),
            headers(&[""]),
            headers(&["two words"]),
            headers(&["quote\"d"]),
            headers(&["tab\tid"]),
            headers(&[&"x".repeat(MAX_LEN + 1)]),
            headers(&["one", "two"]),
        ] {
            let id = for_request(&bad);
            assert_eq!(id.len(), 33, "{bad:?} -> {id:?}");
        }
        assert_eq!(
            for_request(&headers(&[&"x".repeat(MAX_LEN)])).len(),
            MAX_LEN
        );
    }
}
