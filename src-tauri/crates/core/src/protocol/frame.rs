//! Wire framing: `len: u32 BE | kind: u8 | payload`, where `len` counts the kind byte plus
//! the payload (so `len >= 1`). The decoder is sans-IO; `read_frame`/`write_frame` are thin
//! blocking helpers over it for callers that own a `Read`/`Write`.

use std::fmt;
use std::io::{self, Read, Write};
use uuid::Uuid;

pub const KIND_JSON: u8 = 0;
pub const KIND_OUTPUT: u8 = 1;
/// `save_dropped_file` carries up to 25 MiB base64'd (~33.4 MiB) inside one JSON frame.
pub const MAX_FRAME_LEN: u32 = 64 * 1024 * 1024;
const LEN_BYTES: usize = 4;
const UUID_BYTES: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// Raw UTF-8 JSON bytes; parsed by `msg`.
    Json(Vec<u8>),
    /// Terminal output: the payload is the 16-byte UUID followed by raw bytes.
    Output { terminal: Uuid, data: Vec<u8> },
    /// A kind this side does not know. Receivers skip it (new kinds are capability-gated).
    Unknown { kind: u8, payload: Vec<u8> },
}

#[derive(Debug)]
pub enum FrameError {
    /// `len == 0`: not even a kind byte.
    Empty,
    /// `len` above the maximum; rejected from the header, never allocated.
    TooLarge(u32),
    /// A kind-1 frame whose payload is shorter than a UUID.
    ShortOutput(usize),
    /// The stream ended inside a frame.
    TruncatedEof,
    Io(io::Error),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::Empty => write!(f, "empty frame"),
            FrameError::TooLarge(n) => write!(f, "frame too large ({n} bytes)"),
            FrameError::ShortOutput(n) => write!(f, "output frame too short ({n} bytes)"),
            FrameError::TruncatedEof => write!(f, "stream ended inside a frame"),
            FrameError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for FrameError {}

impl From<io::Error> for FrameError {
    fn from(e: io::Error) -> Self {
        FrameError::Io(e)
    }
}

fn check_len(len: u32, max: u32) -> Result<(), FrameError> {
    if len == 0 {
        Err(FrameError::Empty)
    } else if len > max {
        Err(FrameError::TooLarge(len))
    } else {
        Ok(())
    }
}

fn parse_body(kind: u8, payload: Vec<u8>) -> Result<Frame, FrameError> {
    match kind {
        KIND_JSON => Ok(Frame::Json(payload)),
        KIND_OUTPUT => {
            if payload.len() < UUID_BYTES {
                return Err(FrameError::ShortOutput(payload.len()));
            }
            let mut id = [0u8; UUID_BYTES];
            id.copy_from_slice(&payload[..UUID_BYTES]);
            let mut data = payload;
            data.drain(..UUID_BYTES);
            Ok(Frame::Output {
                terminal: Uuid::from_bytes(id),
                data,
            })
        }
        kind => Ok(Frame::Unknown { kind, payload }),
    }
}

/// Incremental decoder: `feed` bytes as they arrive, then call `next_frame` until `None`.
#[derive(Debug)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    max: u32,
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::with_max(MAX_FRAME_LEN)
    }

    pub fn with_max(max: u32) -> Self {
        Self {
            buf: Vec::new(),
            max,
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next complete frame, or `None` when more bytes are needed. An error is final:
    /// the stream can no longer be trusted to be at a frame boundary.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, FrameError> {
        if self.buf.len() < LEN_BYTES {
            return Ok(None);
        }
        let len = u32::from_be_bytes(self.buf[..LEN_BYTES].try_into().unwrap());
        check_len(len, self.max)?;
        let total = LEN_BYTES + len as usize;
        if self.buf.len() < total {
            return Ok(None);
        }
        let kind = self.buf[LEN_BYTES];
        let payload = self.buf[LEN_BYTES + 1..total].to_vec();
        self.buf.drain(..total);
        parse_body(kind, payload).map(Some)
    }

    /// No partial frame is buffered, so an EOF now is a clean one.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

fn frame_len(payload_len: usize) -> Result<u32, FrameError> {
    let len = payload_len + 1;
    if len > MAX_FRAME_LEN as usize {
        return Err(FrameError::TooLarge(len.min(u32::MAX as usize) as u32));
    }
    Ok(len as u32)
}

fn encode_raw(kind: u8, parts: &[&[u8]], out: &mut Vec<u8>) -> Result<(), FrameError> {
    let len = frame_len(parts.iter().map(|p| p.len()).sum())?;
    out.reserve(LEN_BYTES + len as usize);
    out.extend_from_slice(&len.to_be_bytes());
    out.push(kind);
    for p in parts {
        out.extend_from_slice(p);
    }
    Ok(())
}

/// Append a kind-0 frame. Fails with `TooLarge` if it would exceed `MAX_FRAME_LEN`.
pub fn encode_json(payload: &[u8], out: &mut Vec<u8>) -> Result<(), FrameError> {
    encode_raw(KIND_JSON, &[payload], out)
}

/// Append a kind-1 frame. Fails with `TooLarge` if it would exceed `MAX_FRAME_LEN`.
pub fn encode_output(terminal: &Uuid, data: &[u8], out: &mut Vec<u8>) -> Result<(), FrameError> {
    encode_raw(KIND_OUTPUT, &[terminal.as_bytes(), data], out)
}

/// Read until `buf` is full. Returns how many bytes were read before EOF.
fn read_full<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// Read one frame. `Ok(None)` is a clean EOF at a frame boundary. The 4-byte length is read
/// and validated before anything else, so an oversized header never allocates.
pub fn read_frame<R: Read>(r: &mut R, max: u32) -> Result<Option<Frame>, FrameError> {
    let mut len_buf = [0u8; LEN_BYTES];
    match read_full(r, &mut len_buf)? {
        0 => return Ok(None),
        LEN_BYTES => {}
        _ => return Err(FrameError::TruncatedEof),
    }
    let len = u32::from_be_bytes(len_buf);
    check_len(len, max)?;
    let mut body = vec![0u8; len as usize];
    if read_full(r, &mut body)? < body.len() {
        return Err(FrameError::TruncatedEof);
    }
    let kind = body.remove(0);
    parse_body(kind, body).map(Some)
}

/// Write one frame and flush.
pub fn write_frame<W: Write>(w: &mut W, f: &Frame) -> io::Result<()> {
    let mut out = Vec::new();
    let res = match f {
        Frame::Json(p) => encode_json(p, &mut out),
        Frame::Output { terminal, data } => encode_output(terminal, data, &mut out),
        Frame::Unknown { kind, payload } => encode_raw(*kind, &[payload], &mut out),
    };
    res.map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
    w.write_all(&out)?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn json(p: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        encode_json(p, &mut v).unwrap();
        v
    }

    #[test]
    fn json_frame_roundtrip() {
        let bytes = json(br#"{"t":"x"}"#);
        assert_eq!(&bytes[..4], &10u32.to_be_bytes());
        assert_eq!(bytes[4], KIND_JSON);
        let f = read_frame(&mut Cursor::new(bytes), MAX_FRAME_LEN)
            .unwrap()
            .unwrap();
        assert_eq!(f, Frame::Json(br#"{"t":"x"}"#.to_vec()));
    }

    #[test]
    fn output_frame_roundtrip() {
        let id = Uuid::new_v4();
        let mut bytes = Vec::new();
        encode_output(&id, b"\x1b[1mhi", &mut bytes).unwrap();
        assert_eq!(bytes[4], KIND_OUTPUT);
        assert_eq!(&bytes[5..21], id.as_bytes());
        let f = read_frame(&mut Cursor::new(bytes), MAX_FRAME_LEN)
            .unwrap()
            .unwrap();
        assert_eq!(
            f,
            Frame::Output {
                terminal: id,
                data: b"\x1b[1mhi".to_vec()
            }
        );
    }

    fn three() -> (Vec<u8>, Vec<Frame>) {
        let id = Uuid::new_v4();
        let mut bytes = json(b"{}");
        encode_output(&id, b"abc", &mut bytes).unwrap();
        bytes.extend(json(b"[1]"));
        let frames = vec![
            Frame::Json(b"{}".to_vec()),
            Frame::Output {
                terminal: id,
                data: b"abc".to_vec(),
            },
            Frame::Json(b"[1]".to_vec()),
        ];
        (bytes, frames)
    }

    #[test]
    fn decoder_byte_by_byte() {
        let (bytes, frames) = three();
        let mut d = FrameDecoder::new();
        let mut got = Vec::new();
        for b in bytes {
            d.feed(&[b]);
            if let Some(f) = d.next_frame().unwrap() {
                got.push(f);
                assert!(d.next_frame().unwrap().is_none());
            }
        }
        assert_eq!(got, frames);
        assert!(d.is_empty());
    }

    #[test]
    fn decoder_many_in_one_feed() {
        let (bytes, frames) = three();
        let mut d = FrameDecoder::new();
        d.feed(&bytes);
        let mut got = Vec::new();
        while let Some(f) = d.next_frame().unwrap() {
            got.push(f);
        }
        assert_eq!(got, frames);
    }

    #[test]
    fn zero_length_is_error() {
        let r = read_frame(&mut Cursor::new(vec![0, 0, 0, 0]), MAX_FRAME_LEN);
        assert!(matches!(r, Err(FrameError::Empty)));
        let mut d = FrameDecoder::new();
        d.feed(&[0, 0, 0, 0]);
        assert!(matches!(d.next_frame(), Err(FrameError::Empty)));
    }

    /// Counts the bytes handed out, to prove the reader stops at the length field.
    struct Counting<R> {
        inner: R,
        read: usize,
    }
    impl<R: Read> Read for Counting<R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.read += n;
            Ok(n)
        }
    }

    #[test]
    fn oversize_rejected_before_alloc() {
        let mut bytes = vec![0xFF, 0xFF, 0xFF, 0xFF];
        bytes.extend(std::iter::repeat_n(7u8, 64));
        let mut r = Counting {
            inner: Cursor::new(bytes),
            read: 0,
        };
        // A 1-byte read buffer makes the count exact.
        let mut r1 = io::BufReader::with_capacity(1, &mut r);
        let res = read_frame(&mut r1, MAX_FRAME_LEN);
        assert!(matches!(res, Err(FrameError::TooLarge(0xFFFF_FFFF))));
        drop(r1);
        assert_eq!(r.read, 4);
        let mut d = FrameDecoder::new();
        d.feed(&[0xFF, 0xFF, 0xFF, 0xFF]);
        assert!(matches!(d.next_frame(), Err(FrameError::TooLarge(_))));
    }

    #[test]
    fn short_output_is_error() {
        let mut bytes = 11u32.to_be_bytes().to_vec();
        bytes.push(KIND_OUTPUT);
        bytes.extend([0u8; 10]);
        let r = read_frame(&mut Cursor::new(bytes), MAX_FRAME_LEN);
        assert!(matches!(r, Err(FrameError::ShortOutput(10))));
    }

    #[test]
    fn unknown_kind_passthrough() {
        let mut bytes = Vec::new();
        encode_raw(7, &[b"zz"], &mut bytes).unwrap();
        bytes.extend(json(b"{}"));
        let mut c = Cursor::new(bytes);
        assert_eq!(
            read_frame(&mut c, MAX_FRAME_LEN).unwrap().unwrap(),
            Frame::Unknown {
                kind: 7,
                payload: b"zz".to_vec()
            }
        );
        assert_eq!(
            read_frame(&mut c, MAX_FRAME_LEN).unwrap().unwrap(),
            Frame::Json(b"{}".to_vec())
        );
    }

    #[test]
    fn clean_eof_is_none() {
        assert!(read_frame(&mut Cursor::new(Vec::new()), MAX_FRAME_LEN)
            .unwrap()
            .is_none());
    }

    #[test]
    fn eof_mid_frame_is_error() {
        let full = json(b"{\"t\":1}");
        for cut in [2, 4, 6] {
            let r = read_frame(&mut Cursor::new(full[..cut].to_vec()), MAX_FRAME_LEN);
            assert!(matches!(r, Err(FrameError::TruncatedEof)), "cut at {cut}");
        }
    }

    #[test]
    fn oversized_encode_is_rejected() {
        let big = vec![b' '; MAX_FRAME_LEN as usize];
        let mut out = Vec::new();
        assert!(matches!(
            encode_json(&big, &mut out),
            Err(FrameError::TooLarge(_))
        ));
        assert!(out.is_empty());
    }
}
