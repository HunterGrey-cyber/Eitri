//! How one thread of the listener wakes another that sits in `poll`.

use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};

use crate::window_link::link::{clear, eventfd, signal};

/// An eventfd: readable from a [`Wake::signal`] until the next [`Wake::drain`]. Signals collapse, so a waker that
/// fires twice before the sleeper runs costs one wake, and the sleeper looks at its queues after draining.
pub struct Wake(OwnedFd);

impl Wake {
    pub fn new() -> io::Result<Wake> {
        eventfd().map(Wake)
    }

    pub fn signal(&self) {
        signal(self.0.as_fd());
    }

    pub fn drain(&self) {
        clear(self.0.as_fd());
    }

    /// For a `pollfd`. Valid for as long as this `Wake` lives.
    pub fn fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn readable(wake: &Wake) -> bool {
        let mut fds = [libc::pollfd {
            fd: wake.fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        // SAFETY: `fds` is a valid array of one entry for the call.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 1, 0) };
        assert!(ready >= 0, "poll failed: {}", io::Error::last_os_error());
        fds[0].revents & libc::POLLIN != 0
    }

    #[test]
    fn a_signal_wakes_a_poll_and_a_drain_clears_it() {
        let wake = Wake::new().unwrap();
        assert!(!readable(&wake), "a new wake has nothing to say");
        wake.drain();
        assert!(!readable(&wake), "a drain with nothing pending returns and stays clear");

        wake.signal();
        wake.signal();
        assert!(readable(&wake), "a signal makes it readable");
        wake.drain();
        assert!(!readable(&wake), "one drain clears any number of signals");

        wake.signal();
        assert!(readable(&wake), "it wakes again after a drain");
    }
}
