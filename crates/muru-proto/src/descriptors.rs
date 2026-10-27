//! Descriptor passing on the worker link (design specification section
//! 5.7): a file descriptor travels with SCM_RIGHTS attached to the first
//! byte of the frame that refers to it, so the receiver has it by the time
//! it has decoded that frame. A side that may receive one reads the socket
//! with recvmsg and a control buffer, since a plain read discards an
//! attached descriptor. A frame with more descriptors than one message
//! carries has them spread over its first bytes, one message each.

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use tokio::io::{AsyncRead, AsyncWriteExt, Interest, ReadBuf};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

/// The most descriptors one message carries.
pub const MAX_DESCRIPTORS: usize = 64;

/// Room for a control message carrying `MAX_DESCRIPTORS` descriptors.
const CONTROL_LEN: usize = 512;

/// A control buffer aligned as the kernel requires.
#[repr(C, align(8))]
struct Control([u8; CONTROL_LEN]);

/// The control message length for `count` descriptors.
fn control_space(count: usize) -> usize {
    // SAFETY: CMSG_SPACE only computes a size.

    unsafe { libc::CMSG_SPACE((count * std::mem::size_of::<RawFd>()) as u32) as usize }
}

/// The read half of a link that keeps the descriptors attached to what it
/// reads, for the frames that refer to them.
pub struct FdReader {
    half: OwnedReadHalf,
    received: VecDeque<OwnedFd>,
}

impl FdReader {
    /// Wrap the read half of a connected Unix stream.
    pub fn new(half: OwnedReadHalf) -> Self {
        Self {
            half,
            received: VecDeque::new(),
        }
    }

    /// The oldest descriptor received and not yet taken: the one attached
    /// to the frame just decoded, when the frames that carry descriptors
    /// are the only ones that do.
    pub fn take_fd(&mut self) -> Option<OwnedFd> {
        self.received.pop_front()
    }
}

impl AsyncRead for FdReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let FdReader { half, received } = self.get_mut();

        loop {
            ready!(half.as_ref().poll_read_ready(cx))?;
            let destination = buf.initialize_unfilled();
            let stream = half.as_ref();
            let outcome = stream.try_io(Interest::READABLE, || {
                recv_message(stream.as_raw_fd(), destination, received)
            });

            match outcome {
                Ok(count) => {
                    buf.advance(count);
                    return Poll::Ready(Ok(()));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                Err(error) => return Poll::Ready(Err(error)),
            }
        }
    }
}

/// One recvmsg on `socket` into `buffer`, pushing the descriptors that
/// arrive onto `received`.
fn recv_message(
    socket: RawFd,
    buffer: &mut [u8],
    received: &mut VecDeque<OwnedFd>,
) -> io::Result<usize> {
    let mut iov = libc::iovec {
        iov_base: buffer.as_mut_ptr().cast(),
        iov_len: buffer.len(),
    };
    let mut control = Control([0u8; CONTROL_LEN]);

    // SAFETY: every pointer refers to a buffer that lives for the call,
    // the control buffer is aligned and sized for MAX_DESCRIPTORS, and
    // the descriptors read out of it are ones the kernel has just
    // installed in this process, each taken exactly once.

    unsafe {
        let mut message: libc::msghdr = std::mem::zeroed();
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.0.as_mut_ptr().cast();
        message.msg_controllen = control_space(MAX_DESCRIPTORS) as _;
        let count = libc::recvmsg(socket, &mut message, 0);

        if count < 0 {
            return Err(io::Error::last_os_error());
        }

        let mut header = libc::CMSG_FIRSTHDR(&message);

        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(header);
                let bytes =
                    (*header).cmsg_len as usize - (data as usize - header as *const u8 as usize);

                for index in 0..bytes / std::mem::size_of::<RawFd>() {
                    let mut descriptor: RawFd = -1;
                    std::ptr::copy_nonoverlapping(
                        data.cast::<RawFd>().add(index),
                        &mut descriptor,
                        1,
                    );

                    if descriptor >= 0 {
                        libc::fcntl(descriptor, libc::F_SETFD, libc::FD_CLOEXEC);
                        received.push_back(OwnedFd::from_raw_fd(descriptor));
                    }
                }
            }

            header = libc::CMSG_NXTHDR(&message, header);
        }

        if message.msg_flags & libc::MSG_CTRUNC != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "more descriptors arrived than the link accepts",
            ));
        }

        Ok(count as usize)
    }
}

/// Write `bytes`, a whole frame, with `fd` attached to its first byte.
pub async fn write_with_fd(
    writer: &mut OwnedWriteHalf,
    bytes: &[u8],
    fd: BorrowedFd<'_>,
) -> io::Result<()> {
    write_with_fds(writer, bytes, &[fd]).await
}

/// Write `bytes`, a whole frame, with `fds` attached to its first bytes:
/// up to `MAX_DESCRIPTORS` with each byte, the last group with the rest
/// of the frame, so the receiver holds all of them, in order, once it has
/// read the frame.
pub async fn write_with_fds(
    writer: &mut OwnedWriteHalf,
    bytes: &[u8],
    fds: &[BorrowedFd<'_>],
) -> io::Result<()> {
    let groups: Vec<&[BorrowedFd<'_>]> = fds.chunks(MAX_DESCRIPTORS).collect();

    if groups.len() > bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "more descriptors than the frame has bytes to carry them",
        ));
    }

    let mut written = 0;

    for (index, group) in groups.iter().enumerate() {
        let end = if index + 1 == groups.len() {
            bytes.len()
        } else {
            written + 1
        };

        // A message sent in part has carried its descriptors with its
        // first byte; what is left of it goes as plain bytes.

        let piece = &bytes[written..end];
        let stream = writer.as_ref();
        let sent = stream
            .async_io(Interest::WRITABLE, || {
                send_message(stream.as_raw_fd(), piece, group)
            })
            .await?;

        if sent < piece.len() {
            writer.write_all(&piece[sent..]).await?;
        }

        written = end;
    }

    if written < bytes.len() {
        writer.write_all(&bytes[written..]).await?;
    }

    Ok(())
}

