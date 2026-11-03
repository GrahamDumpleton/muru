//! Framing for the framed worker link: one request per connection.
//!
//! Every frame is a 4 byte little-endian payload length, a type byte, a
//! flags byte and the payload. Structured payloads are protobuf messages
//! from the worker protocol; BODY payloads are raw bytes.

use std::io;

use bytes::{BufMut, Bytes, BytesMut};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt,
};

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
    /// One whole WebSocket message, raw bytes, in either direction; the
    /// TEXT flag says which kind (section 5.6). On a gRPC call it is one
    /// whole gRPC message (section 6.13).
    pub const MESSAGE: u8 = 8;
    /// Closes a WebSocket with a code and reason (protobuf `Close`), in
    /// either direction; each side sends exactly one (section 5.6).
    pub const CLOSE: u8 = 9;
    /// The response body, or the next piece of it, is a range of the file
    /// whose descriptor is attached to the frame (protobuf `FileHint`),
    /// worker to edge (section 5.7). The END flag ends the body with it.
    pub const FILE: u8 = 11;
    /// The request was handed to the other side of the worker (protobuf
    /// `Detach`), worker to edge, before any of the response (section
    /// 6.14).
    pub const DETACH: u8 = 12;
    // Kind 10 is reserved for INFORMATIONAL (design specification
    // section 5.2).
}

/// Frame flags.
pub mod flag {
    /// Set on a REQUEST, RESPONSE or BODY frame that is the last frame of its
    /// direction, so no END frame follows.
    pub const END: u8 = 0x01;
    /// Set on a MESSAGE frame that carries a text message rather than a
    /// binary one.
    pub const TEXT: u8 = 0x02;
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

/// A frame's header: its type, its flags and the length of the payload
/// that follows.
#[derive(Debug, Clone, Copy)]
pub struct Header {
    /// One of the `kind` constants.
    pub kind: u8,
    /// A set of `flag` bits.
    pub flags: u8,
    /// The payload's length.
    pub len: usize,
}

impl Header {
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
    let Some(header) = read_header(reader).await? else {
        return Ok(None);
    };

    read_payload(reader, header).await.map(Some)
}

/// Read one frame's header, leaving its payload to be read. Returns `None`
/// on a clean end of stream at a frame boundary.
pub async fn read_header<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Option<Header>> {
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

    Ok(Some(Header {
        kind: header[4],
        flags: header[5],
        len,
    }))
}

/// Read the whole payload of the frame whose header was just read.
pub async fn read_payload<R: AsyncRead + Unpin>(
    reader: &mut R,
    header: Header,
) -> io::Result<Frame> {
    // The payload is read into memory that is not zeroed first, which for
    // a large BODY frame was a second pass over every byte.

    let mut payload = BytesMut::with_capacity(header.len);

    while payload.len() < header.len {
        let wanted = header.len - payload.len();

        if reader.read_buf(&mut (&mut payload).limit(wanted)).await? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
    }

    Ok(Frame {
        kind: header.kind,
        flags: header.flags,
        payload: payload.freeze(),
    })
}

/// Read what has arrived of a payload being read in pieces, at least one
/// byte and at most `most`.
pub async fn read_piece<R: AsyncRead + Unpin>(reader: &mut R, most: usize) -> io::Result<Bytes> {
    let mut piece = BytesMut::with_capacity(most);

    if reader.read_buf(&mut (&mut piece).limit(most)).await? == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }

    Ok(piece.freeze())
}

/// Read and discard `len` bytes of a payload.
pub async fn skip<R: AsyncBufRead + Unpin>(reader: &mut R, mut len: usize) -> io::Result<()> {
    while len > 0 {
        let available = reader.fill_buf().await?;

        if available.is_empty() {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }

        let used = available.len().min(len);
        reader.consume(used);
        len -= used;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use tokio::io::BufReader;
    use tokio::net::UnixStream;

    use super::*;
    use crate::descriptors::FdReader;

    #[tokio::test]
    async fn a_payload_read_whole_in_pieces_or_skipped_arrives_intact_and_a_short_one_fails() {
        let (left, right) = UnixStream::pair().unwrap();
        let (_, mut writer) = left.into_split();
        let (read_half, _) = right.into_split();
        let mut reader = BufReader::with_capacity(1024, FdReader::new(read_half));
        let payload: Bytes = (0..1_000_000u32)
            .map(|n| n as u8)
            .collect::<Vec<_>>()
            .into();

        let sent = payload.clone();
        let writing = tokio::spawn(async move {
            write(&mut writer, kind::BODY, flag::END, sent.clone())
                .await
                .unwrap();
            write(&mut writer, kind::BODY, 0, sent.clone())
                .await
                .unwrap();
            write(&mut writer, kind::BODY, 0, sent).await.unwrap();
            write(&mut writer, kind::BODY, 0, "after".into())
                .await
                .unwrap();
            writer
                .write_all(&[9, 0, 0, 0, kind::BODY, 0, 1, 2])
                .await
                .unwrap();
        });

        let frame = read(&mut reader).await.unwrap().expect("a frame");
        assert_eq!((frame.kind, frame.flags), (kind::BODY, flag::END));
        assert_eq!(frame.payload, payload);

        // A payload read in pieces, and one skipped, leave the reader at
        // the next frame.

        let header = read_header(&mut reader).await.unwrap().expect("a header");
        assert_eq!(header.len, 1_000_000);
        let mut pieces = BytesMut::new();

        while pieces.len() < header.len {
            let piece = read_piece(&mut reader, (header.len - pieces.len()).min(65536))
                .await
                .unwrap();
            assert!(!piece.is_empty() && piece.len() <= 65536);
            pieces.extend_from_slice(&piece);
        }

        assert_eq!(pieces, payload);
        let header = read_header(&mut reader).await.unwrap().expect("a header");
        skip(&mut reader, header.len).await.unwrap();
        assert_eq!(read(&mut reader).await.unwrap().unwrap().payload, "after");
        writing.await.unwrap();

        let short = read(&mut reader).await.unwrap_err();
        assert_eq!(short.kind(), io::ErrorKind::UnexpectedEof);
        assert!(read(&mut reader).await.unwrap().is_none());
    }
}
