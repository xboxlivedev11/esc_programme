//! RFC 6455 server-side WebSocket codec: handshake key, frame reader (unmasking, fragment
//! reassembly, validation) and unmasked server frame writer. Std only.

use crate::crypto::{b64_encode, sha1};
use std::io::{self, Read};

pub const OP_CONT: u8 = 0;
pub const OP_TEXT: u8 = 1;
pub const OP_BINARY: u8 = 2;
pub const OP_CLOSE: u8 = 8;
pub const OP_PING: u8 = 9;
pub const OP_PONG: u8 = 10;
/// Magic GUID appended to the client key in the opening handshake (RFC 6455 section 1.3).
pub const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Payload bytes read (and buffer growth) per step, so a lying length header cannot force a huge allocation.
const READ_CHUNK: usize = 64 * 1024;

/// Computes `Sec-WebSocket-Accept` for a `Sec-WebSocket-Key` value.
pub fn accept_key(client_key: &str) -> String {
    b64_encode(&sha1(format!("{client_key}{GUID}").as_bytes()))
}

/// A complete message (or control frame) received from the client.
#[derive(Debug, PartialEq)]
pub enum Msg {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close(Option<u16>, String),
}

/// Reads client frames from a stream and reassembles fragmented messages.
///
/// Any error returned by `read` is fatal: the connection should be closed, not read again.
pub struct Reader {
    max: usize,
    /// Payload of the data message currently being assembled.
    msg: Vec<u8>,
    /// Opcode (text or binary) of the open fragmented message, if any.
    open: Option<u8>,
}

fn bad(why: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why)
}

/// XORs `data` with the 4-byte masking `key` (key index restarts at 0), eight bytes at a time.
fn unmask(data: &mut [u8], key: [u8; 4]) {
    let wide = u64::from_ne_bytes([key[0], key[1], key[2], key[3], key[0], key[1], key[2], key[3]]);
    let (words, tail) = data.as_chunks_mut::<8>();
    for w in words {
        *w = (u64::from_ne_bytes(*w) ^ wide).to_ne_bytes();
    }
    for (i, b) in tail.iter_mut().enumerate() {
        *b ^= key[i % 4];
    }
}

impl Reader {
    /// Creates a reader that rejects messages larger than `max_message` bytes.
    pub fn new(max_message: usize) -> Self {
        Reader { max: max_message, msg: Vec::new(), open: None }
    }

    /// Blocks until the next complete data message or control frame; `InvalidData` on protocol violations.
    pub fn read(&mut self, r: &mut impl Read) -> io::Result<Msg> {
        loop {
            let mut head = [0u8; 2];
            r.read_exact(&mut head)?;
            let fin = head[0] & 0x80 != 0;
            let op = head[0] & 0x0F;
            if head[0] & 0x70 != 0 {
                return Err(bad("reserved bits set"));
            }
            if !matches!(op, OP_CONT | OP_TEXT | OP_BINARY | OP_CLOSE | OP_PING | OP_PONG) {
                return Err(bad("unknown opcode"));
            }
            if head[1] & 0x80 == 0 {
                return Err(bad("client frame not masked"));
            }
            let control = op >= OP_CLOSE;
            let len7 = head[1] & 0x7F;
            if control && (!fin || len7 > 125) {
                return Err(bad("invalid control frame"));
            }
            match (op, self.open) {
                (OP_CONT, None) => return Err(bad("continuation without a message")),
                (OP_TEXT | OP_BINARY, Some(_)) => return Err(bad("new message inside a fragmented message")),
                _ => {}
            }
            let len = match len7 {
                126 => {
                    let mut b = [0u8; 2];
                    r.read_exact(&mut b)?;
                    u64::from(u16::from_be_bytes(b))
                }
                127 => {
                    let mut b = [0u8; 8];
                    r.read_exact(&mut b)?;
                    let n = u64::from_be_bytes(b);
                    if n >> 63 != 0 {
                        return Err(bad("64-bit length has its high bit set"));
                    }
                    n
                }
                n => u64::from(n),
            };
            if !control && len > (self.max - self.msg.len()) as u64 {
                return Err(bad("message too large"));
            }
            let mut key = [0u8; 4];
            r.read_exact(&mut key)?;

            if control {
                let mut buf = [0u8; 125];
                let payload = &mut buf[..len as usize];
                r.read_exact(payload)?;
                unmask(payload, key);
                return control_msg(op, payload);
            }

            let (start, len) = (self.msg.len(), len as usize);
            let mut done = 0;
            while done < len {
                let n = (len - done).min(READ_CHUNK);
                self.msg.resize(start + done + n, 0);
                r.read_exact(&mut self.msg[start + done..])?;
                done += n;
            }
            unmask(&mut self.msg[start..], key);

            if op != OP_CONT {
                self.open = Some(op);
            }
            if fin {
                let data = std::mem::take(&mut self.msg);
                return match self.open.take() {
                    Some(OP_TEXT) => String::from_utf8(data)
                        .map(Msg::Text)
                        .map_err(|_| bad("invalid UTF-8 in text message")),
                    _ => Ok(Msg::Binary(data)),
                };
            }
        }
    }
}

