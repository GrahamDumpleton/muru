//! Readiness and status for a service manager (design specification
//! section 13.3): sd_notify messages to the socket NOTIFY_SOCKET names,
//! and nothing at all when it is unset.

use std::os::unix::net::UnixDatagram;

/// Send a state such as `READY=1` to the service manager, when there
/// is one. Failures are logged and otherwise ignored: the service runs
/// the same either way.
pub fn notify(state: &str) {
    let Some(socket_name) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };

    let socket_name = socket_name.to_string_lossy();

    if let Err(error) = send(&socket_name, state) {
        tracing::debug!(%error, socket = %socket_name, "notifying the service manager failed");
    }
}

/// Tell the service manager the daemon is ready, with a status line.
pub fn ready(status: &str) {
    notify(&format!("READY=1\nSTATUS={status}"));
}

/// Tell the service manager the daemon is on its way out.
pub fn stopping() {
    notify("STOPPING=1");
}

/// Send a state to a notification socket by name: a path, or `@` and an
/// abstract name on Linux.
fn send(socket_name: &str, state: &str) -> std::io::Result<()> {
    let socket = UnixDatagram::unbound()?;

    match socket_name.strip_prefix('@') {
        Some(name) => send_abstract(&socket, name, state),
        None => socket.send_to(state.as_bytes(), socket_name).map(|_| ()),
    }
}

#[cfg(target_os = "linux")]
fn send_abstract(socket: &UnixDatagram, name: &str, state: &str) -> std::io::Result<()> {
    use std::os::linux::net::SocketAddrExt as _;

    let address = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())?;
    socket.send_to_addr(state.as_bytes(), &address).map(|_| ())
}

#[cfg(not(target_os = "linux"))]
fn send_abstract(_: &UnixDatagram, _: &str, _: &str) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "abstract socket names are only available on Linux",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_reach_the_named_socket() {
        let dir = std::env::temp_dir().join(format!("muru-notify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notify.sock");
        let _ = std::fs::remove_file(&path);
        let receiver = UnixDatagram::bind(&path).unwrap();
        send(path.to_str().unwrap(), "READY=1\nSTATUS=serving").unwrap();
        let mut buffer = [0u8; 256];
        let read = receiver.recv(&mut buffer).unwrap();
        assert_eq!(
            std::str::from_utf8(&buffer[..read]).unwrap(),
            "READY=1\nSTATUS=serving"
        );
        assert!(send(dir.join("missing.sock").to_str().unwrap(), "READY=1").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
