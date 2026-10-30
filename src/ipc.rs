//! One session at a time: the first instance listens on an abstract unix socket,
//! later instances connect to it, and every connection means "toggle".

use std::io;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};

fn addr() -> io::Result<SocketAddr> {
    let uid = rustix::process::getuid().as_raw();
    SocketAddr::from_abstract_name(format!("voicetotextn-{uid}"))
}

/// Returns a listener when no session is running. Otherwise toggles the
/// running session and returns `None`.
pub fn claim() -> io::Result<Option<UnixListener>> {
    let addr = addr()?;
    if UnixStream::connect_addr(&addr).is_ok() {
        return Ok(None);
    }
    match UnixListener::bind_addr(&addr) {
        Ok(listener) => {
            listener.set_nonblocking(true)?;
            Ok(Some(listener))
        }
        // Another instance won the race between our connect and bind.
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => UnixStream::connect_addr(&addr).map(|_| None),
        Err(e) => Err(e),
    }
}

/// Accepts all pending connections and returns how many toggles arrived.
pub fn drain(listener: &UnixListener) -> usize {
    let mut n = 0;
    while listener.accept().is_ok() {
        n += 1;
    }
    n
}
