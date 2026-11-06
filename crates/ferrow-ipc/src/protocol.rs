//! Versioned, length-prefixed binary protocol (M2a).
//!
//! Wire format, all integers little-endian:
//!
//! ```text
//! frame   := u32 payload_len || payload        (payload_len <= MAX_FRAME_BYTES, > 0)
//! payload := u8 frame_version || u8 kind || u32 request_id || body
//! str     := u32 len || utf-8 bytes            (len <= MAX_TEXT_BYTES)
//! bytes   := u32 len || raw bytes              (len <= MAX_PATH_BYTES)
//! ```
//!
//! The protocol version is negotiated with `Hello{version}` as the
//! first message on a connection; `frame_version` carries the same
//! value on every frame so a future framing change is detectable
//! before the body is interpreted. The parser never panics: every
//! length is checked against the remaining input and the limits
//! below before any allocation, and trailing bytes are rejected.

use std::io::{Read, Write};

/// The only protocol version this build speaks.
pub const PROTOCOL_VERSION: u16 = 1;
/// Hard cap on one frame's payload. Results are paged to fit.
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
/// Hard cap on a length-prefixed text field (query, mode, message).
pub const MAX_TEXT_BYTES: usize = 64 * 1024;
/// Hard cap on one path in a `Results` page.
pub const MAX_PATH_BYTES: usize = 64 * 1024;
/// Hard cap on paths in one `Results` page.
pub const MAX_PATHS_PER_PAGE: usize = 10_000;

const KIND_HELLO: u8 = 1;
const KIND_QUERY: u8 = 2;
const KIND_COUNT: u8 = 3;
const KIND_STATUS: u8 = 4;
const KIND_CANCEL: u8 = 5;
const KIND_HELLO_OK: u8 = 101;
const KIND_RESULTS: u8 = 102;
const KIND_COUNT_RESULT: u8 = 103;
const KIND_STATUS_RESULT: u8 = 104;
const KIND_ERROR: u8 = 105;

/// Result sort order. Only name order exists in protocol version 1;
/// the field is on the wire so later versions can add orders
/// without changing the `Query` layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sort {
    /// Index name order (folded name, then raw name, then id).
    #[default]
    Name,
}

/// A client request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Hello {
        version: u16,
    },
    Query {
        text: String,
        /// Maximum hits to return; 0 means no limit.
        limit: u32,
        sort: Sort,
        /// Opaque query flags, reserved; version 1 defines none.
        flags: u32,
    },
    Count {
        text: String,
    },
    Status,
    /// Cancel the in-flight request with this request id.
    Cancel {
        target_id: u32,
    },
}

/// Machine-readable error codes in `Response::Error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    VersionMismatch = 1,
    BadRequest = 2,
    Internal = 3,
    Cancelled = 4,
    TooLarge = 5,
}

/// A server response. `Results` is streamed in pages: every page but
/// the last has `more = true`, all under the request's id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    HelloOk {
        version: u16,
    },
    Results {
        paths: Vec<Vec<u8>>,
        more: bool,
    },
    Count {
        n: u64,
    },
    Status {
        entries: u64,
        mode: String,
        report: String,
    },
    Error {
        code: ErrorCode,
        msg: String,
    },
}

/// One message in either direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Request(Request),
    Response(Response),
}

/// A decoded frame: the wire version byte (not yet validated
/// against [`PROTOCOL_VERSION`]; the handshake does that), the
/// request id the message belongs to, and the message itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    pub version: u8,
    pub id: u32,
    pub message: Message,
}

