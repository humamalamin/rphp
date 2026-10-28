//! Persistent-worker wire protocol, version 1 (see `docs/worker-protocol.md`).
//!
//! Every frame is a 12-byte big-endian header followed by its payload:
//!
//! ```text
//! 0..2   magic "RW"
//! 2      protocol version (1)
//! 3      frame type
//! 4..8   request id (u32; 0 for HELLO)
//! 8..12  payload length (u32, at most MAX_PAYLOAD)
//! ```
//!
//! Heads and trailers are JSON; bodies are raw bytes. Everything decoded here
//! comes from PHP and is treated as untrusted: the length is checked against
//! [`MAX_PAYLOAD`] before any buffer grows.

use std::fmt;

use bytes::{Buf, Bytes, BytesMut};
use serde::{Deserialize, Serialize};

pub const MAGIC: [u8; 2] = *b"RW";
pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 12;
/// Largest payload in either direction.
pub const MAX_PAYLOAD: usize = 1024 * 1024;
/// Largest JSON head (request or response).
pub const MAX_HEAD: usize = 64 * 1024;
/// Body bytes per frame sent to PHP.
pub const BODY_CHUNK: usize = 256 * 1024;

/// Worker -> RPHP, once after connecting: `Hello`.
pub const HELLO: u8 = 0x01;
/// RPHP -> worker: `RequestHead`, then body frames, then `REQUEST_END`.
pub const REQUEST: u8 = 0x10;
pub const REQUEST_BODY: u8 = 0x11;
pub const REQUEST_END: u8 = 0x12;
/// Worker -> RPHP: `ResponseHead`, then body frames, then `ResponseEnd`.
pub const RESPONSE: u8 = 0x20;
pub const RESPONSE_BODY: u8 = 0x21;
pub const RESPONSE_END: u8 = 0x22;
/// Worker -> RPHP instead of (or in the middle of) a response: `WorkerError`.
pub const ERROR: u8 = 0x2f;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    BadMagic,
    UnsupportedVersion(u8),
    TooLarge { frame_type: u8, len: usize },
    Json { frame_type: u8, message: String },
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic => f.write_str("not a worker protocol frame (bad magic)"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported worker protocol version {v}"),
            Self::TooLarge { frame_type, len } => {
                write!(
                    f,
                    "frame 0x{frame_type:02x} payload of {len} bytes exceeds the limit"
                )
            }
            Self::Json {
                frame_type,
                message,
            } => write!(f, "invalid JSON in frame 0x{frame_type:02x}: {message}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub frame_type: u8,
    pub request_id: u32,
    pub payload: Bytes,
}

impl Frame {
    /// Decode a JSON payload, enforcing [`MAX_HEAD`].
    pub fn json<'a, T: Deserialize<'a>>(&'a self) -> Result<T, ProtocolError> {
        if self.payload.len() > MAX_HEAD {
            return Err(ProtocolError::TooLarge {
                frame_type: self.frame_type,
                len: self.payload.len(),
            });
        }
        serde_json::from_slice(&self.payload).map_err(|err| ProtocolError::Json {
            frame_type: self.frame_type,
            message: err.to_string(),
        })
    }
}

/// The 12-byte header of a frame carrying `len` payload bytes, or an error
/// if `len` exceeds [`MAX_PAYLOAD`].
pub fn frame_header(
    frame_type: u8,
    request_id: u32,
    len: usize,
) -> Result<[u8; HEADER_LEN], ProtocolError> {
    if len > MAX_PAYLOAD {
        return Err(ProtocolError::TooLarge { frame_type, len });
    }
    let mut header = [0u8; HEADER_LEN];
    header[..2].copy_from_slice(&MAGIC);
    header[2] = VERSION;
    header[3] = frame_type;
    header[4..8].copy_from_slice(&request_id.to_be_bytes());
    header[8..].copy_from_slice(&(len as u32).to_be_bytes());
    Ok(header)
}

/// Append one frame (header and payload).
pub fn write_frame(
    buf: &mut Vec<u8>,
    frame_type: u8,
    request_id: u32,
    payload: &[u8],
) -> Result<(), ProtocolError> {
    let header = frame_header(frame_type, request_id, payload.len())?;
    buf.reserve(HEADER_LEN + payload.len());
    buf.extend_from_slice(&header);
    buf.extend_from_slice(payload);
    Ok(())
}

/// Decode one frame from the front of `buf`; `Ok(None)` until it is complete.
pub fn decode_frame(buf: &mut BytesMut) -> Result<Option<Frame>, ProtocolError> {
    if buf.len() < HEADER_LEN {
        return Ok(None);
    }
    if buf[..2] != MAGIC {
        return Err(ProtocolError::BadMagic);
    }
    if buf[2] != VERSION {
        return Err(ProtocolError::UnsupportedVersion(buf[2]));
    }
    let frame_type = buf[3];
    let request_id = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let len = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]) as usize;
    if len > MAX_PAYLOAD {
        return Err(ProtocolError::TooLarge { frame_type, len });
    }
    if buf.len() < HEADER_LEN + len {
        buf.reserve(HEADER_LEN + len - buf.len());
        return Ok(None);
    }
    buf.advance(HEADER_LEN);
    let payload = buf.split_to(len).freeze();
    Ok(Some(Frame {
        frame_type,
        request_id,
        payload,
    }))
}

