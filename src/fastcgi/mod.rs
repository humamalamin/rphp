//! FastCGI wire format, client side (responder role).
//!
//! Spec: <https://fastcgi-archives.github.io/FastCGI_Specification.html>.
//! `std` + `bytes` only, so it can be unit-tested and fuzzed without I/O.
//! Everything decoded here comes from PHP and is treated as untrusted: record
//! lengths are bounded by the 16-bit wire format and checked before use.

use std::fmt;

use bytes::{Buf, Bytes, BytesMut};

pub const VERSION_1: u8 = 1;
pub const HEADER_LEN: usize = 8;
/// Largest content a single record can carry.
pub const MAX_CONTENT_LEN: usize = u16::MAX as usize;

/// The only request id used: RPHP sends one request per connection.
pub const REQUEST_ID: u16 = 1;

pub const BEGIN_REQUEST: u8 = 1;
pub const ABORT_REQUEST: u8 = 2;
pub const END_REQUEST: u8 = 3;
pub const PARAMS: u8 = 4;
pub const STDIN: u8 = 5;
pub const STDOUT: u8 = 6;
pub const STDERR: u8 = 7;

const ROLE_RESPONDER: u16 = 1;
const REQUEST_COMPLETE: u8 = 0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    UnsupportedVersion(u8),
    UnexpectedRequestId(u16),
    MalformedEndRequest(usize),
    /// `END_REQUEST` with a protocol status other than `REQUEST_COMPLETE`.
    Rejected(u8),
    MalformedParams,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersion(v) => write!(f, "unsupported FastCGI version {v}"),
            Self::UnexpectedRequestId(id) => write!(f, "unexpected FastCGI request id {id}"),
            Self::MalformedEndRequest(len) => {
                write!(f, "END_REQUEST record has {len} content bytes, expected 8")
            }
            Self::Rejected(status) => {
                write!(
                    f,
                    "FastCGI application rejected the request (status {status})"
                )
            }
            Self::MalformedParams => f.write_str("malformed FastCGI name-value pairs"),
        }
    }
}

impl std::error::Error for ProtocolError {}

/// A decoded record, already checked for version and request id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    Stdout(Bytes),
    Stderr(Bytes),
    EndRequest {
        app_status: u32,
    },
    /// A record type a client does not act on; skipped by callers.
    Other(u8),
}

/// Append one record, padded to a multiple of 8 bytes.
///
/// Panics if `content` exceeds [`MAX_CONTENT_LEN`]; use [`write_stream`] for
/// arbitrary lengths.
pub fn write_record(buf: &mut Vec<u8>, record_type: u8, content: &[u8]) {
    let len = u16::try_from(content.len()).expect("FastCGI record content too long");
    let padding = (8 - content.len() % 8) % 8;
    buf.reserve(HEADER_LEN + content.len() + padding);
    buf.extend_from_slice(&[VERSION_1, record_type]);
    buf.extend_from_slice(&REQUEST_ID.to_be_bytes());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(&[padding as u8, 0]);
    buf.extend_from_slice(content);
    buf.extend_from_slice(&[0; 7][..padding]);
}

/// Append `data` as a sequence of stream records (without the empty
/// terminating record).
pub fn write_stream(buf: &mut Vec<u8>, record_type: u8, data: &[u8]) {
    for chunk in data.chunks(MAX_CONTENT_LEN) {
        write_record(buf, record_type, chunk);
    }
}

/// Append the empty record that terminates a stream.
pub fn end_stream(buf: &mut Vec<u8>, record_type: u8) {
    write_record(buf, record_type, &[]);
}

/// Append `BEGIN_REQUEST` for the responder role. The server closes the
/// connection after the request (`FCGI_KEEP_CONN` is not set).
pub fn begin_request(buf: &mut Vec<u8>) {
    let mut body = [0u8; 8];
    body[..2].copy_from_slice(&ROLE_RESPONDER.to_be_bytes());
    write_record(buf, BEGIN_REQUEST, &body);
}

/// Encode name-value pairs for a `PARAMS` stream.
pub fn encode_params<'a, I>(pairs: I) -> Vec<u8>
where
    I: IntoIterator<Item = (&'a [u8], &'a [u8])>,
{
    let mut out = Vec::new();
    for (name, value) in pairs {
        encode_len(&mut out, name.len());
        encode_len(&mut out, value.len());
        out.extend_from_slice(name);
        out.extend_from_slice(value);
    }
    out
}

