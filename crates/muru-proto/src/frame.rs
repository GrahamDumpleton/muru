//! Framing for the framed worker link: one request per connection.
//!
//! Every frame is a 4 byte little-endian payload length, a type byte, a
//! flags byte and the payload. Structured payloads are protobuf messages
//! from the worker protocol; BODY payloads are raw bytes.

use std::io;

use bytes::{BufMut, Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Size of a frame header.
pub const HEADER_LEN: usize = 6;

/// Largest payload accepted, to bound memory on a corrupt stream.
pub const MAX_PAYLOAD: usize = 16 * 1024 * 1024;

/// Frame types.
pub mod kind {
    /// Protocol version and capabilities, once per connection in each direction.
    pub const HELLO: u8 = 1;
    /// A request head (protobuf `RequestHead`), edge to worker.
    pub const REQUEST: u8 = 2;
    /// One chunk of request or response body, raw bytes.
    pub const BODY: u8 = 3;
    /// A response head (protobuf `ResponseHead`), worker to edge.
    pub const RESPONSE: u8 = 4;
    /// End of the body in that direction (protobuf `End`).
    pub const END: u8 = 5;
    /// The edge has no more use for the request: the client has gone, or
    /// the response ended first (protobuf `Abort`), edge to worker.
    pub const ABORT: u8 = 6;
    /// The application has started reading the request body, so the edge
    /// may tell a client waiting on Expect: 100-continue to send it
    /// (empty payload), worker to edge.
    pub const CONTINUE: u8 = 7;
    // Kinds 8 and up are reserved for MESSAGE, CLOSE, INFORMATIONAL, FILE
    // and DETACH (design specification section 5.2).
}

/// Frame flags.
pub mod flag {
    /// Set on a REQUEST, RESPONSE or BODY frame that is the last frame of its
    /// direction, so no END frame follows.
    pub const END: u8 = 0x01;
}

/// A decoded frame.
#[derive(Debug, Clone)]
pub struct Frame {
    /// One of the `kind` constants.
    pub kind: u8,
    /// A set of `flag` bits.
    pub flags: u8,
    /// The payload, raw bytes or an encoded protobuf message.
    pub payload: Bytes,
}

impl Frame {
    /// Whether the END flag is set.
    pub fn is_end(&self) -> bool {
        self.flags & flag::END != 0
    }
}

/// Append a frame header to a buffer.
pub fn put_header(buffer: &mut BytesMut, kind: u8, flags: u8, payload_len: usize) {
    buffer.put_u32_le(payload_len as u32);
    buffer.put_u8(kind);
    buffer.put_u8(flags);
}

/// Encode a frame into one contiguous buffer.
pub fn encode(kind: u8, flags: u8, payload: &[u8]) -> BytesMut {
    let mut buffer = BytesMut::with_capacity(HEADER_LEN + payload.len());
    put_header(&mut buffer, kind, flags, payload.len());
    buffer.put_slice(payload);
    buffer
}

/// Write one frame. Small payloads go out in one write with the header;
/// large ones are written after it, without being copied.
pub async fn write<W: AsyncWrite + Unpin>(
    writer: &mut W,
    kind: u8,
    flags: u8,
    payload: Bytes,
) -> io::Result<()> {
    const COPY_LIMIT: usize = 16 * 1024;

    if payload.len() <= COPY_LIMIT {
        let buffer = encode(kind, flags, &payload);
        writer.write_all(&buffer).await
    } else {
        let mut header = BytesMut::with_capacity(HEADER_LEN);
        put_header(&mut header, kind, flags, payload.len());
        writer.write_all(&header).await?;
        writer.write_all(&payload).await
    }
}

/// Read one frame. Returns `None` on a clean end of stream at a frame boundary.
pub async fn read<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Option<Frame>> {
    let mut header = [0u8; HEADER_LEN];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    if len > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame payload of {len} bytes exceeds the limit"),
        ));
    }
    let mut payload = BytesMut::zeroed(len);
    reader.read_exact(&mut payload).await?;
    Ok(Some(Frame {
        kind: header[4],
        flags: header[5],
        payload: payload.freeze(),
    }))
}
