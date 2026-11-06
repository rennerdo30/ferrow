//! Protocol tests: exact round trips, frame limits, and a
//! deterministic fuzz-style sweep proving the parser never
//! panics on arbitrary or mutated input.

use ferrow_ipc::protocol::{
    MAX_FRAME_BYTES, MAX_PATH_BYTES, MAX_TEXT_BYTES, Message, PROTOCOL_VERSION, ProtocolError,
    Request, Response, Sort, decode_payload, encode_frame, read_message, write_message,
};
use ferrow_ipc::{Envelope, ErrorCode};

fn round_trip(id: u32, message: Message) {
    let frame = encode_frame(id, &message).unwrap();
    assert_eq!(
        u32::from_le_bytes(frame[0..4].try_into().unwrap()) as usize,
        frame.len() - 4
    );
    let decoded = decode_payload(&frame[4..]).unwrap();
    assert_eq!(
        decoded,
        Envelope {
            version: PROTOCOL_VERSION as u8,
            id,
            message
        }
    );
    // And through the stream helpers.
    let mut buf = Vec::new();
    write_message(&mut buf, id, &decoded.message).unwrap();
    let mut cursor = std::io::Cursor::new(buf);
    assert_eq!(read_message(&mut cursor).unwrap(), Some(decoded));
    assert_eq!(
        read_message(&mut cursor).unwrap(),
        None,
        "clean EOF at a frame boundary"
    );
}

#[test]
fn every_message_round_trips() {
    round_trip(
        0,
        Message::Request(Request::Hello {
            version: PROTOCOL_VERSION,
        }),
    );
    round_trip(
        7,
        Message::Request(Request::Query {
            text: "report 2024 äöü".into(),
            limit: 25,
            sort: Sort::Name,
            flags: 0,
        }),
    );
    round_trip(8, Message::Request(Request::Count { text: "x".into() }));
    round_trip(9, Message::Request(Request::Status));
    round_trip(10, Message::Request(Request::Cancel { target_id: 7 }));
    round_trip(
        0,
        Message::Response(Response::HelloOk {
            version: PROTOCOL_VERSION,
        }),
    );
    round_trip(
        7,
        Message::Response(Response::Results {
            paths: vec![b"/a/b.txt".to_vec(), vec![0xff, 0x00, b'x']],
            more: true,
        }),
    );
    round_trip(
        7,
        Message::Response(Response::Results {
            paths: vec![],
            more: false,
        }),
    );
    round_trip(8, Message::Response(Response::Count { n: u64::MAX }));
    round_trip(
        9,
        Message::Response(Response::Status {
            entries: 123_456,
            mode: "walk".into(),
            report: "walk: 1 entries".into(),
        }),
    );
    round_trip(
        9,
        Message::Response(Response::Error {
            code: ErrorCode::VersionMismatch,
            msg: "nope".into(),
        }),
    );
}

#[test]
fn frame_limits_are_enforced_on_encode() {
    let big_text = "x".repeat(MAX_TEXT_BYTES + 1);
    assert_eq!(
        encode_frame(1, &Message::Request(Request::Count { text: big_text })),
        Err(ProtocolError::TooLarge)
    );
    let big_path = vec![b'x'; MAX_PATH_BYTES + 1];
    assert_eq!(
        encode_frame(
            1,
            &Message::Response(Response::Results {
                paths: vec![big_path],
                more: false
            })
        ),
        Err(ProtocolError::TooLarge)
    );
    // Individually legal paths that together exceed one frame.
    let paths = vec![vec![b'x'; MAX_PATH_BYTES]; MAX_FRAME_BYTES / MAX_PATH_BYTES + 1];
    assert_eq!(
        encode_frame(
            1,
            &Message::Response(Response::Results { paths, more: false })
        ),
        Err(ProtocolError::TooLarge)
    );
}

