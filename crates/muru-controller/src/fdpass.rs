//! Passing file descriptors between processes over a Unix socket with
//! SCM_RIGHTS (design specification sections 2.3 and 5.7). The daemon
//! hands listening sockets to edges this way, and takes them back from a
//! running edge when it has restarted without one.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

/// Send one descriptor with a single byte of payload.
pub fn send_fd(socket: &UnixStream, fd: RawFd) -> io::Result<()> {
    let mut payload = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: payload.as_mut_ptr().cast(),
        iov_len: payload.len(),
    };
    let mut control = Control([0u8; CONTROL_LEN]);

    // SAFETY: every pointer refers to a buffer that lives for the call,
    // and the control buffer is aligned and sized for one descriptor.

    unsafe {
        let mut message: libc::msghdr = std::mem::zeroed();
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.0.as_mut_ptr().cast();
        message.msg_controllen = control_space() as _;

        let header = libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::copy_nonoverlapping(&fd, libc::CMSG_DATA(header).cast::<RawFd>(), 1);

        if libc::sendmsg(socket.as_raw_fd(), &message, 0) < 0 {
            return Err(io::Error::last_os_error());
        }
    }

    Ok(())
}

/// Receive one descriptor sent with `send_fd`.
pub fn recv_fd(socket: &UnixStream) -> io::Result<OwnedFd> {
    let mut payload = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: payload.as_mut_ptr().cast(),
        iov_len: payload.len(),
    };
    let mut control = Control([0u8; CONTROL_LEN]);

    // SAFETY: as for send_fd; the descriptor read out of the control
    // message is one the kernel has just installed in this process.

    unsafe {
        let mut message: libc::msghdr = std::mem::zeroed();
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.0.as_mut_ptr().cast();
        message.msg_controllen = control_space() as _;

        let received = libc::recvmsg(socket.as_raw_fd(), &mut message, 0);

        if received < 0 {
            return Err(io::Error::last_os_error());
        }

        let header = libc::CMSG_FIRSTHDR(&message);
        if header.is_null()
            || (*header).cmsg_level != libc::SOL_SOCKET
            || (*header).cmsg_type != libc::SCM_RIGHTS
        {
            return Err(io::Error::other("no descriptor was sent"));
        }
        let mut fd: RawFd = -1;
        std::ptr::copy_nonoverlapping(libc::CMSG_DATA(header).cast::<RawFd>(), &mut fd, 1);

        if fd < 0 {
            return Err(io::Error::other("an invalid descriptor was sent"));
        }

        Ok(OwnedFd::from_raw_fd(fd))
    }
}

/// Room for one control message carrying one descriptor.
const CONTROL_LEN: usize = 32;

/// A control buffer aligned as the kernel requires.
#[repr(C, align(8))]
struct Control([u8; CONTROL_LEN]);

/// The control message length for one descriptor.
fn control_space() -> usize {
    // SAFETY: CMSG_SPACE only computes a size.

    unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as usize }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use super::*;

    #[test]
    fn a_descriptor_crosses_a_socket_pair() {
        let (left, right) = UnixStream::pair().unwrap();
        let (mut writer, reader) = std::os::unix::net::UnixStream::pair().unwrap();

        send_fd(&left, reader.as_raw_fd()).unwrap();
        let received = recv_fd(&right).unwrap();
        drop(reader);

        // The received descriptor is the read end of the second pair.

        writer.write_all(b"hello").unwrap();
        drop(writer);
        let mut copy = UnixStream::from(received);
        let mut text = String::new();
        copy.read_to_string(&mut text).unwrap();
        assert_eq!(text, "hello");
    }
}
