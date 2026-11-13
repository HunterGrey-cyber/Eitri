use std::io;

pub fn require_own(peer_uid: u32, own_euid: u32) -> io::Result<()> {
    if peer_uid == own_euid {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("socket peer uid {peer_uid} differs from this user ({own_euid})"),
        ))
    }
}

pub fn check(stream: &tokio::net::UnixStream) -> io::Result<()> {
    // Tokio obtains Linux credentials with SO_PEERCRED without reading the socket.
    require_own(stream.peer_cred()?.uid(), agent::private_fs::euid())
}

#[cfg(test)]
mod tests {
    #[test]
    fn daemon_refuses_a_peer_of_another_uid() {
        super::require_own(42, 42).unwrap();
        assert_eq!(
            super::require_own(43, 42).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }
}