/// A protocol-level decode or encode failure. Never a panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    /// A length field exceeds a hard limit.
    TooLarge,
    /// Input ended before a complete value could be read.
    Truncated,
    /// Bytes remain after a complete message body.
    TrailingBytes,
    UnknownKind(u8),
    InvalidUtf8,
    InvalidValue(&'static str),
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtocolError::TooLarge => f.write_str("message exceeds a size limit"),
            ProtocolError::Truncated => f.write_str("message is truncated"),
            ProtocolError::TrailingBytes => f.write_str("trailing bytes after message"),
            ProtocolError::UnknownKind(k) => write!(f, "unknown message kind {k}"),
            ProtocolError::InvalidUtf8 => f.write_str("text field is not valid UTF-8"),
            ProtocolError::InvalidValue(what) => write!(f, "invalid value: {what}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

impl From<ProtocolError> for std::io::Error {
    fn from(e: ProtocolError) -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
    }
}

struct Writer(Vec<u8>);

impl Writer {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn str(&mut self, v: &str) -> Result<(), ProtocolError> {
        if v.len() > MAX_TEXT_BYTES {
            return Err(ProtocolError::TooLarge);
        }
        self.u32(v.len() as u32);
        self.0.extend_from_slice(v.as_bytes());
        Ok(())
    }
    fn bytes(&mut self, v: &[u8]) -> Result<(), ProtocolError> {
        if v.len() > MAX_PATH_BYTES {
            return Err(ProtocolError::TooLarge);
        }
        self.u32(v.len() as u32);
        self.0.extend_from_slice(v);
        Ok(())
    }
}

fn encode_kind(message: &Message, w: &mut Writer) -> Result<u8, ProtocolError> {
    Ok(match message {
        Message::Request(Request::Hello { version }) => {
            w.u16(*version);
            KIND_HELLO
        }
        Message::Request(Request::Query {
            text,
            limit,
            sort,
            flags,
        }) => {
            w.str(text)?;
            w.u32(*limit);
            w.u8(match sort {
                Sort::Name => 0,
            });
            w.u32(*flags);
            KIND_QUERY
        }
        Message::Request(Request::Count { text }) => {
            w.str(text)?;
            KIND_COUNT
        }
        Message::Request(Request::Status) => KIND_STATUS,
        Message::Request(Request::Cancel { target_id }) => {
            w.u32(*target_id);
            KIND_CANCEL
        }
        Message::Response(Response::HelloOk { version }) => {
            w.u16(*version);
            KIND_HELLO_OK
        }
        Message::Response(Response::Results { paths, more }) => {
            if paths.len() > MAX_PATHS_PER_PAGE {
                return Err(ProtocolError::TooLarge);
            }
            w.u8(u8::from(*more));
            w.u32(paths.len() as u32);
            for path in paths {
                w.bytes(path)?;
            }
            KIND_RESULTS
        }
        Message::Response(Response::Count { n }) => {
            w.u64(*n);
            KIND_COUNT_RESULT
        }
        Message::Response(Response::Status {
            entries,
            mode,
            report,
        }) => {
            w.u64(*entries);
            w.str(mode)?;
            w.str(report)?;
            KIND_STATUS_RESULT
        }
        Message::Response(Response::Error { code, msg }) => {
            w.u16(*code as u16);
            w.str(msg)?;
            KIND_ERROR
        }
    })
}

/// Encode one message into a complete frame (length prefix
/// included). Fails with [`ProtocolError::TooLarge`] instead of
/// producing a frame the peer would reject.
pub fn encode_frame(id: u32, message: &Message) -> Result<Vec<u8>, ProtocolError> {
    let mut body = Writer(Vec::new());
    let kind = encode_kind(message, &mut body)?;
    let payload_len = 1 + 1 + 4 + body.0.len();
    if payload_len > MAX_FRAME_BYTES {
        return Err(ProtocolError::TooLarge);
    }
    let mut frame = Vec::with_capacity(4 + payload_len);
    frame.extend_from_slice(&(payload_len as u32).to_le_bytes());
    frame.push(PROTOCOL_VERSION as u8);
    frame.push(kind);
    frame.extend_from_slice(&id.to_le_bytes());
    frame.extend_from_slice(&body.0);
    Ok(frame)
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], ProtocolError> {
        let end = self.pos.checked_add(n).ok_or(ProtocolError::TooLarge)?;
        let out = self
            .bytes
            .get(self.pos..end)
            .ok_or(ProtocolError::Truncated)?;
        self.pos = end;
        Ok(out)
    }
    fn u8(&mut self) -> Result<u8, ProtocolError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, ProtocolError> {
        Ok(u16::from_le_bytes(
            self.take(2)?.try_into().expect("2 bytes"),
        ))
    }
    fn u32(&mut self) -> Result<u32, ProtocolError> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }
    fn u64(&mut self) -> Result<u64, ProtocolError> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }
    fn len_prefixed(&mut self, cap: usize) -> Result<&'a [u8], ProtocolError> {
        let len = self.u32()? as usize;
        if len > cap {
            return Err(ProtocolError::TooLarge);
        }
        self.take(len)
    }
    fn str(&mut self) -> Result<String, ProtocolError> {
        let raw = self.len_prefixed(MAX_TEXT_BYTES)?;
        std::str::from_utf8(raw)
            .map(str::to_owned)
            .map_err(|_| ProtocolError::InvalidUtf8)
    }
    fn finish(&self) -> Result<(), ProtocolError> {
        if self.pos == self.bytes.len() {
            Ok(())
        } else {
            Err(ProtocolError::TrailingBytes)
        }
    }
}

