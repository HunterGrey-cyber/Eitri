//! What the session has to write to the PTY and could not yet: the user's input, and the answers
//! the terminal owes the program.
//!
//! The two are kept apart because they deserve different treatment when the program stops reading
//! its input. A program that asks the terminal questions and never
//! reads the answers -- `yes $'\e[6n'` -- fills the tty's input buffer, after which every write
//! gets `EAGAIN`; a single queue then grows by an answer per question, at the rate the program
//! can print, until the whole IDE runs out of memory. So:
//! - **input is retained until written or explicitly discarded**: it is what the user typed or
//!   pasted; a remote attachment discards its pending input when its stream ends;
//! - **answers are capped** at [`REPLY_CAP`] bytes pending; beyond it they are dropped and counted.
//!   An answer nobody reads is worth nothing, and xterm and foot answer on a best-effort basis too;
//! - **input goes first**, so a backlog of answers never delays a key. The one exception is an
//!   answer the kernel has already taken part of: it is finished first, because a key in the middle
//!   of an escape sequence would corrupt both. For the same reason a unit (one key's or one read's
//!   worth of answers) is never split by the other kind.
//!
//! Pure: [`Outbox::flush`] is given the write to use, so every rule is tested without a PTY.

use std::collections::VecDeque;
use std::io;

/// The most answer bytes that wait for a program that is not reading them.
pub const REPLY_CAP: usize = 64 * 1024;

/// How a [`Outbox::flush`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flushed {
    /// Everything was written.
    Drained,
    /// The kernel took no more (`EAGAIN`, or a write of zero bytes); the rest waits for `POLLOUT`.
    Blocked,
    /// The write failed otherwise: the child is gone. Everything pending was discarded.
    Closed,
}

#[derive(Debug, Default)]
pub struct Outbox {
    input: VecDeque<u8>,
    replies: VecDeque<Vec<u8>>,
    /// Bytes of `replies.front()` already written: while non-zero, that answer finishes first.
    reply_written: usize,
    reply_bytes: usize,
    dropped: u64,
    high_water: usize,
    input_written: u64,
}

impl Outbox {
    pub fn new() -> Self {
        Outbox::default()
    }

    /// Queues input from the user. Never dropped.
    pub fn push_input(&mut self, bytes: &[u8]) {
        self.input.extend(bytes);
        self.note_len();
    }

    /// Total user-input bytes accepted by the PTY, excluding terminal replies.
    pub fn input_written(&self) -> u64 {
        self.input_written
    }

    /// Drops only input that has not reached the PTY. A partially written reply still finishes.
    pub fn discard_input(&mut self) {
        self.input.clear();
    }

    /// Queues answers to the program, as one unit. `false`: dropped, because the answers already
    /// waiting and these would exceed [`REPLY_CAP`].
    pub fn push_reply(&mut self, bytes: Vec<u8>) -> bool {
        if bytes.is_empty() {
            return true;
        }
        if self.reply_bytes + bytes.len() > REPLY_CAP {
            self.dropped += bytes.len() as u64;
            return false;
        }
        self.reply_bytes += bytes.len();
        self.replies.push_back(bytes);
        self.note_len();
        true
    }

    pub fn is_empty(&self) -> bool {
        self.input.is_empty() && self.replies.is_empty()
    }

    /// Bytes waiting, of both kinds.
    pub fn len(&self) -> usize {
        self.input.len() + self.reply_bytes - self.reply_written
    }

    /// The most bytes that ever waited at once.
    pub fn high_water(&self) -> usize {
        self.high_water
    }

    /// Answer bytes dropped at the cap, ever.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Writes as much as `write` takes: a started answer first, then input, then answers.
    pub fn flush(&mut self, mut write: impl FnMut(&[u8]) -> io::Result<usize>) -> Flushed {
        loop {
            let from_input = self.reply_written == 0 && !self.input.is_empty();
            let chunk: &[u8] = if from_input {
                self.input.as_slices().0
            } else if let Some(reply) = self.replies.front() {
                &reply[self.reply_written..]
            } else {
                return Flushed::Drained;
            };
            match write(chunk) {
                Ok(0) => return Flushed::Blocked,
                Ok(n) if from_input => {
                    self.input.drain(..n);
                    self.input_written += n as u64;
                }
                Ok(n) => {
                    self.reply_written += n;
                    let front = self.replies.front().map_or(0, Vec::len);
                    if self.reply_written == front {
                        self.replies.pop_front();
                        self.reply_bytes -= front;
                        self.reply_written = 0;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Flushed::Blocked,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    self.input.clear();
                    self.replies.clear();
                    self.reply_bytes = 0;
                    self.reply_written = 0;
                    return Flushed::Closed;
                }
            }
        }
    }

    /// Everything not yet written, in the order [`Outbox::flush`] would have written it -- a
    /// started answer's rest, then input, then the other answers -- leaving the outbox empty. What
    /// a terminal moving to another process takes along, for its new holder to write.
    pub fn take_unwritten(&mut self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.len());
        if self.reply_written != 0 {
            if let Some(front) = self.replies.pop_front() {
                out.extend_from_slice(&front[self.reply_written..]);
            }
        }
        out.extend(self.input.drain(..));
        for reply in self.replies.drain(..) {
            out.extend_from_slice(&reply);
        }
        self.reply_bytes = 0;
        self.reply_written = 0;
        out
    }