fn encode_len(out: &mut Vec<u8>, len: usize) {
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let len = u32::try_from(len)
            .ok()
            .filter(|&l| l <= 0x7fff_ffff)
            .expect("FastCGI name-value length exceeds 2^31-1");
        out.extend_from_slice(&(len | 0x8000_0000).to_be_bytes());
    }
}

/// A decoded name-value pair.
pub type Param = (Vec<u8>, Vec<u8>);

/// Decode a complete `PARAMS` payload. Lengths are checked against the
/// remaining input before anything is sliced or allocated.
pub fn decode_params(mut data: &[u8]) -> Result<Vec<Param>, ProtocolError> {
    fn len(data: &mut &[u8]) -> Result<usize, ProtocolError> {
        match data.first() {
            None => Err(ProtocolError::MalformedParams),
            Some(&b) if b & 0x80 == 0 => {
                data.advance(1);
                Ok(b as usize)
            }
            Some(_) if data.len() < 4 => Err(ProtocolError::MalformedParams),
            Some(_) => Ok((data.get_u32() & 0x7fff_ffff) as usize),
        }
    }

    let mut pairs = Vec::new();
    while !data.is_empty() {
        let name_len = len(&mut data)?;
        let value_len = len(&mut data)?;
        if data.len() < name_len.saturating_add(value_len) {
            return Err(ProtocolError::MalformedParams);
        }
        let (name, rest) = data.split_at(name_len);
        let (value, rest) = rest.split_at(value_len);
        pairs.push((name.to_vec(), value.to_vec()));
        data = rest;
    }
    Ok(pairs)
}

