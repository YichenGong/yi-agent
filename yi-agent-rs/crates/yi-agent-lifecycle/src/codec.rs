//! Bounded framing for the control channel.
//!
//! Wire shape of one frame:
//!
//! ```text
//! +--------------------+---------------------------------------------+
//! | u32 big-endian len | JSON body of exactly `len` bytes            |
//! +--------------------+---------------------------------------------+
//! ```
//!
//! `len` counts the JSON body only. [`MAX_CONTROL_FRAME`] caps the *whole* frame
//! including the 4-byte header, so the largest legal body is
//! `MAX_CONTROL_FRAME - 4`. The header is validated before a single body byte is
//! allocated: a peer cannot make the reader reserve 4 GiB by lying about a length.
//!
//! The protocol version travels *inside* the JSON envelope rather than in a
//! separate field of the header, so one decode pass validates both envelope and
//! payload, and a future v2 can change the envelope shape without colliding with
//! v1 readers. This crate deliberately does not reuse the business IPC's
//! `IpcResponse` deserializer: the control channel must be able to answer an
//! incompatible business protocol, which is impossible if the two share a codec.

use std::io;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::CONTROL_VERSION;
use crate::MAX_CONTROL_FRAME;

/// Length of the big-endian body-length header.
pub const LENGTH_HEADER_BYTES: usize = 4;

/// Largest body a legal frame can carry.
pub const MAX_CONTROL_BODY: usize = MAX_CONTROL_FRAME - LENGTH_HEADER_BYTES;

#[derive(Serialize)]
struct EnvelopeRef<'a, T> {
    control_version: u16,
    payload: &'a T,
}

#[derive(Deserialize)]
struct Envelope<T> {
    control_version: u16,
    payload: T,
}

fn invalid(kind: io::ErrorKind, message: &str) -> io::Error {
    io::Error::new(kind, message.to_string())
}

/// Serialize one request or response into a complete, length-prefixed frame.
///
/// Refuses to produce a frame that the peer's decoder would reject: an over-long
/// payload is a programming error on this side, reported to the caller rather
/// than written out as a header that lies.
pub fn encode<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    let envelope = EnvelopeRef {
        control_version: CONTROL_VERSION,
        payload: value,
    };
    let body = serde_json::to_vec(&envelope)
        .map_err(|error| invalid(io::ErrorKind::InvalidInput, &error.to_string()))?;
    if body.len() > MAX_CONTROL_BODY {
        return Err(invalid(
            io::ErrorKind::InvalidInput,
            "control frame body exceeds MAX_CONTROL_FRAME",
        ));
    }
    let mut frame = Vec::with_capacity(LENGTH_HEADER_BYTES + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// Decode one complete frame, rejecting it before allocating an over-long body.
pub fn decode<T: DeserializeOwned>(frame: &[u8]) -> io::Result<T> {
    let header: [u8; LENGTH_HEADER_BYTES] = frame
        .get(..LENGTH_HEADER_BYTES)
        .ok_or_else(|| {
            invalid(
                io::ErrorKind::UnexpectedEof,
                "control frame header is truncated",
            )
        })?
        .try_into()
        .map_err(|_| {
            invalid(
                io::ErrorKind::UnexpectedEof,
                "control frame header is truncated",
            )
        })?;
    let body_len = u32::from_be_bytes(header) as usize;
    // Validate the length against the ceiling *and* against what actually arrived
    // before touching the body.
    if body_len + LENGTH_HEADER_BYTES > MAX_CONTROL_FRAME {
        return Err(invalid(
            io::ErrorKind::InvalidData,
            "control frame exceeds MAX_CONTROL_FRAME",
        ));
    }
    let body = frame
        .get(LENGTH_HEADER_BYTES..LENGTH_HEADER_BYTES + body_len)
        .ok_or_else(|| {
            invalid(
                io::ErrorKind::UnexpectedEof,
                "control frame body is shorter than its header",
            )
        })?;
    // from_utf8 first, so a binary frame is reported as invalid data rather than
    // as a confusing JSON syntax error deep inside serde.
    let text = std::str::from_utf8(body).map_err(|_| {
        invalid(
            io::ErrorKind::InvalidData,
            "control frame body is not UTF-8",
        )
    })?;
    let envelope: Envelope<T> = serde_json::from_str(text)
        .map_err(|error| invalid(io::ErrorKind::InvalidData, &error.to_string()))?;
    if envelope.control_version != CONTROL_VERSION {
        return Err(invalid(
            io::ErrorKind::InvalidData,
            "unsupported control protocol version",
        ));
    }
    Ok(envelope.payload)
}

/// Read exactly one frame from a stream, refusing an over-long declaration before
/// reading the body.
pub fn read_frame<R: io::Read>(reader: &mut R) -> io::Result<Vec<u8>> {
    let mut header = [0u8; LENGTH_HEADER_BYTES];
    reader.read_exact(&mut header)?;
    let body_len = u32::from_be_bytes(header) as usize;
    if body_len + LENGTH_HEADER_BYTES > MAX_CONTROL_FRAME {
        return Err(invalid(
            io::ErrorKind::InvalidData,
            "control frame exceeds MAX_CONTROL_FRAME",
        ));
    }
    let mut body = vec![0u8; body_len];
    reader.read_exact(&mut body)?;
    let mut frame = Vec::with_capacity(LENGTH_HEADER_BYTES + body_len);
    frame.extend_from_slice(&header);
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// Serialize and write exactly one frame.
pub fn write_frame<W: io::Write, T: Serialize>(writer: &mut W, value: &T) -> io::Result<()> {
    let frame = encode(value)?;
    writer.write_all(&frame)?;
    writer.flush()
}

/// Build the exact JSON envelope a frame would carry for `payload`.
///
/// Exposed so the boundary tests can assert the off-by-one behaviour at
/// `MAX_CONTROL_FRAME` against the real envelope shape instead of duplicating the
/// format string in the test (which is how the first version of that test drifted).
pub fn test_envelope<T: Serialize>(payload: &T) -> String {
    serde_json::to_string(&EnvelopeRef {
        control_version: CONTROL_VERSION,
        payload,
    })
    .expect("envelope of a serializable payload always serializes")
}
