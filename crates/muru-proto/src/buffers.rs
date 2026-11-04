//! The worker link's socket buffers (design specification section 5.1).
//! On a Unix stream socket only the sender's send buffer limits what is
//! written before the writer has to wait, and a socket accepted from a
//! listener takes the listener's sizes. So the worker sets it on its
//! listening socket for responses, and the edge on its connected socket
//! for request bodies.

use std::io;
use std::os::fd::AsRawFd;

/// The send buffer each side of the worker link asks for. On macOS the
/// default is 8 KiB, so a large body crossed the link in 8 KiB steps,
/// each a wakeup on both sides; 256 KiB is where measuring large responses
/// stopped improving. Linux starts a Unix socket near this already.
pub const SEND_BUFFER: usize = 256 * 1024;

/// Ask for `SEND_BUFFER` as the send buffer of `socket`. The system may
/// give less (Linux caps it at `net.core.wmem_max`); it refuses only a
/// size above what it allows at all, which the caller reports and lives
/// with.
pub fn set_send_buffer(socket: &impl AsRawFd) -> io::Result<()> {
    let size = SEND_BUFFER as libc::c_int;

    // SAFETY: the option value is a c_int that lives for the call.

    let outcome = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            (&raw const size).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };

    if outcome == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use std::io::{ErrorKind, Write};
    use std::os::unix::net::UnixStream;

    use super::*;

    /// How many bytes `writer` takes before it would block.
    fn queued(writer: &mut UnixStream) -> usize {
        writer.set_nonblocking(true).unwrap();
        let piece = [0u8; 4096];
        let mut total = 0;

        loop {
            match writer.write(&piece) {
                Ok(count) => total += count,
                Err(error) if error.kind() == ErrorKind::WouldBlock => return total,
                Err(error) => panic!("{error}"),
            }
        }
    }

    #[test]
    fn the_sender_holds_a_large_body_before_it_waits() {
        let (mut writer, _reader) = UnixStream::pair().unwrap();
        set_send_buffer(&writer).unwrap();

        // Linux counts its own overhead against the buffer and caps it, so
        // only macOS holds exactly what was asked for.

        if cfg!(target_os = "macos") {
            assert!(queued(&mut writer) >= SEND_BUFFER);
        } else {
            assert!(queued(&mut writer) > 64 * 1024);
        }
    }
}
