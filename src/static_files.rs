//! Serving existing files under the document root without PHP.
//!
//! Only regular files that stay inside the document root after symlink
//! resolution are served. PHP sources and hidden files never are; such
//! requests fall through to the front controller, which decides what to do.

use std::{
    fs::Metadata,
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use bytes::BytesMut;
use hyper::{
    HeaderMap, Method, Response, StatusCode,
    header::{self, HeaderValue},
};
use tokio::io::AsyncReadExt;
use tracing::debug;

use crate::body::{self, ResponseBody};

/// Extensions that may contain PHP source; serving them would leak code.
const NEVER_SERVED: &[&str] = &[
    "php", "phtml", "phar", "phps", "php3", "php4", "php5", "php7", "php8", "inc",
];

const CHUNK_SIZE: usize = 64 * 1024;

/// A file that may be served for a request.
#[derive(Debug)]
pub struct StaticFile {
    pub path: PathBuf,
    pub metadata: Metadata,
}

/// Map a request path to a servable file, or `None` to let PHP handle it.
pub async fn lookup(root: &Path, request_path: &str) -> Option<StaticFile> {
    let rel = sanitize(request_path)?;
    let path = tokio::fs::canonicalize(root.join(rel)).await.ok()?;
    if !path.starts_with(root) {
        return None;
    }
    let metadata = tokio::fs::metadata(&path).await.ok()?;
    if !metadata.is_file() {
        return None;
    }
    Some(StaticFile { path, metadata })
}

/// Decode and check a request path. Rejects anything that is not a plain
/// relative path of normal components, hidden components (except
/// `.well-known`) and PHP source extensions.
fn sanitize(request_path: &str) -> Option<PathBuf> {
    let decoded = percent_decode(request_path)?;
    if decoded.contains('\0') || decoded.contains('\\') {
        return None;
    }
    let rel = PathBuf::from(decoded.trim_start_matches('/'));
    let mut first = true;
    for component in rel.components() {
        let Component::Normal(part) = component else {
            return None;
        };
        let part = part.to_str()?;
        if part.starts_with('.') && !(first && part == ".well-known") {
            return None;
        }
        first = false;
    }
    if rel.as_os_str().is_empty() {
        return None;
    }
    let ext = rel
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    if ext.is_some_and(|e| NEVER_SERVED.contains(&e.as_str())) {
        return None;
    }
    Some(rel)
}

/// Strict `%XX` decoding; `None` on malformed escapes or non-UTF-8 output.
fn percent_decode(s: &str) -> Option<String> {
    if !s.contains('%') {
        return Some(s.to_string());
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            let hex = std::str::from_utf8(hex).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Respond with the file. `HEAD` gets the headers only; a matching
/// `If-Modified-Since` gets `304`.
pub async fn serve(
    file: StaticFile,
    method: &Method,
    request_headers: &HeaderMap,
) -> Response<ResponseBody> {
    let len = file.metadata.len();
    let last_modified = file.metadata.modified().ok().map(http_date);

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type(&file.path)),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if let Some(lm) = last_modified
        .as_deref()
        .and_then(|lm| HeaderValue::from_str(lm).ok())
    {
        headers.insert(header::LAST_MODIFIED, lm);
    }

    let not_modified = last_modified.is_some()
        && request_headers
            .get(header::IF_MODIFIED_SINCE)
            .zip(headers.get(header::LAST_MODIFIED))
            .is_some_and(|(since, lm)| since == lm);
    if not_modified {
        let mut res = Response::new(body::empty());
        *res.status_mut() = StatusCode::NOT_MODIFIED;
        *res.headers_mut() = headers;
        return res;
    }

    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    let body = if method == Method::HEAD {
        body::empty()
    } else {
        match tokio::fs::File::open(&file.path).await {
            Ok(f) => stream_file(f),
            Err(err) => {
                debug!(path = %file.path.display(), error = %err, "static file vanished");
                return crate::server::plain(StatusCode::NOT_FOUND, "not found\n");
            }
        }
    };
    let mut res = Response::new(body);
    *res.headers_mut() = headers;
    res
}

fn stream_file(mut file: tokio::fs::File) -> ResponseBody {
    let (tx, body) = body::channel(4);
    tokio::spawn(async move {
        loop {
            let mut buf = BytesMut::with_capacity(CHUNK_SIZE);
            match file.read_buf(&mut buf).await {
                Ok(0) => return,
                Ok(_) => {
                    if tx.send(Ok(buf.freeze())).await.is_err() {
                        return;
                    }
                }
                Err(err) => {
                    let _ = tx.send(Err(Box::new(err))).await;
                    return;
                }
            }
        }
    });
    body
}

fn content_type(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "pdf" => "application/pdf",
        "wasm" => "application/wasm",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

/// IMF-fixdate (RFC 9110 §5.6.7), e.g. `Sun, 06 Nov 1994 08:49:37 GMT`.
fn http_date(time: SystemTime) -> String {
    let secs = time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (hour, min, sec) = (rem / 3600, rem % 3600 / 60, rem % 60);
    let weekday = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][(days % 7) as usize];

    // Civil-from-days (Howard Hinnant), valid for all dates after 1970.
    let z = days as i64 + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let month_name = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][(month - 1) as usize];

    format!("{weekday}, {day:02} {month_name} {year} {hour:02}:{min:02}:{sec:02} GMT")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn sanitize_accepts_plain_paths() {
        assert_eq!(sanitize("/css/app.css"), Some("css/app.css".into()));
        assert_eq!(sanitize("/a%20b.txt"), Some("a b.txt".into()));
        assert_eq!(
            sanitize("/.well-known/security.txt"),
            Some(".well-known/security.txt".into())
        );
    }

    #[test]
    fn sanitize_rejects_traversal_hidden_and_php() {
        for bad in [
            "/",
            "/../etc/passwd",
            "/a/%2e%2e/%2e%2e/etc/passwd",
            "/.env",
            "/.git/config",
            "/sub/.htaccess",
            "/index.php",
            "/INDEX.PHP",
            "/lib/config.inc",
            "/shell.phtml",
            "/a%00.txt",
            "/a%zz",
            "/a\\..\\b",
            "/x/.well-known/y",
        ] {
            assert_eq!(sanitize(bad), None, "{bad}");
        }
    }

    #[test]
    fn formats_http_dates() {
        let t = UNIX_EPOCH + Duration::from_secs(784_111_777);
        assert_eq!(http_date(t), "Sun, 06 Nov 1994 08:49:37 GMT");
        assert_eq!(http_date(UNIX_EPOCH), "Thu, 01 Jan 1970 00:00:00 GMT");
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400); // 2000-02-29
        assert_eq!(http_date(leap), "Tue, 29 Feb 2000 00:00:00 GMT");
    }

    #[test]
    fn content_types() {
        assert_eq!(content_type(Path::new("a.CSS")), "text/css; charset=utf-8");
        assert_eq!(content_type(Path::new("a.bin")), "application/octet-stream");
    }
}