    fn note_len(&mut self) {
        self.high_water = self.high_water.max(self.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discard_preserves_replies_and_counts_only_partial_input_writes() {
        let mut out = Outbox::new();
        out.push_reply(b"reply".to_vec());
        let mut kernel = Kernel::new(2, 2);
        out.flush(|b| kernel.write(b));
        out.push_input(b"stale");
        out.discard_input();
        out.push_input(b"new");
        kernel.room = 6;
        out.flush(|b| kernel.write(b));
        assert_eq!(kernel.taken, b"replyn");
        assert_eq!(out.input_written(), 1);
        out.discard_input();
        kernel.room = 100;
        out.flush(|b| kernel.write(b));
        assert_eq!(kernel.taken, b"replyn");
        assert_eq!(out.input_written(), 1);
    }

    /// A kernel that takes at most `room` bytes in all, `per_write` at a time.
    struct Kernel {
        taken: Vec<u8>,
        room: usize,
        per_write: usize,
    }

    impl Kernel {
        fn new(room: usize, per_write: usize) -> Self {
            Kernel {
                taken: Vec::new(),
                room,
                per_write,
            }
        }

        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let n = bytes.len().min(self.per_write).min(self.room - self.taken.len());
            if n == 0 {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            self.taken.extend_from_slice(&bytes[..n]);
            Ok(n)
        }
    }

    #[test]
    fn answers_beyond_the_cap_are_dropped_and_counted_and_input_never_is() {
        let mut outbox = Outbox::new();
        let reply = vec![b'r'; 1024];
        let accepted = (0..100).filter(|_| outbox.push_reply(reply.clone())).count();
        assert_eq!(accepted, REPLY_CAP / 1024);
        assert_eq!(outbox.dropped(), ((100 - REPLY_CAP / 1024) * 1024) as u64);
        outbox.push_input(&vec![b'i'; 3 * REPLY_CAP]);
        assert_eq!(
            outbox.len(),
            REPLY_CAP + 3 * REPLY_CAP,
            "input past the cap is kept whole"
        );
        assert_eq!(outbox.high_water(), outbox.len());
    }

    /// What is taken along is exactly what a flush with endless room would still have written,
    /// in the same order, and nothing is left behind.
    #[test]
    fn the_unwritten_bytes_are_taken_in_the_order_a_flush_would_write_them() {
        let mut outbox = Outbox::new();
        outbox.push_reply(b"\x1b[12;40R".to_vec());
        let mut kernel = Kernel::new(4, usize::MAX);
        assert_eq!(outbox.flush(|b| kernel.write(b)), Flushed::Blocked);
        outbox.push_input(b"ls\r");
        outbox.push_reply(b"\x1b[?6c".to_vec());
        let mut twin = Outbox::new();
        twin.push_input(&outbox.take_unwritten());
        assert!(outbox.is_empty());
        assert_eq!(outbox.len(), 0);
        let mut rest = Kernel::new(usize::MAX, usize::MAX);
        twin.flush(|b| rest.write(b));
        assert_eq!(kernel.taken, b"\x1b[12");
        assert_eq!(rest.taken, b";40Rls\r\x1b[?6c");
    }

    #[test]
    fn queued_input_goes_before_queued_answers() {
        let mut outbox = Outbox::new();
        outbox.push_reply(b"\x1b[1;1R".to_vec());
        outbox.push_input(b"ls\r");
        let mut kernel = Kernel::new(usize::MAX, usize::MAX);
        assert_eq!(outbox.flush(|b| kernel.write(b)), Flushed::Drained);
        assert_eq!(kernel.taken, b"ls\r\x1b[1;1R");
        assert!(outbox.is_empty());
    }

    #[test]
    fn an_answer_the_kernel_took_part_of_finishes_before_input_resumes() {
        let mut outbox = Outbox::new();
        outbox.push_reply(b"\x1b[12;40R".to_vec());
        let mut kernel = Kernel::new(3, 3);
        assert_eq!(outbox.flush(|b| kernel.write(b)), Flushed::Blocked);
        assert_eq!(kernel.taken, b"\x1b[1");
        outbox.push_input(b"x");
        kernel.room = usize::MAX;
        assert_eq!(outbox.flush(|b| kernel.write(b)), Flushed::Drained);
        assert_eq!(kernel.taken, b"\x1b[12;40Rx", "never a key inside an escape sequence");
    }

    #[test]
    fn a_blocked_write_keeps_everything_and_a_dead_child_discards_it() {
        let mut outbox = Outbox::new();
        outbox.push_input(b"abc");
        outbox.push_reply(b"\x1b[0n".to_vec());
        assert_eq!(
            outbox.flush(|_| Err(io::ErrorKind::WouldBlock.into())),
            Flushed::Blocked
        );
        assert_eq!(outbox.len(), 7);
        assert_eq!(outbox.flush(|_| Ok(0)), Flushed::Blocked);
        assert_eq!(outbox.len(), 7);
        assert_eq!(
            outbox.flush(|_| Err(io::Error::from_raw_os_error(libc::EIO))),
            Flushed::Closed
        );
        assert!(outbox.is_empty());
        assert!(
            outbox.push_reply(vec![b'r'; REPLY_CAP]),
            "the cap is on what is pending, not ever"
        );
    }
}
