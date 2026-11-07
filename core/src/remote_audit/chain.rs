//! The chain: each line's `prev` is the SHA-256 of the line before it, read under the lock from the end of the
//! current file (or of `audit.jsonl.1` when the current one is empty), and the line itself, cut to fit.

use super::{Fields, AUDIT_RECORD_MAX, AUDIT_V, CURRENT, ELLIPSIS};
use agent::private_fs::{euid, PRIVATE_FILE_MODE};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// The `prev` of the first record ever.
pub(super) const ZERO_PREV: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// How much is read at a time when looking for the start of the last line.
const TAIL_CHUNK: u64 = 8192;

/// `bytes` as lowercase hex digits.
pub(super) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// The `prev` a line's successor carries: the SHA-256 of its bytes without the newline.
pub(super) fn line_hash(line: &[u8]) -> String {
    hex(&Sha256::digest(line))
}

/// What every record starts with.
pub(super) struct Header<'a> {
    pub(super) time: String,
    pub(super) writer: &'a str,
    pub(super) run: &'a str,
    pub(super) seq: u64,
    pub(super) event: &'static str,
    pub(super) prev: &'a str,
}

/// The record as one line without its newline, at most [`AUDIT_RECORD_MAX`] bytes with it: while it is longer, the
/// string field longest as written is cut at a character boundary by as many written bytes as the line is over,
/// and ends with `…`. Each cut makes one string strictly shorter, so this ends; only a header that alone is too long
/// fails.
pub(super) fn render_within(header: &Header<'_>, mut fields: Fields) -> io::Result<String> {
    loop {
        let line = render(header, &fields);
        let Some(over) = (line.len() + 1).checked_sub(AUDIT_RECORD_MAX).filter(|n| *n > 0) else {
            return Ok(line);
        };
        let longest = fields
            .iter()
            .enumerate()
            .filter_map(|(i, (_, v))| v.as_str().map(|s| (i, written_len(s))))
            .filter(|(i, _)| fields[*i].1.as_str() != Some(ELLIPSIS))
            .max_by_key(|(_, len)| *len);
        let Some((i, len)) = longest else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("an audit record of {} bytes with nothing left to cut", line.len()),
            ));
        };
        let text = fields[i].1.as_str().unwrap_or_default();
        let budget = len.saturating_sub(over + ELLIPSIS.len());
        let mut keep = 0;
        let mut written = 0;
        for (at, c) in text.char_indices() {
            written += written_char_len(c);
            if written > budget {
                break;
            }
            keep = at + c.len_utf8();
        }
        fields[i].1 = Value::String(format!("{}{ELLIPSIS}", &text[..keep]));
    }
}

/// How many bytes `text` takes inside a JSON string as `serde_json` writes it.
fn written_len(text: &str) -> usize {
    text.chars().map(written_char_len).sum()
}

/// How many bytes one character takes inside a JSON string: quotes, backslashes and the five short escapes take
/// two, other control characters six (`\u00XX`), the rest their UTF-8 bytes.
fn written_char_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2,
        c if (c as u32) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

/// The header, then the fields in their order. Written by hand, not through a map, so the order is the same
/// whatever `serde_json`'s features are.
fn render(header: &Header<'_>, fields: &Fields) -> String {
    let string = |s: &str| Value::from(s).to_string();
    let mut line = format!(
        "{{\"v\":{AUDIT_V},\"time\":{},\"writer\":{},\"run\":{},\"seq\":{},\"event\":{},\"prev\":{}",
        string(&header.time),
        string(header.writer),
        string(header.run),
        header.seq,
        string(header.event),
        string(header.prev),
    );
    for (key, value) in fields {
        line.push(',');
        line.push_str(&string(key));
        line.push(':');
        line.push_str(&value.to_string());
    }
    line.push('}');
    line
}

