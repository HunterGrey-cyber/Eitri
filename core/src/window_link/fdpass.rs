//! Descriptors across the link. A seqpacket socketpair keeps message boundaries, so the descriptors passed with a
//! packet belong to that packet only, and a peer's last packets arrive before its end-of-file. Every received
//! descriptor is close-on-exec from the moment it exists (`MSG_CMSG_CLOEXEC`), so no terminal, task or agent either
//! window starts later inherits it. Bodies travel in sealed memfds, whose content cannot change once sent.

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::FileExt;

/// The most descriptors one packet may carry (a move's 16 and a body's memfd fit with room).
pub const MAX_FDS_PER_PACKET: usize = 32;

const SEALS: libc::c_int = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;

fn last() -> io::Error {
    io::Error::last_os_error()
}

fn invalid(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why.to_owned())
}

/// Both ends close-on-exec.
pub fn seqpacket_pair() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` has room for the two descriptors `socketpair` writes.
    let rc = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    };
    if rc != 0 {
        return Err(last());
    }
    // SAFETY: both descriptors were just created and nothing else owns them.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Sends one packet: `bytes` (never empty: an empty packet reads as end-of-file) and `fds`, all or nothing.
pub fn send(sock: BorrowedFd<'_>, bytes: &[u8], fds: &[BorrowedFd<'_>]) -> io::Result<()> {
    if bytes.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "an empty packet"));
    }
    if fds.len() > MAX_FDS_PER_PACKET {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many descriptors for one packet",
        ));
    }
    let raw: Vec<RawFd> = fds.iter().map(|fd| fd.as_raw_fd()).collect();
    let payload = std::mem::size_of_val(raw.as_slice()) as u32;
    // SAFETY: `CMSG_SPACE` only computes a size.
    let space = if raw.is_empty() {
        0
    } else {
        (unsafe { libc::CMSG_SPACE(payload) }) as usize
    };
    let mut control = vec![0u64; space.div_ceil(8)];
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    // SAFETY: a zeroed `msghdr` is a valid empty one; the fields set below point at buffers that outlive the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if space > 0 {
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        // SAFETY: `msg_control` points at `space` zeroed, 8-byte-aligned bytes: room for one header and `raw`.
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(payload) as _;
            std::ptr::copy_nonoverlapping(raw.as_ptr().cast::<u8>(), libc::CMSG_DATA(cmsg), payload as usize);
        }
    }
    loop {
        // SAFETY: `msg` and everything it points at live for the call.
        let n = unsafe { libc::sendmsg(sock.as_raw_fd(), &msg, libc::MSG_NOSIGNAL) };
        if n >= 0 {
            return if n as usize == bytes.len() {
                Ok(())
            } else {
                Err(io::Error::new(io::ErrorKind::WriteZero, "a packet was cut short"))
            };
        }
        let err = last();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

#[derive(Debug)]
pub struct Received {
    pub bytes: Vec<u8>,
    pub fds: Vec<OwnedFd>,
}

/// Receives one packet of at most `max_len` bytes. `None` is the peer's end-of-file. A packet or a descriptor list
/// that did not fit is an error, and the descriptors that did arrive are closed.
pub fn recv(sock: BorrowedFd<'_>, max_len: usize) -> io::Result<Option<Received>> {
    let mut buf = vec![0u8; max_len];
    // SAFETY: `CMSG_SPACE` only computes a size.
    let space = unsafe { libc::CMSG_SPACE((MAX_FDS_PER_PACKET * std::mem::size_of::<RawFd>()) as u32) } as usize;
    let mut control = vec![0u64; space.div_ceil(8)];
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    // SAFETY: as in `send`.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    let n = loop {
        // SAFETY: `msg` and everything it points at live for the call.
        let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
        if n >= 0 {
            break n as usize;
        }
        let err = last();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    };
    let mut fds = Vec::new();
    // SAFETY: the headers walked are the ones the kernel wrote within `msg_controllen`; each descriptor in them is
    // new and owned by this process alone.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                let len = (*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                for i in 0..len / std::mem::size_of::<RawFd>() {
                    fds.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(data.add(i))));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    if msg.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
        drop(fds);
        return Err(invalid("a packet or its descriptors did not fit"));
    }
    if n == 0 && fds.is_empty() {
        return Ok(None);
    }
    buf.truncate(n);
    Ok(Some(Received { bytes: buf, fds }))
}

/// A close-on-exec memfd holding `bytes`, sealed against any change.
pub fn sealed_memfd(bytes: &[u8]) -> io::Result<OwnedFd> {
    // SAFETY: the name is a NUL-terminated literal.
    let raw = unsafe { libc::memfd_create(c"eitri-link-body".as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
    if raw < 0 {
        return Err(last());
    }
    // SAFETY: `memfd_create` returned a new descriptor nothing else owns.
    let mut file = File::from(unsafe { OwnedFd::from_raw_fd(raw) });
    file.write_all(bytes)?;
    let fd = OwnedFd::from(file);
    // SAFETY: `fd` is open for the call.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, SEALS) } != 0 {
        return Err(last());
    }
    Ok(fd)
}

/// A received body: `fd` must be a memfd carrying all four seals and exactly `len` bytes, and `len` within `cap`.
pub fn read_sealed(fd: &OwnedFd, len: usize, cap: usize) -> io::Result<Vec<u8>> {
    if len > cap {
        return Err(invalid("a body over its cap"));
    }
    // SAFETY: `fd` is open for the call.
    let seals = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GET_SEALS) };
    if seals < 0 || seals & SEALS != SEALS {
        return Err(invalid("a body that is not sealed"));
    }
    let file = File::from(fd.try_clone()?);
    if file.metadata()?.len() != len as u64 {
        return Err(invalid("a body whose size is not its header's"));
    }
    let mut buf = vec![0u8; len];
    file.read_exact_at(&mut buf, 0)?;
    Ok(buf)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FdKind {
    /// The master side of a pseudo-terminal.
    PtyMaster,
    /// Either end of a pipe.
    Pipe,
    /// A regular file of this user (a lease, a lock).
    OwnFile,
    /// A sealed memfd.
    Memfd,
    Socket,
}

fn fstat(fd: BorrowedFd<'_>) -> io::Result<libc::stat> {
    // SAFETY: a zeroed `stat` is a valid buffer for `fstat` to fill.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `fd` is open and `st` is writable for the call.
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return Err(last());
    }
    Ok(st)
}

/// Refuses a received descriptor that is not what its record says it is.
pub fn check(fd: BorrowedFd<'_>, want: FdKind) -> io::Result<()> {
    let st = fstat(fd)?;
    let format = st.st_mode & libc::S_IFMT;
    let ok = match want {
        FdKind::PtyMaster => {
            let mut n: libc::c_uint = 0;
            // SAFETY: `TIOCGPTN` writes one `c_uint`; it succeeds only on a pseudo-terminal master.
            format == libc::S_IFCHR && unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCGPTN, &mut n) } == 0
        }
        FdKind::Pipe => format == libc::S_IFIFO,
        // SAFETY: `geteuid` cannot fail.
        FdKind::OwnFile => format == libc::S_IFREG && st.st_uid == unsafe { libc::geteuid() },
        FdKind::Memfd => {
            // SAFETY: `fd` is open for the call; a file that is not a memfd answers an error.
            let seals = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GET_SEALS) };
            format == libc::S_IFREG && seals >= 0 && seals & SEALS == SEALS
        }
        FdKind::Socket => format == libc::S_IFSOCK,
    };
    if ok {
        Ok(())
    } else {
        Err(invalid(&format!("a descriptor that is not a {want:?}")))
    }
}

pub fn is_cloexec(fd: BorrowedFd<'_>) -> io::Result<bool> {
    // SAFETY: `fd` is open for the call.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
    if flags < 0 {
        return Err(last());
    }
    Ok(flags & libc::FD_CLOEXEC != 0)
}

/// Sets close-on-exec on a descriptor this process inherited and does not own yet (window 2's link end, before
/// anything else runs).
pub fn set_cloexec_raw(fd: RawFd) -> io::Result<()> {
    // SAFETY: `fcntl` on a descriptor number either acts on it or fails with `EBADF`.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    // SAFETY: as above.
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } != 0 {
        return Err(last());
    }
    Ok(())
}

pub fn socket_type(fd: BorrowedFd<'_>) -> io::Result<libc::c_int> {
    let mut value: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `value` and `len` are writable and sized for `SO_TYPE`.
    let rc = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut value as *mut libc::c_int).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(last());
    }
    Ok(value)
}

/// The peer's pid and effective uid, as the kernel recorded them when the pair was made.
pub fn peer_cred(fd: BorrowedFd<'_>) -> io::Result<(u32, u32)> {
    // SAFETY: a zeroed `ucred` is a valid buffer.
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` are writable and sized for `SO_PEERCRED`.
    let rc = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(last());
    }
    Ok((cred.pid as u32, cred.uid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    fn pipe() -> (OwnedFd, OwnedFd) {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: `fds` has room for two descriptors.
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        // SAFETY: both are new.
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }

    #[test]
    fn packets_keep_their_boundaries_and_their_own_descriptors() {
        let (a, b) = seqpacket_pair().unwrap();
        let (r, w) = pipe();
        send(a.as_fd(), b"one", &[r.as_fd(), w.as_fd()]).unwrap();
        send(a.as_fd(), b"two", &[]).unwrap();
        let first = recv(b.as_fd(), 1024).unwrap().unwrap();
        assert_eq!(first.bytes, b"one");
        assert_eq!(first.fds.len(), 2);
        for fd in &first.fds {
            assert!(
                is_cloexec(fd.as_fd()).unwrap(),
                "a received descriptor is close-on-exec"
            );
            check(fd.as_fd(), FdKind::Pipe).unwrap();
        }
        let second = recv(b.as_fd(), 1024).unwrap().unwrap();
        assert_eq!(second.bytes, b"two");
        assert!(second.fds.is_empty());
    }

    // The test that starts a child lives in `core/tests/window_link_fdpass_child.rs`: a fork from this binary would
    // hold copies of other tests' descriptors (the pair lock's among them) until it execs.

    #[test]
    fn a_peers_last_packet_arrives_before_its_end_of_file() {
        let (a, b) = seqpacket_pair().unwrap();
        send(a.as_fd(), b"done", &[]).unwrap();
        drop(a);
        assert_eq!(recv(b.as_fd(), 64).unwrap().unwrap().bytes, b"done");
        assert!(recv(b.as_fd(), 64).unwrap().is_none());
    }

    #[test]
    fn a_packet_that_does_not_fit_is_refused() {
        let (a, b) = seqpacket_pair().unwrap();
        let (r, _w) = pipe();
        send(a.as_fd(), &[7u8; 100], &[r.as_fd()]).unwrap();
        assert_eq!(recv(b.as_fd(), 10).unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert!(send(a.as_fd(), b"", &[]).is_err());
    }

    #[test]
    fn a_body_is_sealed_and_read_back_whole_and_nothing_else_is_accepted() {
        let data = vec![b'x'; 100_000];
        let fd = sealed_memfd(&data).unwrap();
        check(fd.as_fd(), FdKind::Memfd).unwrap();
        assert!(is_cloexec(fd.as_fd()).unwrap());
        assert_eq!(read_sealed(&fd, data.len(), 1 << 20).unwrap(), data);
        assert!(
            read_sealed(&fd, data.len() - 1, 1 << 20).is_err(),
            "size other than the header's"
        );
        assert!(read_sealed(&fd, data.len(), 10).is_err(), "over its cap");
        // SAFETY: a literal name.
        let raw = unsafe { libc::memfd_create(c"unsealed".as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
        assert!(raw >= 0);
        // SAFETY: new.
        let unsealed = unsafe { OwnedFd::from_raw_fd(raw) };
        assert!(read_sealed(&unsealed, 0, 10).is_err());
        assert!(check(unsealed.as_fd(), FdKind::Memfd).is_err());
    }

    #[test]
    fn each_descriptor_kind_is_told_apart() {
        let (r, _w) = pipe();
        assert!(check(r.as_fd(), FdKind::Pipe).is_ok());
        assert!(check(r.as_fd(), FdKind::PtyMaster).is_err());
        // SAFETY: opens a new pseudo-terminal master.
        let raw = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
        assert!(raw >= 0);
        // SAFETY: new.
        let master = unsafe { OwnedFd::from_raw_fd(raw) };
        assert!(check(master.as_fd(), FdKind::PtyMaster).is_ok());
        assert!(check(master.as_fd(), FdKind::Pipe).is_err());
        let body = sealed_memfd(b"x").unwrap();
        assert!(
            check(body.as_fd(), FdKind::OwnFile).is_ok(),
            "a memfd is a regular file of ours"
        );
        let (s, _t) = seqpacket_pair().unwrap();
        assert!(check(s.as_fd(), FdKind::Socket).is_ok());
        assert!(check(s.as_fd(), FdKind::OwnFile).is_err());
    }

    #[test]
    fn the_pair_reports_its_type_and_its_peers_credentials() {
        let (a, _b) = seqpacket_pair().unwrap();
        assert_eq!(socket_type(a.as_fd()).unwrap(), libc::SOCK_SEQPACKET);
        let (pid, uid) = peer_cred(a.as_fd()).unwrap();
        assert_eq!(pid, std::process::id());
        // SAFETY: cannot fail.
        assert_eq!(uid, unsafe { libc::geteuid() });
    }

    #[test]
    fn close_on_exec_can_be_set_on_an_inherited_number() {
        let (r, _w) = pipe();
        // SAFETY: `dup` of an open descriptor; the copy has no close-on-exec.
        let raw = unsafe { libc::dup(r.as_raw_fd()) };
        // SAFETY: new.
        let copy = unsafe { OwnedFd::from_raw_fd(raw) };
        assert!(!is_cloexec(copy.as_fd()).unwrap());
        set_cloexec_raw(raw).unwrap();
        assert!(is_cloexec(copy.as_fd()).unwrap());
    }
}