#[test]
fn over_limit_length_prefix_is_rejected_before_allocating() {
    for len in [0u32, (MAX_FRAME_BYTES as u32) + 1, u32::MAX] {
        let bytes = len.to_le_bytes();
        let mut cursor = std::io::Cursor::new(bytes.to_vec());
        let err = read_message(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "len {len}");
    }
    // A payload longer than the frame cap is rejected outright.
    assert_eq!(
        decode_payload(&vec![0u8; MAX_FRAME_BYTES + 1]),
        Err(ProtocolError::TooLarge)
    );
}

#[test]
fn malformed_payloads_are_typed_errors() {
    assert_eq!(decode_payload(&[]), Err(ProtocolError::Truncated));
    assert_eq!(
        decode_payload(&[1, 99, 0, 0, 0, 0]),
        Err(ProtocolError::UnknownKind(99))
    );
    // Query with a huge text length.
    let mut payload = vec![1, 2, 0, 0, 0, 0];
    payload.extend_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(decode_payload(&payload), Err(ProtocolError::TooLarge));
    // Query with invalid UTF-8 text.
    let mut payload = vec![1, 2, 0, 0, 0, 0];
    payload.extend_from_slice(&1u32.to_le_bytes());
    payload.push(0xff);
    payload.extend_from_slice(&[0; 9]);
    assert_eq!(decode_payload(&payload), Err(ProtocolError::InvalidUtf8));
    // Trailing byte after a complete Status request.
    let mut frame = encode_frame(3, &Message::Request(Request::Status)).unwrap();
    frame.push(0);
    assert_eq!(
        decode_payload(&frame[4..]),
        Err(ProtocolError::TrailingBytes)
    );
    // Truncated payload on the wire.
    let frame = encode_frame(
        3,
        &Message::Request(Request::Count {
            text: "hello".into(),
        }),
    )
    .unwrap();
    let mut cursor = std::io::Cursor::new(frame[..frame.len() - 2].to_vec());
    assert_eq!(
        read_message(&mut cursor).unwrap_err().kind(),
        std::io::ErrorKind::UnexpectedEof
    );
    // Truncated length prefix on the wire (2 of 4 bytes).
    let mut cursor = std::io::Cursor::new(vec![5u8, 0]);
    assert_eq!(
        read_message(&mut cursor).unwrap_err().kind(),
        std::io::ErrorKind::UnexpectedEof
    );
}

/// Tiny deterministic PRNG (xorshift64*) for reproducible sweeps.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

#[test]
fn parser_never_panics_on_garbage_or_mutations() {
    let seeds: Vec<Vec<u8>> = [
        Message::Request(Request::Query {
            text: "seed query".into(),
            limit: 3,
            sort: Sort::Name,
            flags: 0,
        }),
        Message::Response(Response::Results {
            paths: vec![b"a".to_vec(), b"bb".to_vec()],
            more: true,
        }),
        Message::Response(Response::Status {
            entries: 1,
            mode: "m".into(),
            report: "r".into(),
        }),
    ]
    .iter()
    .map(|m| encode_frame(5, m).unwrap()[4..].to_vec())
    .collect();

    let mut rng = Rng(0x1234_5678_9abc_def1);
    // Pure garbage.
    for _ in 0..5_000 {
        let len = (rng.next() % 64) as usize;
        let bytes: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        let _ = decode_payload(&bytes);
    }
    // Mutations and truncations of valid payloads.
    for seed in &seeds {
        for cut in 0..seed.len() {
            let _ = decode_payload(&seed[..cut]);
        }
        for _ in 0..3_000 {
            let mut mutated = seed.clone();
            for _ in 0..(rng.next() % 4 + 1) {
                let at = (rng.next() as usize) % mutated.len();
                mutated[at] = rng.next() as u8;
            }
            if let Ok(envelope) = decode_payload(&mutated) {
                // Anything the parser accepts must re-encode and
                // decode to itself (parser/encoder agreement).
                let frame = encode_frame(envelope.id, &envelope.message).unwrap();
                assert_eq!(
                    decode_payload(&frame[4..]).unwrap().message,
                    envelope.message
                );
            }
        }
    }
}
