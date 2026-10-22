//! A child started after a descriptor receive inherits none of the received descriptors. Its own binary and its
//! only test: the child's fork holds copies of every descriptor of the process until it execs, which in the
//! library's test binary would keep other tests' descriptors (a held lock among them) open behind their backs,
//! and other tests closing descriptors meanwhile would move the received numbers onto the ones a child opens itself.

#![cfg(target_os = "linux")]

use std::fs::File;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};

use eitri_core::window_link::fdpass::{recv, send, seqpacket_pair};

fn pipe() -> (OwnedFd, OwnedFd) {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` has room for two descriptors.
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    // SAFETY: both are new.
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

#[test]
fn a_child_started_after_a_receive_inherits_none_of_the_received_descriptors() {
    let (a, b) = seqpacket_pair().unwrap();
    // Push the received numbers above anything a small child opens itself.
    let _pad: Vec<File> = (0..20).map(|_| File::open("/dev/null").unwrap()).collect();
    let (r, w) = pipe();
    send(a.as_fd(), b"x", &[r.as_fd(), w.as_fd()]).unwrap();
    let got = recv(b.as_fd(), 16).unwrap().unwrap();
    let numbers: Vec<String> = got.fds.iter().map(|fd| fd.as_raw_fd().to_string()).collect();
    let out = std::process::Command::new("sh")
        .args(["-c", "ls /proc/self/fd"])
        .output()
        .unwrap();
    let listed = String::from_utf8(out.stdout).unwrap();
    for n in numbers {
        assert!(
            !listed.lines().any(|l| l.trim() == n),
            "fd {n} leaked into a child: {listed}"
        );
    }
}