/// `audit.jsonl` in `dir`, opened to append for one record and created 0600 when missing. A symlink in its place
/// is refused (`O_NOFOLLOW`), a FIFO cannot hold the open (`O_NONBLOCK`), and anything but a regular file of this
/// user is refused; a file someone widened is set back to 0600 on the handle.
pub(super) fn open_current(dir: &Path) -> io::Result<File> {
    let path = dir.join(CURRENT);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .mode(PRIVATE_FILE_MODE)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(&path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.uid() != euid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not a regular file of this user", path.display()),
        ));
    }
    if meta.mode() & 0o777 != PRIVATE_FILE_MODE {
        file.set_permissions(std::fs::Permissions::from_mode(PRIVATE_FILE_MODE))?;
    }
    Ok(file)
}

/// The end of a file: the hash of its last line, and whether that line has its newline.
pub(super) struct Tail {
    pub(super) hash: String,
    pub(super) closed: bool,
}

/// Where the last line of `file` starts and ends (its newline left out), `None` when the file is empty. Found by
/// reading backwards a chunk at a time, so a long line costs reads, not memory.
fn last_line_span(file: &File) -> io::Result<Option<(u64, u64, bool)>> {
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(None);
    }
    let mut last = [0u8; 1];
    file.read_exact_at(&mut last, len - 1)?;
    let closed = last[0] == b'\n';
    let end = if closed { len - 1 } else { len };
    let mut start = end;
    let mut chunk = vec![0u8; TAIL_CHUNK as usize];
    while start > 0 {
        let step = TAIL_CHUNK.min(start);
        let buf = &mut chunk[..step as usize];
        file.read_exact_at(buf, start - step)?;
        if let Some(i) = buf.iter().rposition(|b| *b == b'\n') {
            start = start - step + i as u64 + 1;
            return Ok(Some((start, end, closed)));
        }
        start -= step;
    }
    Ok(Some((0, end, closed)))
}

/// The tail of `file`, `None` when it is empty.
pub(super) fn tail(file: &File) -> io::Result<Option<Tail>> {
    let Some((start, end, closed)) = last_line_span(file)? else {
        return Ok(None);
    };
    let mut hasher = Sha256::new();
    let mut chunk = vec![0u8; TAIL_CHUNK as usize];
    let mut at = start;
    while at < end {
        let step = TAIL_CHUNK.min(end - at) as usize;
        file.read_exact_at(&mut chunk[..step], at)?;
        hasher.update(&chunk[..step]);
        at += step as u64;
    }
    Ok(Some(Tail {
        hash: hex(&hasher.finalize()),
        closed,
    }))
}

/// The `prev` for a record going into an empty current file: the hash of `audit.jsonl.1`'s last line, or
/// [`ZERO_PREV`] when there is none.
pub(super) fn previous_file_hash(dir: &Path) -> io::Result<String> {
    let path = dir.join(super::rotated_name(1));
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(&path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(ZERO_PREV.to_string()),
        Err(e) => return Err(e),
    };
    let meta = file.metadata()?;
    if !meta.is_file() || meta.uid() != euid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not a regular file of this user", path.display()),
        ));
    }
    Ok(tail(&file)?.map_or_else(|| ZERO_PREV.to_string(), |t| t.hash))
}

/// Writes `bytes` with one `write(2)`. Should the kernel take only part of it, the rest follows at once: the lock
/// is held, so no other writer's line can come between the two.
pub(super) fn append(file: &mut File, bytes: &[u8]) -> io::Result<()> {
    let n = file.write(bytes)?;
    if n < bytes.len() {
        file.write_all(&bytes[n..])?;
    }
    Ok(())
}

/// The last line of `file` without its newline when it is at most `cap` bytes.
pub(super) fn last_line(file: &File, cap: usize) -> io::Result<Option<Vec<u8>>> {
    let Some((start, end, _)) = last_line_span(file)? else {
        return Ok(None);
    };
    if end - start > cap as u64 {
        return Ok(None);
    }
    let mut line = vec![0u8; (end - start) as usize];
    file.read_exact_at(&mut line, start)?;
    Ok(Some(line))
}