/// Sent by the worker right after it connects.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Hello {
    pub protocol: u8,
    pub pid: u32,
    /// `PHP_VERSION` of the worker.
    pub php: String,
}

/// The request as the worker sees it. Header names are lowercase; repeated
/// headers appear as repeated pairs, in order.
#[derive(Debug, Clone, Serialize)]
pub struct RequestHead {
    pub method: String,
    /// Path and query exactly as received (`REQUEST_URI`).
    pub uri: String,
    pub path: String,
    pub query: String,
    pub protocol: String,
    pub scheme: String,
    pub headers: Vec<(String, String)>,
    pub remote_addr: String,
    pub remote_port: u16,
    pub server_addr: String,
    pub server_port: u16,
    pub server_name: String,
    pub document_root: String,
    pub body_length: usize,
    /// Time left until the request deadline, in milliseconds.
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ResponseHead {
    pub status: u16,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ResponseEnd {
    /// `memory_get_usage()` after the request, in bytes.
    pub memory: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WorkerError {
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(bytes: &[u8]) -> Result<Vec<Frame>, ProtocolError> {
        let mut buf = BytesMut::from(bytes);
        let mut out = Vec::new();
        while let Some(frame) = decode_frame(&mut buf)? {
            out.push(frame);
        }
        assert!(buf.is_empty(), "trailing bytes");
        Ok(out)
    }

    #[test]
    fn round_trip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, REQUEST, 7, br#"{"a":1}"#).unwrap();
        write_frame(&mut buf, REQUEST_END, 7, b"").unwrap();
        let frames = decode_all(&buf).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].frame_type, REQUEST);
        assert_eq!(frames[0].request_id, 7);
        assert_eq!(&frames[0].payload[..], br#"{"a":1}"#);
        assert!(frames[1].payload.is_empty());
    }

    #[test]
    fn partial_input_waits_for_more() {
        let mut full = Vec::new();
        write_frame(&mut full, RESPONSE_BODY, 1, b"hello").unwrap();
        for cut in 0..full.len() {
            let mut buf = BytesMut::from(&full[..cut]);
            assert_eq!(decode_frame(&mut buf), Ok(None), "cut at {cut}");
            assert_eq!(buf.len(), cut);
        }
    }

    #[test]
    fn rejects_bad_magic_version_and_oversized_length_before_buffering() {
        assert_eq!(
            decode_all(b"XX\x01\x20\0\0\0\x01\0\0\0\0"),
            Err(ProtocolError::BadMagic)
        );
        assert_eq!(
            decode_all(b"RW\x02\x20\0\0\0\x01\0\0\0\0"),
            Err(ProtocolError::UnsupportedVersion(2))
        );
        // A 4 GiB length is refused from the header alone.
        let mut buf = BytesMut::from(&b"RW\x01\x21\0\0\0\x01\xff\xff\xff\xff"[..]);
        assert!(matches!(
            decode_frame(&mut buf),
            Err(ProtocolError::TooLarge { .. })
        ));
        assert!(
            buf.capacity() < MAX_PAYLOAD,
            "buffer grew for a rejected frame"
        );
    }

    #[test]
    fn oversized_payloads_are_refused_when_writing() {
        let mut buf = Vec::new();
        let err = write_frame(&mut buf, REQUEST, 1, &vec![0; MAX_PAYLOAD + 1]).unwrap_err();
        assert!(matches!(err, ProtocolError::TooLarge { .. }));
        assert!(buf.is_empty());
    }

    #[test]
    fn json_payloads_are_bounded_and_validated() {
        let frame = Frame {
            frame_type: RESPONSE,
            request_id: 1,
            payload: Bytes::from_static(br#"{"status":201,"headers":[["x-a","1"],["x-a","2"]]}"#),
        };
        let head: ResponseHead = frame.json().unwrap();
        assert_eq!(head.status, 201);
        assert_eq!(head.headers.len(), 2);

        let bad = Frame {
            payload: Bytes::from_static(b"{not json"),
            ..frame.clone()
        };
        assert!(matches!(
            bad.json::<ResponseHead>(),
            Err(ProtocolError::Json { .. })
        ));

        let huge = Frame {
            payload: Bytes::from(vec![b' '; MAX_HEAD + 1]),
            ..frame
        };
        assert!(matches!(
            huge.json::<ResponseHead>(),
            Err(ProtocolError::TooLarge { .. })
        ));
    }

    #[test]
    fn request_head_serializes_headers_as_pairs() {
        let head = RequestHead {
            method: "GET".into(),
            uri: "/a?b=1".into(),
            path: "/a".into(),
            query: "b=1".into(),
            protocol: "HTTP/1.1".into(),
            scheme: "http".into(),
            headers: vec![("cookie".into(), "a=1".into())],
            remote_addr: "127.0.0.1".into(),
            remote_port: 5000,
            server_addr: "127.0.0.1".into(),
            server_port: 8080,
            server_name: "localhost".into(),
            document_root: "/srv/public".into(),
            body_length: 0,
            timeout_ms: 30_000,
        };
        let json = serde_json::to_string(&head).unwrap();
        assert!(json.contains(r#""headers":[["cookie","a=1"]]"#), "{json}");
    }
}
