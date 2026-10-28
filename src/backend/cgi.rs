//! CGI/1.1 mapping: HTTP request -> FastCGI params, CGI response head -> HTTP.
//!
//! Pure functions, so the security-relevant rules (httpoxy, header name
//! spoofing, response header validation) are unit-tested without PHP.

use std::{net::SocketAddr, os::unix::ffi::OsStrExt, path::PathBuf};

use hyper::{
    HeaderMap, StatusCode, Version,
    header::{self, HeaderName, HeaderValue},
    http::request::Parts,
};

/// The script every PHP request runs (front-controller model).
#[derive(Debug, Clone)]
pub struct Script {
    pub document_root: PathBuf,
    /// Absolute, canonical path of the entry point.
    pub filename: PathBuf,
    /// URL path of the entry point, e.g. `/index.php`.
    pub name: String,
}

impl Script {
    pub fn new(document_root: PathBuf, filename: PathBuf) -> Self {
        let rel = filename
            .strip_prefix(&document_root)
            .unwrap_or(&filename)
            .to_string_lossy()
            .into_owned();
        Self {
            document_root,
            filename,
            name: format!("/{}", rel.trim_start_matches('/')),
        }
    }
}

/// Connection facts that are not part of the HTTP request itself.
#[derive(Debug, Clone, Copy)]
pub struct ConnInfo {
    pub remote: SocketAddr,
    pub local: SocketAddr,
}

/// Build the FastCGI params for one request, already encoded.
pub fn params(script: &Script, req: &Parts, conn: ConnInfo, body_len: usize) -> Vec<u8> {
    let mut p = Params::default();

    p.add("GATEWAY_INTERFACE", "CGI/1.1");
    p.add(
        "SERVER_SOFTWARE",
        concat!("rphp/", env!("CARGO_PKG_VERSION")),
    );
    p.add("SERVER_PROTOCOL", protocol(req.version));
    p.add("REQUEST_SCHEME", "http");
    p.add("REQUEST_METHOD", req.method.as_str());
    p.add(
        "REQUEST_URI",
        req.uri
            .path_and_query()
            .map_or(req.uri.path(), |pq| pq.as_str()),
    );
    p.add("QUERY_STRING", req.uri.query().unwrap_or(""));
    p.add("DOCUMENT_ROOT", script.document_root.as_os_str().as_bytes());
    p.add("SCRIPT_FILENAME", script.filename.as_os_str().as_bytes());
    p.add("SCRIPT_NAME", &script.name);
    p.add("DOCUMENT_URI", &script.name);
    // Required by php-cgi builds with cgi.force_redirect enabled.
    p.add("REDIRECT_STATUS", "200");

    p.add("SERVER_NAME", server_name(&req.headers, conn.local));
    p.add("SERVER_ADDR", conn.local.ip().to_string());
    p.add("SERVER_PORT", conn.local.port().to_string());
    p.add("REMOTE_ADDR", conn.remote.ip().to_string());
    p.add("REMOTE_PORT", conn.remote.port().to_string());

    if let Some(ct) = req.headers.get(header::CONTENT_TYPE) {
        p.add("CONTENT_TYPE", ct.as_bytes());
    }
    // The body is fully read before PHP runs, so this is the real length,
    // not whatever the client claimed.
    if body_len > 0 || req.headers.contains_key(header::CONTENT_LENGTH) {
        p.add("CONTENT_LENGTH", body_len.to_string());
    }

    for name in req.headers.keys() {
        let Some(var) = http_var(name) else { continue };
        let separator: &[u8] = if name == header::COOKIE { b"; " } else { b", " };
        let mut value = Vec::new();
        for (i, v) in req.headers.get_all(name).iter().enumerate() {
            if i > 0 {
                value.extend_from_slice(separator);
            }
            value.extend_from_slice(v.as_bytes());
        }
        p.add(var, value);
    }

    p.0
}

#[derive(Default)]
struct Params(Vec<u8>);

impl Params {
    fn add(&mut self, name: impl AsRef<[u8]>, value: impl AsRef<[u8]>) {
        let (name, value) = (name.as_ref(), value.as_ref());
        self.0
            .extend(crate::fastcgi::encode_params([(name, value)]));
    }
}

pub(crate) fn protocol(version: Version) -> &'static str {
    match version {
        Version::HTTP_10 => "HTTP/1.0",
        Version::HTTP_2 => "HTTP/2.0",
        _ => "HTTP/1.1",
    }
}