fn error_code(v: u16) -> Result<ErrorCode, ProtocolError> {
    Ok(match v {
        1 => ErrorCode::VersionMismatch,
        2 => ErrorCode::BadRequest,
        3 => ErrorCode::Internal,
        4 => ErrorCode::Cancelled,
        5 => ErrorCode::TooLarge,
        _ => return Err(ProtocolError::InvalidValue("error code")),
    })
}

/// Decode one frame payload (without the length prefix). Total:
/// every input yields a message or a [`ProtocolError`], never a
/// panic and never an allocation beyond the documented limits.
pub fn decode_payload(payload: &[u8]) -> Result<Envelope, ProtocolError> {
    if payload.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::TooLarge);
    }
    let mut r = Reader {
        bytes: payload,
        pos: 0,
    };
    let version = r.u8()?;
    let kind = r.u8()?;
    let id = r.u32()?;
    let message = match kind {
        KIND_HELLO => Message::Request(Request::Hello { version: r.u16()? }),
        KIND_QUERY => {
            let text = r.str()?;
            let limit = r.u32()?;
            let sort = match r.u8()? {
                0 => Sort::Name,
                _ => return Err(ProtocolError::InvalidValue("sort")),
            };
            let flags = r.u32()?;
            Message::Request(Request::Query {
                text,
                limit,
                sort,
                flags,
            })
        }
        KIND_COUNT => Message::Request(Request::Count { text: r.str()? }),
        KIND_STATUS => Message::Request(Request::Status),
        KIND_CANCEL => Message::Request(Request::Cancel {
            target_id: r.u32()?,
        }),
        KIND_HELLO_OK => Message::Response(Response::HelloOk { version: r.u16()? }),
        KIND_RESULTS => {
            let more = match r.u8()? {
                0 => false,
                1 => true,
                _ => return Err(ProtocolError::InvalidValue("more flag")),
            };
            let count = r.u32()? as usize;
            if count > MAX_PATHS_PER_PAGE {
                return Err(ProtocolError::TooLarge);
            }
            let mut paths = Vec::with_capacity(count);
            for _ in 0..count {
                paths.push(r.len_prefixed(MAX_PATH_BYTES)?.to_vec());
            }
            Message::Response(Response::Results { paths, more })
        }
        KIND_COUNT_RESULT => Message::Response(Response::Count { n: r.u64()? }),
        KIND_STATUS_RESULT => {
            let entries = r.u64()?;
            let mode = r.str()?;
            let report = r.str()?;
            Message::Response(Response::Status {
                entries,
                mode,
                report,
            })
        }
        KIND_ERROR => {
            let code = error_code(r.u16()?)?;
            let msg = r.str()?;
            Message::Response(Response::Error { code, msg })
        }
        other => return Err(ProtocolError::UnknownKind(other)),
    };
    r.finish()?;
    Ok(Envelope {
        version,
        id,
        message,
    })
}

/// Write one complete frame and flush.
pub fn write_message<W: Write>(w: &mut W, id: u32, message: &Message) -> std::io::Result<()> {
    let frame = encode_frame(id, message)?;
    w.write_all(&frame)?;
    w.flush()
}

/// Read one frame. `Ok(None)` is a clean end of stream exactly at
/// a frame boundary; a partial frame is `UnexpectedEof`, and an
/// over-limit length prefix is rejected before allocating.
pub fn read_message<R: Read>(r: &mut R) -> std::io::Result<Option<Envelope>> {
    let mut len_buf = [0u8; 4];
    // Read the first prefix byte separately so a clean EOF at a
    // frame boundary (0 bytes) is told apart from a partial
    // prefix (1-3 bytes then EOF), which is an error.
    if r.read(&mut len_buf[0..1])? == 0 {
        return Ok(None);
    }
    r.read_exact(&mut len_buf[1..])?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len == 0 || len > MAX_FRAME_BYTES {
        return Err(ProtocolError::TooLarge.into());
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(Some(decode_payload(&payload)?))
}
