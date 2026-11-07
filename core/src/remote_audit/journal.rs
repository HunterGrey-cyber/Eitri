//! The journal copy: each record as one datagram of the journal's native protocol, with no library.
//!
//! A field is `NAME=value\n`; a value holding a newline is sent in the binary form, `NAME\n`, its length as eight
//! bytes little-endian, the value, `\n`. The send waits at most [`SEND_WITHIN`] for room in the journal's queue and
//! a failure is dropped: the file is the record of truth for the writer, and `verify` reports a run whose journal
//! copies skip a number.

use super::JournalTarget;
use std::os::unix::net::UnixDatagram;
use std::time::Duration;

/// How long a send may wait for the journal to take it.
const SEND_WITHIN: Duration = Duration::from_millis(50);

/// The journal side of one writer.
pub(super) struct Journal {
    target: JournalTarget,
    socket: Option<UnixDatagram>,
}

impl Journal {
    pub(super) fn new(target: JournalTarget) -> Journal {
        let mut journal = Journal { target, socket: None };
        journal.socket = journal.make_socket();
        journal
    }

    /// An unbound datagram socket that waits at most [`SEND_WITHIN`], made while there is a target.
    fn make_socket(&self) -> Option<UnixDatagram> {
        if self.target == JournalTarget::Off {
            return None;
        }
        let socket = UnixDatagram::unbound().ok()?;
        socket.set_write_timeout(Some(SEND_WITHIN)).ok()?;
        Some(socket)
    }

    /// Sends one record; nothing is returned, because a record never fails for its journal copy.
    pub(super) fn send(&mut self, event: &str, writer: &str, run: &str, seq: u64, line: &str) {
        let JournalTarget::Socket(path) = &self.target else {
            return;
        };
        if self.socket.is_none() {
            self.socket = self.make_socket();
        }
        let Some(socket) = &self.socket else {
            return;
        };
        let message = format!("eitri remote {event}");
        let seq = seq.to_string();
        let datagram = encode(&[
            ("MESSAGE", message.as_bytes()),
            ("SYSLOG_IDENTIFIER", b"eitri-remote"),
            ("EITRI_EVENT", event.as_bytes()),
            ("EITRI_WRITER", writer.as_bytes()),
            ("EITRI_RUN", run.as_bytes()),
            ("EITRI_SEQ", seq.as_bytes()),
            ("EITRI_RECORD", line.as_bytes()),
        ]);
        let _ = socket.send_to(&datagram, path);
    }
}

/// One datagram holding `fields`.
pub(super) fn encode(fields: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, value) in fields {
        out.extend_from_slice(name.as_bytes());
        if value.contains(&b'\n') {
            out.push(b'\n');
            out.extend_from_slice(&(value.len() as u64).to_le_bytes());
            out.extend_from_slice(value);
        } else {
            out.push(b'=');
            out.extend_from_slice(value);
        }
        out.push(b'\n');
    }
    out
}

/// The fields of a datagram [`encode`] made, `None` for anything else.
#[cfg(test)]
pub(super) fn decode(mut bytes: &[u8]) -> Option<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::new();
    while !bytes.is_empty() {
        let end = bytes.iter().position(|b| *b == b'\n' || *b == b'=')?;
        let name = String::from_utf8(bytes[..end].to_vec()).ok()?;
        if bytes[end] == b'=' {
            let rest = &bytes[end + 1..];
            let stop = rest.iter().position(|b| *b == b'\n')?;
            out.push((name, rest[..stop].to_vec()));
            bytes = &rest[stop + 1..];
        } else {
            let rest = &bytes[end + 1..];
            let len = u64::from_le_bytes(rest.get(..8)?.try_into().ok()?) as usize;
            let value = rest.get(8..8 + len)?.to_vec();
            if rest.get(8 + len) != Some(&b'\n') {
                return None;
            }
            out.push((name, value));
            bytes = &rest[8 + len + 1..];
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::super::*;
    use super::*;

    fn field<'a>(fields: &'a [(String, Vec<u8>)], name: &str) -> &'a [u8] {
        let found: Vec<_> = fields.iter().filter(|(k, _)| k == name).collect();
        assert_eq!(found.len(), 1, "{name} once in {fields:?}");
        &found[0].1
    }

    #[test]
    fn journal_gets_each_record_with_its_fields() {
        let dir = audit_dir_for("audit-journal");
        let journal = FakeJournal::bind();
        let mut log = AuditLog::open(&dir, WriterKind::Serve, None, journal.target()).unwrap();
        log.record(cmd("d1", 1)).unwrap();
        log.record(Event::Refused {
            reason: Reason::BadToken,
            device: None,
            listener: Some(Listener::Loopback),
            peer: Some("127.0.0.1:4000".into()),
            key: None,
            kind: None,
        })
        .unwrap();
        let lines = lines(&dir);
        for (n, (line, event)) in lines.iter().zip(["cmd", "refused"]).enumerate() {
            let fields = journal.next();
            assert_eq!(field(&fields, "MESSAGE"), format!("eitri remote {event}").as_bytes());
            assert_eq!(field(&fields, "SYSLOG_IDENTIFIER"), b"eitri-remote");
            assert_eq!(field(&fields, "EITRI_EVENT"), event.as_bytes());
            assert_eq!(field(&fields, "EITRI_WRITER"), b"serve");
            assert_eq!(field(&fields, "EITRI_RUN"), log.run().as_bytes());
            assert_eq!(field(&fields, "EITRI_SEQ"), (n + 1).to_string().as_bytes());
            assert_eq!(
                field(&fields, "EITRI_RECORD"),
                line.as_bytes(),
                "the record is the file's line"
            );
        }
        assert!(journal.is_empty(), "one datagram per record");
    }

    #[test]
    fn a_value_with_a_newline_goes_in_the_binary_form() {
        let dir = audit_dir_for("audit-journal-nl");
        let journal = FakeJournal::bind();
        let mut log = AuditLog::open(&dir, WriterKind::Instance("a\nb".into()), None, journal.target()).unwrap();
        log.record(cmd("d1", 1)).unwrap();
        let fields = journal.next();
        assert_eq!(field(&fields, "EITRI_WRITER"), b"a\nb");
        let bytes = encode(&[("A", b"x\ny"), ("B", b"z")]);
        assert_eq!(bytes, b"A\n\x03\0\0\0\0\0\0\0x\ny\nB=z\n");
    }

    #[test]
    fn missing_journal_never_fails_a_record() {
        let dir = audit_dir_for("audit-journal-none");
        let nowhere = dir.parent().unwrap().join("no-such-socket");
        let mut log = AuditLog::open(&dir, WriterKind::Serve, None, JournalTarget::Socket(nowhere)).unwrap();
        log.record(cmd("d1", 1)).unwrap();
        log.record(cmd("d1", 2)).unwrap();
        assert_eq!(lines(&dir).len(), 2);
        // A journal that went away after the open is the same.
        let journal = FakeJournal::bind();
        let mut log = AuditLog::open(&dir, WriterKind::Serve, None, journal.target()).unwrap();
        drop(journal);
        log.record(cmd("d1", 3)).unwrap();
        assert_eq!(lines(&dir).len(), 3);
    }
}