/// `HTTP_*` variable for a request header, or `None` if it must not be passed.
///
/// * `Proxy` is dropped (httpoxy: it would become `HTTP_PROXY`).
/// * Names containing `_` are dropped, so `X-Foo_Bar` cannot spoof
///   `X-Foo-Bar` once both map to `HTTP_X_FOO_BAR`.
/// * `Content-Type` and `Content-Length` are passed as `CONTENT_*` instead.
fn http_var(name: &HeaderName) -> Option<String> {
    let name = name.as_str();
    if name.contains('_') || name == "proxy" || name == "content-type" || name == "content-length" {
        return None;
    }
    let mut var = String::with_capacity(5 + name.len());
    var.push_str("HTTP_");
    var.extend(name.chars().map(|c| match c {
        '-' => '_',
        c => c.to_ascii_uppercase(),
    }));
    Some(var)
}

/// Host header without its port, falling back to the local address.
pub(crate) fn server_name(headers: &HeaderMap, local: SocketAddr) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(str::trim)
        .filter(|h| !h.is_empty());
    let Some(host) = host else {
        return local.ip().to_string();
    };
    if let Some(rest) = host.strip_prefix('[') {
        // IPv6 literal: "[::1]:8080" -> "[::1]"
        return match rest.find(']') {
            Some(end) => host[..end + 2].to_string(),
            None => host.to_string(),
        };
    }
    host.split(':').next().unwrap_or(host).to_string()
}

/// Status and headers of a CGI response.
#[derive(Debug)]
pub struct Head {
    pub status: StatusCode,
    pub headers: HeaderMap,
}

/// Find the blank line ending the CGI header block. Returns the length of the
/// header block and the offset at which the body starts.
pub fn find_head_end(buf: &[u8]) -> Option<(usize, usize)> {
    if buf.starts_with(b"\r\n") {
        return Some((0, 2));
    }
    if buf.starts_with(b"\n") {
        return Some((0, 1));
    }
    let mut i = 0;
    while let Some(pos) = buf[i..].iter().position(|&b| b == b'\n') {
        let nl = i + pos;
        match &buf[nl + 1..] {
            [b'\n', ..] => return Some((nl, nl + 2)),
            [b'\r', b'\n', ..] => return Some((nl, nl + 3)),
            _ => i = nl + 1,
        }
    }
    None
}

/// Headers that describe the PHP <-> RPHP hop, not the response.
const HOP_BY_HOP: [HeaderName; 5] = [
    header::CONNECTION,
    header::TRANSFER_ENCODING,
    header::TE,
    header::TRAILER,
    header::UPGRADE,
];

/// True for headers that describe the PHP <-> RPHP hop, not the response.
pub(crate) fn is_hop_by_hop(name: &HeaderName) -> bool {
    HOP_BY_HOP.contains(name) || name.as_str() == "keep-alive"
}

/// Parse a CGI header block (without the terminating blank line).
pub fn parse_head(block: &[u8]) -> Result<Head, String> {
    let mut status = None;
    let mut headers = HeaderMap::new();

    for line in block.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let colon = line
            .iter()
            .position(|&b| b == b':')
            .ok_or_else(|| format!("header line without ':': {:?}", lossy(line)))?;
        let name = HeaderName::from_bytes(&line[..colon])
            .map_err(|_| format!("invalid header name {:?}", lossy(&line[..colon])))?;
        let value = line[colon + 1..].trim_ascii();

        if name.as_str() == "status" {
            status = Some(parse_status(value)?);
            continue;
        }
        if is_hop_by_hop(&name) {
            continue;
        }
        let value = HeaderValue::from_bytes(value)
            .map_err(|_| format!("invalid value for header {name}"))?;
        headers.append(name, value);
    }

    let status = match status {
        Some(status) => status,
        None if headers.contains_key(header::LOCATION) => StatusCode::FOUND,
        None => StatusCode::OK,
    };
    Ok(Head { status, headers })
}