/// The lines of a file's bytes; a final newline ends the last line rather than starting an empty one.
pub(super) fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let body = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    body.split(|b| *b == b'\n').collect()
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::super::*;
    use super::*;

    #[test]
    fn two_writers_keep_one_chain() {
        let dir = audit_dir_for("audit-two");
        let writers: Vec<_> = ["serve", "i-1"]
            .into_iter()
            .map(|name| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    let kind = if name == "serve" {
                        WriterKind::Serve
                    } else {
                        WriterKind::Instance(name.into())
                    };
                    let mut log = AuditLog::open(&dir, kind, None, JournalTarget::Off).unwrap();
                    // Both rotate while the other writes: about 220 records fill a file, so the 1000 span five
                    // files and none is dropped.
                    log.rotate_bytes = 70_000;
                    for n in 0..500 {
                        log.record(cmd(name, n)).unwrap();
                    }
                })
            })
            .collect();
        for w in writers {
            w.join().unwrap();
        }
        assert!(files(&dir).len() >= 4, "the writers rotated as they went");
        let lines = lines(&dir);
        assert_eq!(lines.len(), 1000);
        assert_eq!(object(&lines[0])["prev"], ZERO_PREV);
        assert_chained(&lines);
        for name in ["serve", "i-1"] {
            let seqs: Vec<u64> = lines
                .iter()
                .map(|l| object(l))
                .filter(|o| o["writer"] == name)
                .map(|o| o["seq"].as_u64().unwrap())
                .collect();
            assert_eq!(
                seqs,
                (1..=500).collect::<Vec<_>>(),
                "{name}'s records, whole and in order"
            );
        }
    }

    #[test]
    fn a_torn_tail_is_closed_before_the_next_line() {
        let dir = audit_dir_for("audit-torn");
        let mut log = AuditLog::open(&dir, WriterKind::Serve, None, JournalTarget::Off).unwrap();
        log.record(cmd("d1", 1)).unwrap();
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join(CURRENT))
            .unwrap();
        f.write_all(br#"{"v":1,"time":"20"#).unwrap();
        drop(f);
        log.record(cmd("d1", 2)).unwrap();
        let lines = lines(&dir);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1], r#"{"v":1,"time":"20"#, "the fragment stays a line of its own");
        assert_eq!(object(&lines[2])["prev"].as_str().unwrap(), hash_of(&lines[1]));
        assert_eq!(object(&lines[2])["seq"], 2);
    }

    #[test]
    fn the_tail_of_a_long_last_line_is_read_whole() {
        let dir = audit_dir_for("audit-tail");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f");
        let long = "x".repeat(3 * TAIL_CHUNK as usize + 17);
        std::fs::write(&path, format!("first\n{long}\n")).unwrap();
        let file = File::open(&path).unwrap();
        let end = tail(&file).unwrap().unwrap();
        assert!(end.closed);
        assert_eq!(end.hash, line_hash(long.as_bytes()));
        assert_eq!(last_line(&file, 16).unwrap(), None, "over the cap");
        std::fs::write(&path, "only").unwrap();
        let file = File::open(&path).unwrap();
        let end = tail(&file).unwrap().unwrap();
        assert!(!end.closed);
        assert_eq!(end.hash, line_hash(b"only"));
        assert_eq!(last_line(&file, 16).unwrap(), Some(b"only".to_vec()));
    }

    #[test]
    fn lines_split_on_newlines() {
        assert_eq!(split_lines(b""), Vec::<&[u8]>::new());
        assert_eq!(split_lines(b"a\nb\n"), vec![&b"a"[..], &b"b"[..]]);
        assert_eq!(split_lines(b"a\n\nb"), vec![&b"a"[..], &b""[..], &b"b"[..]]);
    }
}