/// One sendmsg of `bytes` on `socket` with `fds` attached.
fn send_message(socket: RawFd, bytes: &[u8], fds: &[BorrowedFd<'_>]) -> io::Result<usize> {
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast(),
        iov_len: bytes.len(),
    };
    let mut control = Control([0u8; CONTROL_LEN]);
    let raw: Vec<RawFd> = fds.iter().map(AsRawFd::as_raw_fd).collect();

    // SAFETY: every pointer refers to a buffer that lives for the call,
    // and the control buffer is aligned and sized for MAX_DESCRIPTORS,
    // which bounds `fds`.

    unsafe {
        let mut message: libc::msghdr = std::mem::zeroed();
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.0.as_mut_ptr().cast();
        message.msg_controllen = control_space(raw.len()) as _;
        let header = libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN((raw.len() * std::mem::size_of::<RawFd>()) as u32) as _;
        std::ptr::copy_nonoverlapping(
            raw.as_ptr(),
            libc::CMSG_DATA(header).cast::<RawFd>(),
            raw.len(),
        );
        let sent = libc::sendmsg(socket, &message, 0);

        if sent < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(sent as usize)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Seek as _, Write as _};
    use std::os::fd::AsFd;

    use tokio::io::{AsyncReadExt as _, BufReader};
    use tokio::net::UnixStream;

    use super::*;

    #[tokio::test]
    async fn a_descriptor_arrives_with_the_frame_that_carries_it() {
        let (left, right) = UnixStream::pair().unwrap();
        let (_, mut writer) = left.into_split();
        let (read_half, _) = right.into_split();
        let mut reader = BufReader::with_capacity(1024, FdReader::new(read_half));
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"the file").unwrap();
        file.rewind().unwrap();

        writer.write_all(b"before").await.unwrap();
        write_with_fd(&mut writer, b"frame", file.as_fd())
            .await
            .unwrap();
        writer.write_all(b"after").await.unwrap();
        drop(writer);

        let mut text = String::new();
        reader.read_to_string(&mut text).await.unwrap();
        assert_eq!(text, "beforeframeafter");
        let received = reader.get_mut().take_fd().expect("a descriptor");
        let mut copy = std::fs::File::from(received);
        let mut content = String::new();
        copy.read_to_string(&mut content).unwrap();
        assert_eq!(content, "the file");
        assert!(reader.get_mut().take_fd().is_none());
    }

    #[tokio::test]
    async fn many_descriptors_arrive_in_order_with_their_frame() {
        let (left, right) = UnixStream::pair().unwrap();
        let (_, mut writer) = left.into_split();
        let (read_half, _) = right.into_split();
        let mut reader = BufReader::with_capacity(1024, FdReader::new(read_half));
        let files: Vec<std::fs::File> = (0..150)
            .map(|index| {
                let mut file = tempfile::tempfile().unwrap();
                write!(file, "file {index}").unwrap();
                file
            })
            .collect();
        let borrowed: Vec<BorrowedFd<'_>> = files.iter().map(AsFd::as_fd).collect();

        write_with_fds(&mut writer, b"a frame of some length", &borrowed)
            .await
            .unwrap();
        writer.write_all(b"after").await.unwrap();
        drop(writer);

        let mut text = String::new();
        reader.read_to_string(&mut text).await.unwrap();
        assert_eq!(text, "a frame of some lengthafter");

        for index in 0..150 {
            let received = reader.get_mut().take_fd().expect("a descriptor");
            let mut copy = std::fs::File::from(received);
            let mut content = String::new();
            copy.rewind().unwrap();
            copy.read_to_string(&mut content).unwrap();
            assert_eq!(content, format!("file {index}"));
        }

        assert!(reader.get_mut().take_fd().is_none());
    }

    #[tokio::test]
    async fn a_read_stops_at_the_descriptor_so_it_belongs_to_the_next_frame() {
        let (left, right) = UnixStream::pair().unwrap();
        let (_, mut writer) = left.into_split();
        let (read_half, _) = right.into_split();
        let mut reader = FdReader::new(read_half);
        let file = tempfile::tempfile().unwrap();

        writer.write_all(b"before").await.unwrap();
        write_with_fd(&mut writer, b"frame", file.as_fd())
            .await
            .unwrap();
        writer.write_all(b"after").await.unwrap();
        drop(writer);

        // Whatever the first read returns, by the time the frame's first
        // byte has been read the descriptor is in hand, and a read never
        // runs past the frame into what follows it without it.

        let mut everything = Vec::new();

        loop {
            let mut chunk = [0u8; 64];
            let count = reader.read(&mut chunk).await.unwrap();

            if count == 0 {
                break;
            }

            everything.extend_from_slice(&chunk[..count]);

            if everything.len() > b"before".len() {
                assert!(
                    !reader.received.is_empty(),
                    "the descriptor arrived with its frame"
                );
            }
        }

        assert_eq!(everything, b"beforeframeafter");
        assert_eq!(reader.received.len(), 1);
    }
}