fn parse_status(value: &[u8]) -> Result<StatusCode, String> {
    let code = value.split(|&b| b == b' ').next().unwrap_or_default();
    std::str::from_utf8(code)
        .ok()
        .and_then(|c| c.parse::<u16>().ok())
        .and_then(|c| StatusCode::from_u16(c).ok())
        .filter(|s| !s.is_informational())
        .ok_or_else(|| format!("invalid Status header {:?}", lossy(value)))
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fastcgi::decode_params;
    use hyper::Request;
    use std::collections::HashMap;

    fn script() -> Script {
        Script::new("/srv/app/public".into(), "/srv/app/public/index.php".into())
    }

    fn conn() -> ConnInfo {
        ConnInfo {
            remote: "10.0.0.7:51000".parse().unwrap(),
            local: "127.0.0.1:8080".parse().unwrap(),
        }
    }

    fn env(req: Request<()>, body_len: usize) -> HashMap<String, String> {
        let (parts, ()) = req.into_parts();
        decode_params(&params(&script(), &parts, conn(), body_len))
            .unwrap()
            .into_iter()
            .map(|(k, v)| (String::from_utf8(k).unwrap(), String::from_utf8(v).unwrap()))
            .collect()
    }

    #[test]
    fn maps_request_line_and_script() {
        let e = env(
            Request::get("/users/7?sort=name&x=%20")
                .header("host", "example.test:8080")
                .body(())
                .unwrap(),
            0,
        );
        assert_eq!(e["REQUEST_METHOD"], "GET");
        assert_eq!(e["REQUEST_URI"], "/users/7?sort=name&x=%20");
        assert_eq!(e["QUERY_STRING"], "sort=name&x=%20");
        assert_eq!(e["SCRIPT_FILENAME"], "/srv/app/public/index.php");
        assert_eq!(e["SCRIPT_NAME"], "/index.php");
        assert_eq!(e["DOCUMENT_ROOT"], "/srv/app/public");
        assert_eq!(e["SERVER_NAME"], "example.test");
        assert_eq!(e["SERVER_PORT"], "8080");
        assert_eq!(e["REMOTE_ADDR"], "10.0.0.7");
        assert_eq!(e["HTTP_HOST"], "example.test:8080");
        assert!(!e.contains_key("CONTENT_LENGTH"));
    }

    #[test]
    fn content_headers_use_cgi_names_and_real_length() {
        let e = env(
            Request::post("/")
                .header("content-type", "application/json")
                .header("content-length", "999")
                .body(())
                .unwrap(),
            12,
        );
        assert_eq!(e["CONTENT_TYPE"], "application/json");
        assert_eq!(e["CONTENT_LENGTH"], "12");
        assert!(!e.contains_key("HTTP_CONTENT_TYPE"));
        assert!(!e.contains_key("HTTP_CONTENT_LENGTH"));
    }

    #[test]
    fn drops_proxy_and_underscore_headers() {
        let e = env(
            Request::get("/")
                .header("proxy", "http://evil:3128")
                .header("x-forwarded-for", "1.2.3.4")
                .header("x-forwarded_for", "6.6.6.6")
                .body(())
                .unwrap(),
            0,
        );
        assert!(!e.contains_key("HTTP_PROXY"));
        assert_eq!(e["HTTP_X_FORWARDED_FOR"], "1.2.3.4");
    }

    #[test]
    fn joins_repeated_headers() {
        let e = env(
            Request::get("/")
                .header("cookie", "a=1")
                .header("cookie", "b=2")
                .header("accept", "text/html")
                .header("accept", "*/*")
                .body(())
                .unwrap(),
            0,
        );
        assert_eq!(e["HTTP_COOKIE"], "a=1; b=2");
        assert_eq!(e["HTTP_ACCEPT"], "text/html, */*");
    }

    #[test]
    fn server_name_handles_ipv6_and_missing_host() {
        let local = "127.0.0.1:8080".parse().unwrap();
        let mut h = HeaderMap::new();
        assert_eq!(server_name(&h, local), "127.0.0.1");
        h.insert(header::HOST, HeaderValue::from_static("[::1]:9000"));
        assert_eq!(server_name(&h, local), "[::1]");
    }

    #[test]
    fn finds_head_end_with_crlf_or_lf() {
        assert_eq!(find_head_end(b"A: 1\r\n\r\nbody"), Some((5, 8)));
        assert_eq!(find_head_end(b"A: 1\n\nbody"), Some((4, 6)));
        assert_eq!(find_head_end(b"\r\nbody"), Some((0, 2)));
        assert_eq!(find_head_end(b"A: 1\r\nB: 2\r\n"), None);
    }

    #[test]
    fn parses_status_cookies_and_drops_hop_by_hop() {
        let head = parse_head(
            b"Status: 404 Not Found\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\n\
              Content-type: text/html\r\nTransfer-Encoding: chunked\r\nConnection: close",
        )
        .unwrap();
        assert_eq!(head.status, StatusCode::NOT_FOUND);
        assert_eq!(head.headers.get_all("set-cookie").iter().count(), 2);
        assert_eq!(head.headers["content-type"], "text/html");
        assert!(!head.headers.contains_key("transfer-encoding"));
        assert!(!head.headers.contains_key("connection"));
    }

    #[test]
    fn location_without_status_is_a_redirect() {
        let head = parse_head(b"Location: /login").unwrap();
        assert_eq!(head.status, StatusCode::FOUND);
        assert_eq!(parse_head(b"X-A: 1").unwrap().status, StatusCode::OK);
    }

    #[test]
    fn rejects_malformed_heads() {
        assert!(parse_head(b"no colon here").is_err());
        assert!(parse_head(b"Bad Name: x").is_err());
        assert!(parse_head(b"Status: abc").is_err());
        assert!(parse_head(b"Status: 101 Switching Protocols").is_err());
        assert!(parse_head(b"X-Injected: a\x00b").is_err());
    }
}