/// Decodes the (already unmasked) payload of a control frame.
fn control_msg(op: u8, p: &[u8]) -> io::Result<Msg> {
    Ok(match op {
        OP_PING => Msg::Ping(p.to_vec()),
        OP_PONG => Msg::Pong(p.to_vec()),
        _ => match p {
            [] => Msg::Close(None, String::new()),
            [_] => return Err(bad("close payload of length 1")),
            [a, b, reason @ ..] => {
                let reason = std::str::from_utf8(reason).map_err(|_| bad("invalid UTF-8 in close reason"))?;
                Msg::Close(Some(u16::from_be_bytes([*a, *b])), reason.to_owned())
            }
        },
    })
}

/// Builds one unmasked FIN frame (header and payload in a single buffer).
pub fn frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let n = payload.len();
    let mut out = Vec::with_capacity(10 + n);
    out.push(0x80 | (opcode & 0x0F));
    if n < 126 {
        out.push(n as u8);
    } else if n <= usize::from(u16::MAX) {
        out.push(126);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        out.push(127);
        out.extend_from_slice(&(n as u64).to_be_bytes());
    }
    out.extend_from_slice(payload);
    out
}

/// Builds a close frame with a status code and reason (reason cut at a char boundary to fit 125 bytes).
pub fn close_frame(code: u16, reason: &str) -> Vec<u8> {
    let mut end = reason.len().min(123);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    let mut payload = Vec::with_capacity(2 + end);
    payload.extend_from_slice(&code.to_be_bytes());
    payload.extend_from_slice(&reason.as_bytes()[..end]);
    frame(OP_CLOSE, &payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 4] = [0x37, 0xfa, 0x21, 0x3d];

    /// Builds a masked client frame, choosing the length encoding from the payload size.
    fn masked(fin: bool, op: u8, payload: &[u8], key: [u8; 4]) -> Vec<u8> {
        let mut v = vec![(u8::from(fin) << 7) | op];
        match payload.len() {
            n if n < 126 => v.push(0x80 | n as u8),
            n if n <= 65535 => {
                v.push(0x80 | 126);
                v.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                v.push(0x80 | 127);
                v.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        v.extend_from_slice(&key);
        v.extend(payload.iter().enumerate().map(|(i, b)| b ^ key[i % 4]));
        v
    }

    fn read_one(max: usize, bytes: &[u8]) -> io::Result<Msg> {
        Reader::new(max).read(&mut &bytes[..])
    }

    fn assert_invalid(bytes: &[u8], what: &str) {
        match read_one(1 << 20, bytes) {
            Err(e) => assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{what}: {e}"),
            Ok(m) => panic!("{what}: accepted as {m:?}"),
        }
    }

    fn pattern(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 31 % 251) as u8).collect()
    }

    /// Yields one byte per `read` call.
    struct Trickle<'a>(&'a [u8]);
    impl Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.0.is_empty() || buf.is_empty() {
                return Ok(0);
            }
            buf[0] = self.0[0];
            self.0 = &self.0[1..];
            Ok(1)
        }
    }

    #[test]
    fn accept_key_rfc6455() {
        assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn rfc_masked_hello() {
        let bytes = [0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58];
        assert_eq!(read_one(100, &bytes).unwrap(), Msg::Text("Hello".into()));
        assert_eq!(Reader::new(100).read(&mut Trickle(&bytes)).unwrap(), Msg::Text("Hello".into()));
    }

    #[test]
    fn unmask_matches_naive() {
        for len in 0..80 {
            let plain = pattern(len);
            let mut fast = plain.clone();
            unmask(&mut fast, [1, 2, 3, 0xff]);
            let naive: Vec<u8> = plain.iter().enumerate().map(|(i, b)| b ^ [1, 2, 3, 0xff][i % 4]).collect();
            assert_eq!(fast, naive, "len {len}");
        }
    }

    #[test]
    fn fragments_with_interleaved_ping() {
        let mut s = masked(false, OP_TEXT, b"Hel", KEY);
        s.extend(masked(true, OP_PING, b"pp", [9, 8, 7, 6]));
        s.extend(masked(false, OP_CONT, b"lo, ", [1, 2, 3, 4]));
        s.extend(masked(true, OP_CONT, "wörld".as_bytes(), [5, 5, 5, 5]));
        s.extend(masked(true, OP_BINARY, &[1, 2, 3], KEY));
        let mut rd = Reader::new(100);
        let mut cur = &s[..];
        assert_eq!(rd.read(&mut cur).unwrap(), Msg::Ping(b"pp".to_vec()));
        assert_eq!(rd.read(&mut cur).unwrap(), Msg::Text("Hello, wörld".into()));
        assert_eq!(rd.read(&mut cur).unwrap(), Msg::Binary(vec![1, 2, 3]));
        assert_eq!(rd.read(&mut cur).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn fragmented_binary_and_pong_and_empty_continuation() {
        let mut s = masked(false, OP_BINARY, &[1, 2], KEY);
        s.extend(masked(true, OP_PONG, b"", KEY));
        s.extend(masked(false, OP_CONT, b"", KEY));
        s.extend(masked(true, OP_CONT, &[3], KEY));
        let mut rd = Reader::new(100);
        let mut cur = &s[..];
        assert_eq!(rd.read(&mut cur).unwrap(), Msg::Pong(vec![]));
        assert_eq!(rd.read(&mut cur).unwrap(), Msg::Binary(vec![1, 2, 3]));
    }

    #[test]
    fn utf8_char_split_across_fragments_is_fine() {
        let e = "é".as_bytes(); // 2 bytes
        let mut s = masked(false, OP_TEXT, &e[..1], KEY);
        s.extend(masked(true, OP_CONT, &e[1..], KEY));
        assert_eq!(read_one(10, &s).unwrap(), Msg::Text("é".into()));
    }

    #[test]
    fn length_encodings_round_trip() {
        for len in [0, 1, 124, 125, 126, 127, 65535, 65536, 65537, 1 << 20] {
            let payload = pattern(len);
            let key = [0xde, 0xad, 0xbe, 0xef];
            let bytes = masked(true, OP_BINARY, &payload, key);
            let want = if len < 126 { 6 } else if len <= 65535 { 8 } else { 14 };
            assert_eq!(bytes.len(), len + want, "wire size for {len}");
            assert_eq!(read_one(1 << 20, &bytes).unwrap(), Msg::Binary(payload.clone()), "len {len}");
            let text = vec![b'x'; len];
            assert_eq!(
                read_one(1 << 20, &masked(true, OP_TEXT, &text, key)).unwrap(),
                Msg::Text("x".repeat(len))
            );
        }
        // Larger than one internal read chunk, delivered a byte at a time.
        let payload = pattern(READ_CHUNK + 5);
        let bytes = masked(true, OP_BINARY, &payload, KEY);
        assert_eq!(Reader::new(1 << 20).read(&mut Trickle(&bytes)).unwrap(), Msg::Binary(payload));
    }

    #[test]
    fn frame_headers() {
        assert_eq!(frame(OP_TEXT, b""), [0x81, 0]);
        assert_eq!(frame(OP_PONG, b"ab"), [0x8A, 2, b'a', b'b']);
        for (len, header) in [
            (125usize, vec![0x82, 125]),
            (126, vec![0x82, 126, 0, 126]),
            (65535, vec![0x82, 126, 0xFF, 0xFF]),
            (65536, vec![0x82, 127, 0, 0, 0, 0, 0, 1, 0, 0]),
            (1 << 20, vec![0x82, 127, 0, 0, 0, 0, 0, 0x10, 0, 0]),
        ] {
            let payload = pattern(len);
            let f = frame(OP_BINARY, &payload);
            assert_eq!(&f[..header.len()], &header[..], "header for {len}");
            assert_eq!(&f[header.len()..], &payload[..]);
        }
    }

    #[test]
    fn server_frames_are_rejected_as_unmasked() {
        assert_invalid(&frame(OP_TEXT, b"hi"), "unmasked");
        assert_invalid(&[0x81, 0x00], "unmasked empty");
        assert_invalid(&frame(OP_PING, b""), "unmasked ping");
    }

    #[test]
    fn protocol_violations() {
        for rsv in [0x40, 0x20, 0x10] {
            assert_invalid(&[0x81 | rsv, 0x80, 0, 0, 0, 0], "rsv");
        }
        for op in [3, 4, 5, 6, 7, 0x0B, 0x0C, 0x0F] {
            assert_invalid(&[0x80 | op, 0x80, 0, 0, 0, 0], "unknown opcode");
        }
        assert_invalid(&masked(true, OP_PING, &[0; 126], KEY), "control > 125");
        assert_invalid(&masked(true, OP_CLOSE, &[0; 200], KEY), "close > 125");
        assert_invalid(&[0x89, 0xFF, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0], "control with 64-bit length");
        assert_invalid(&masked(false, OP_PING, b"x", KEY), "fragmented ping");
        assert_invalid(&masked(false, OP_CLOSE, b"", KEY), "fragmented close");
        assert_invalid(&masked(true, OP_CONT, b"x", KEY), "orphan continuation");
        let mut s = masked(false, OP_TEXT, b"a", KEY);
        s.extend(masked(true, OP_CONT, b"b", KEY));
        s.extend(masked(true, OP_CONT, b"c", KEY));
        let mut rd = Reader::new(100);
        let mut cur = &s[..];
        rd.read(&mut cur).unwrap();
        assert_eq!(rd.read(&mut cur).unwrap_err().kind(), io::ErrorKind::InvalidData, "continuation after FIN");
        for op2 in [OP_TEXT, OP_BINARY] {
            let mut s = masked(false, OP_BINARY, b"a", KEY);
            s.extend(masked(true, op2, b"b", KEY));
            assert_invalid(&s, "new message in fragmented message");
        }
        let mut hi = vec![0x82, 0xFF, 0x80, 0, 0, 0, 0, 0, 0, 0];
        hi.extend_from_slice(&KEY);
        assert_invalid(&hi, "64-bit length high bit");
        hi[2] = 0xFF;
        assert_invalid(&hi, "64-bit length all ones");
    }

    #[test]
    fn size_limit_is_checked_before_reading_payload() {
        // Header only, no payload bytes: must fail with InvalidData, not UnexpectedEof.
        let mut big = vec![0x82, 0xFF];
        big.extend_from_slice(&(1u64 << 40).to_be_bytes());
        big.extend_from_slice(&KEY);
        assert_eq!(read_one(1 << 20, &big).unwrap_err().kind(), io::ErrorKind::InvalidData);
        let mut at_limit = vec![0x82, 0x80 | 11];
        at_limit.extend_from_slice(&KEY);
        assert_eq!(read_one(10, &at_limit).unwrap_err().kind(), io::ErrorKind::InvalidData);

        assert!(read_one(10, &masked(true, OP_BINARY, &[0; 10], KEY)).is_ok());
        assert!(read_one(10, &masked(true, OP_BINARY, &[0; 11], KEY)).is_err());
        assert!(read_one(0, &masked(true, OP_BINARY, &[], KEY)).is_ok());

        // Limit applies to the sum of fragments.
        let frags = |a: usize, b: usize| {
            let mut s = masked(false, OP_BINARY, &vec![1; a], KEY);
            s.extend(masked(true, OP_CONT, &vec![2; b], KEY));
            s
        };
        assert!(read_one(10, &frags(5, 5)).is_ok());
        assert_eq!(read_one(10, &frags(6, 5)).unwrap_err().kind(), io::ErrorKind::InvalidData);
        // Second fragment's declared length alone busts the limit; payload not present.
        let mut s = masked(false, OP_BINARY, &[1; 6], KEY);
        s.extend_from_slice(&[0x80, 0x80 | 5]);
        s.extend_from_slice(&KEY);
        assert_eq!(read_one(10, &s).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn invalid_utf8() {
        assert_invalid(&masked(true, OP_TEXT, &[0xC3, 0x28], KEY), "bad utf-8 text");
        assert_invalid(&masked(true, OP_TEXT, &[0xFF], KEY), "0xFF in text");
        let mut s = masked(false, OP_TEXT, &"é".as_bytes()[..1], KEY);
        s.extend(masked(true, OP_CONT, b"x", KEY));
        assert_invalid(&s, "bad utf-8 across fragments");
        // Binary frames are not validated.
        assert!(read_one(10, &masked(true, OP_BINARY, &[0xFF, 0xFE], KEY)).is_ok());
    }

    #[test]
    fn close_parsing() {
        assert_eq!(read_one(10, &masked(true, OP_CLOSE, b"", KEY)).unwrap(), Msg::Close(None, String::new()));
        assert_eq!(read_one(10, &masked(true, OP_CLOSE, &[0x03, 0xE8], KEY)).unwrap(), Msg::Close(Some(1000), String::new()));
        let mut p = 1001u16.to_be_bytes().to_vec();
        p.extend_from_slice("bye é".as_bytes());
        assert_eq!(read_one(10, &masked(true, OP_CLOSE, &p, KEY)).unwrap(), Msg::Close(Some(1001), "bye é".into()));
        assert_invalid(&masked(true, OP_CLOSE, &[0x03], KEY), "close length 1");
        assert_invalid(&masked(true, OP_CLOSE, &[0x03, 0xE8, 0xC3, 0x28], KEY), "close reason utf-8");
        // Close with the maximum 125-byte payload.
        assert!(read_one(10, &masked(true, OP_CLOSE, &[b'a'; 125], KEY)).is_ok());
    }

    #[test]
    fn close_frame_round_trip() {
        let f = close_frame(1000, "normal");
        assert_eq!(f[0], 0x88);
        assert_eq!(f[1] as usize, f.len() - 2);
        assert_eq!(&f[2..4], &[0x03, 0xE8]);
        assert_eq!(&f[4..], b"normal");
        // Re-mask the payload as a client would and parse it back.
        let back = read_one(10, &masked(true, OP_CLOSE, &f[2..], KEY)).unwrap();
        assert_eq!(back, Msg::Close(Some(1000), "normal".into()));
        assert_eq!(close_frame(1009, ""), [0x88, 2, 0x03, 0xF1]);

        // Long reasons are truncated to a char boundary; payload stays <= 125.
        for reason in ["x".repeat(500), "é".repeat(200), "€".repeat(100), "😀".repeat(50)] {
            let f = close_frame(4000, &reason);
            assert_eq!(f[0], 0x88);
            assert!(f[1] <= 125 && f[1] as usize == f.len() - 2);
            let Msg::Close(Some(4000), r) = read_one(10, &masked(true, OP_CLOSE, &f[2..], KEY)).unwrap() else {
                panic!("not a close");
            };
            assert!(reason.starts_with(&r) && r.len() >= 120, "kept {} bytes", r.len());
        }
    }

    #[test]
    fn eof_passes_through() {
        assert_eq!(read_one(10, &[]).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(read_one(10, &[0x81]).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        let full = masked(true, OP_TEXT, b"hello", KEY);
        for cut in 1..full.len() {
            assert_eq!(read_one(10, &full[..cut]).unwrap_err().kind(), io::ErrorKind::UnexpectedEof, "cut {cut}");
        }
    }

    #[test]
    fn many_messages_on_one_stream() {
        let mut s = Vec::new();
        for i in 0..50u8 {
            s.extend(masked(true, if i % 2 == 0 { OP_TEXT } else { OP_BINARY }, &vec![b'a' + i % 26; i as usize], [i, 1, 2, 3]));
        }
        let mut rd = Reader::new(100);
        let mut cur = &s[..];
        for i in 0..50u8 {
            let want = if i % 2 == 0 {
                Msg::Text(char::from(b'a' + i % 26).to_string().repeat(i as usize))
            } else {
                Msg::Binary(vec![b'a' + i % 26; i as usize])
            };
            assert_eq!(rd.read(&mut cur).unwrap(), want);
        }
    }
}