/// Decode one record from the front of `buf`.
///
/// Returns `Ok(None)` when `buf` does not yet hold a whole record; the caller
/// reads more and tries again. At most `HEADER_LEN + 65535 + 255` bytes are
/// ever needed for one record.
pub fn decode_record(buf: &mut BytesMut) -> Result<Option<Record>, ProtocolError> {
    if buf.len() < HEADER_LEN {
        return Ok(None);
    }
    let version = buf[0];
    let record_type = buf[1];
    let request_id = u16::from_be_bytes([buf[2], buf[3]]);
    let content_len = u16::from_be_bytes([buf[4], buf[5]]) as usize;
    let padding_len = buf[6] as usize;

    if version != VERSION_1 {
        return Err(ProtocolError::UnsupportedVersion(version));
    }
    let total = HEADER_LEN + content_len + padding_len;
    if buf.len() < total {
        buf.reserve(total - buf.len());
        return Ok(None);
    }

    let mut record = buf.split_to(total);
    record.advance(HEADER_LEN);
    let content = record.split_to(content_len).freeze();

    // Management records (request id 0) are never answered by RPHP.
    if request_id == 0 {
        return Ok(Some(Record::Other(record_type)));
    }
    if request_id != REQUEST_ID {
        return Err(ProtocolError::UnexpectedRequestId(request_id));
    }

    Ok(Some(match record_type {
        STDOUT => Record::Stdout(content),
        STDERR => Record::Stderr(content),
        END_REQUEST => {
            if content.len() != 8 {
                return Err(ProtocolError::MalformedEndRequest(content.len()));
            }
            let app_status = u32::from_be_bytes([content[0], content[1], content[2], content[3]]);
            if content[4] != REQUEST_COMPLETE {
                return Err(ProtocolError::Rejected(content[4]));
            }
            Record::EndRequest { app_status }
        }
        other => Record::Other(other),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(bytes: &[u8]) -> Result<Vec<Record>, ProtocolError> {
        let mut buf = BytesMut::from(bytes);
        let mut out = Vec::new();
        while let Some(record) = decode_record(&mut buf)? {
            out.push(record);
        }
        assert!(buf.is_empty(), "trailing bytes: {buf:?}");
        Ok(out)
    }

    #[test]
    fn records_are_padded_to_eight_bytes() {
        for len in [0, 1, 7, 8, 9, 1000] {
            let mut buf = Vec::new();
            write_record(&mut buf, STDOUT, &vec![b'x'; len]);
            assert_eq!(buf.len() % 8, 0, "len {len}");
            assert_eq!(buf[6] as usize, (8 - len % 8) % 8);
        }
    }

    #[test]
    fn stream_round_trip_across_record_boundary() {
        let data: Vec<u8> = (0..200_000u32).map(|i| i as u8).collect();
        let mut buf = Vec::new();
        write_stream(&mut buf, STDOUT, &data);
        end_stream(&mut buf, STDOUT);

        let mut got = Vec::new();
        let records = decode_all(&buf).unwrap();
        assert_eq!(records.len(), 5, "4 data records + terminator");
        for record in records {
            let Record::Stdout(chunk) = record else {
                panic!("unexpected {record:?}")
            };
            got.extend_from_slice(&chunk);
        }
        assert_eq!(got, data);
    }

    #[test]
    fn partial_input_waits_for_more() {
        let mut full = Vec::new();
        write_record(&mut full, STDOUT, b"hello");
        for cut in 0..full.len() {
            let mut buf = BytesMut::from(&full[..cut]);
            assert_eq!(decode_record(&mut buf), Ok(None), "cut at {cut}");
            assert_eq!(buf.len(), cut, "nothing consumed");
        }
    }

    #[test]
    fn end_request_is_decoded_and_checked() {
        let mut buf = Vec::new();
        write_record(&mut buf, END_REQUEST, &[0, 0, 0, 3, 0, 0, 0, 0]);
        assert_eq!(
            decode_all(&buf).unwrap(),
            vec![Record::EndRequest { app_status: 3 }]
        );

        let mut buf = Vec::new();
        write_record(&mut buf, END_REQUEST, &[0, 0, 0, 0, 2, 0, 0, 0]);
        assert_eq!(decode_all(&buf), Err(ProtocolError::Rejected(2)));

        let mut buf = Vec::new();
        write_record(&mut buf, END_REQUEST, &[0; 4]);
        assert_eq!(decode_all(&buf), Err(ProtocolError::MalformedEndRequest(4)));
    }

    #[test]
    fn rejects_bad_version_and_request_id() {
        let mut buf = Vec::new();
        write_record(&mut buf, STDOUT, b"x");
        buf[0] = 2;
        assert_eq!(decode_all(&buf), Err(ProtocolError::UnsupportedVersion(2)));

        let mut buf = Vec::new();
        write_record(&mut buf, STDOUT, b"x");
        buf[3] = 9;
        assert_eq!(decode_all(&buf), Err(ProtocolError::UnexpectedRequestId(9)));
    }

    #[test]
    fn management_and_unknown_records_are_skippable() {
        let mut buf = Vec::new();
        write_record(&mut buf, 10, b"anything");
        buf[2] = 0;
        buf[3] = 0;
        write_record(&mut buf, 42, b"");
        assert_eq!(
            decode_all(&buf).unwrap(),
            vec![Record::Other(10), Record::Other(42)]
        );
    }

    #[test]
    fn params_round_trip_short_and_long() {
        let long = vec![b'v'; 300];
        let pairs: Vec<(&[u8], &[u8])> = vec![
            (b"REQUEST_METHOD", b"GET"),
            (b"EMPTY", b""),
            (b"LONG", &long),
        ];
        let encoded = encode_params(pairs.iter().copied());
        // 1-byte lengths for short pairs, 4-byte length for the 300-byte value.
        assert_eq!(&encoded[..2], &[14, 3]);
        let decoded = decode_params(&encoded).unwrap();
        assert_eq!(decoded.len(), 3);
        assert_eq!(decoded[2].1, long);
    }

    #[test]
    fn params_lengths_are_checked_before_slicing() {
        assert_eq!(decode_params(&[5]), Err(ProtocolError::MalformedParams));
        assert_eq!(
            decode_params(&[1, 1, b'a']),
            Err(ProtocolError::MalformedParams)
        );
        assert_eq!(
            decode_params(&[0xff, 0xff, 0xff, 0xff, 0, b'a']),
            Err(ProtocolError::MalformedParams)
        );
        assert_eq!(
            decode_params(&[0x80, 0]),
            Err(ProtocolError::MalformedParams)
        );
    }

    #[test]
    fn begin_request_is_responder_without_keep_conn() {
        let mut buf = Vec::new();
        begin_request(&mut buf);
        assert_eq!(buf.len(), 16);
        assert_eq!(buf[1], BEGIN_REQUEST);
        assert_eq!(&buf[8..10], &[0, 1]);
        assert_eq!(buf[10], 0, "flags");
    }
}
