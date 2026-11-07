//! Rotation and pruning, both done by a writer under the log's lock.

use super::{chain, parse_time, read, rotated_name, AUDIT_RECORD_MAX, AUDIT_ROTATED_FILES, CURRENT};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{Duration, SystemTime};

/// Moves `.4` to `.5` (the old `.5` dropped), and so on down to the current file to `.1`.
pub(super) fn rotate(dir: &Path) -> io::Result<()> {
    for n in (1..AUDIT_ROTATED_FILES).rev() {
        rename_if_there(&dir.join(rotated_name(n)), &dir.join(rotated_name(n + 1)))?;
    }
    rename_if_there(&dir.join(CURRENT), &dir.join(rotated_name(1)))
}

fn rename_if_there(from: &Path, to: &Path) -> io::Result<()> {
    match std::fs::rename(from, to) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Deletes, oldest first, each rotated file whose newest record is older than `keep_days` before `now`, stopping
/// at the first that is not: a newer file is never removed while an older one stays, so what is left is still one
/// chain.
pub(super) fn prune(dir: &Path, keep_days: u32, now: SystemTime) -> io::Result<()> {
    let Some(cutoff) = now.checked_sub(Duration::from_secs(u64::from(keep_days) * 86_400)) else {
        return Ok(());
    };
    for n in (1..=AUDIT_ROTATED_FILES).rev() {
        let path = dir.join(rotated_name(n));
        match newest_time(&path)? {
            None if !path.exists() => continue,
            Some(newest) if newest < cutoff => match std::fs::remove_file(&path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            },
            // A file whose last line is no record cannot be dated, so it stays, and so does everything newer.
            _ => return Ok(()),
        }
    }
    Ok(())
}

/// The time of the last record of the file at `path`, `None` when it is missing or its last line is no record.
fn newest_time(path: &Path) -> io::Result<Option<SystemTime>> {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if !file.metadata()?.is_file() {
        return Ok(None);
    }
    let Some(line) = chain::last_line(&file, AUDIT_RECORD_MAX)? else {
        return Ok(None);
    };
    Ok(std::str::from_utf8(&line)
        .ok()
        .and_then(read::Record::parse)
        .and_then(|r| parse_time(&r.time)))
}

/// How many times [`snapshot`] reads the files again when a writer rotated them meanwhile.
const SNAPSHOT_TRIES: usize = 8;

/// The log's files read as one set, oldest first, as `(name, bytes)`, without the writers' lock: reading 60 MiB
/// under it could hold a writer past its wait. Each name's file is identified before and after the files are read,
/// and a rotation or a prune in between means they are read again. A file only ever moves to a higher number and
/// keeps its inode while it has a name, so a name that shows the same file before and after held it throughout;
/// for a rotated name the change time is compared too, which a rename moves, in case a freed inode came back. The
/// current file may grow meanwhile, which changes nothing that was read.
pub(super) fn snapshot(dir: &Path) -> io::Result<Vec<(String, Vec<u8>)>> {
    snapshot_with(dir, |_| {})
}

/// [`snapshot`], calling `after_read` with each file's name once that file is read.
fn snapshot_with(dir: &Path, mut after_read: impl FnMut(&str)) -> io::Result<Vec<(String, Vec<u8>)>> {
    let names: Vec<String> = (1..=AUDIT_ROTATED_FILES)
        .rev()
        .map(rotated_name)
        .chain(std::iter::once(CURRENT.to_string()))
        .collect();
    let identities = |dir: &Path| -> io::Result<Vec<Option<FileIdentity>>> {
        names
            .iter()
            .map(|name| identity(&dir.join(name), name != CURRENT))
            .collect()
    };
    for _ in 0..SNAPSHOT_TRIES {
        let before = identities(dir)?;
        let mut out = Vec::new();
        for (name, seen) in names.iter().zip(&before) {
            if seen.is_none() {
                continue;
            }
            if let Some(bytes) = read::read_file(&dir.join(name))? {
                out.push((name.clone(), bytes));
            }
            after_read(name);
        }
        if identities(dir)? == before {
            return Ok(out);
        }
    }
    Err(io::Error::other(format!(
        "{} was rotated {SNAPSHOT_TRIES} times while it was read",
        dir.join(CURRENT).display()
    )))
}

/// Which file a name held: device, inode and, for a rotated name, the change time.
type FileIdentity = (u64, u64, Option<(i64, i64)>);

fn identity(path: &Path, rotated: bool) -> io::Result<Option<FileIdentity>> {
    use std::os::unix::fs::MetadataExt;
    match path.symlink_metadata() {
        Ok(meta) => Ok(Some((
            meta.dev(),
            meta.ino(),
            rotated.then_some((meta.ctime(), meta.ctime_nsec())),
        ))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::super::*;

    fn names(dir: &Path) -> Vec<String> {
        files(dir).into_iter().map(|(name, _)| name).collect()
    }

    /// The lines of a snapshot, oldest first.
    fn snapshot_lines(snapshot: &[(String, Vec<u8>)]) -> Vec<String> {
        snapshot
            .iter()
            .flat_map(|(_, bytes)| std::str::from_utf8(bytes).unwrap().lines().map(str::to_string))
            .collect()
    }

    #[test]
    fn a_rotation_while_the_files_are_read_is_read_again() {
        let dir = audit_dir_for("audit-rot-snap");
        let mut log = AuditLog::open(&dir, WriterKind::Serve, None, JournalTarget::Off).unwrap();
        log.rotate_bytes = 1200;
        for n in 0..10 {
            log.record(cmd("d1", n)).unwrap();
        }
        assert!(dir.join(rotated_name(2)).exists(), "{:?}", names(&dir));
        let mut rotations = 0;
        let snapshot = super::snapshot_with(&dir, |_| {
            if rotations == 0 {
                // Another writer's record rotates the files once the oldest has been read.
                rotations += 1;
                log.rotate_bytes = 1;
                log.record(cmd("d1", 99)).unwrap();
            }
        })
        .unwrap();
        let got = snapshot_lines(&snapshot);
        assert_eq!(got, lines(&dir), "every record once, none skipped, none twice");
        assert_chained(&got);
        assert_eq!(rotations, 1);
    }

    #[test]
    fn a_snapshot_of_a_quiet_log_is_its_files() {
        let dir = audit_dir_for("audit-rot-snap-quiet");
        let mut log = AuditLog::open(&dir, WriterKind::Serve, None, JournalTarget::Off).unwrap();
        log.rotate_bytes = 1200;
        for n in 0..10 {
            log.record(cmd("d1", n)).unwrap();
        }
        let snapshot = super::snapshot(&dir).unwrap();
        let names_read: Vec<&str> = snapshot.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names_read, names(&dir));
        assert_eq!(snapshot_lines(&snapshot), lines(&dir));
    }

    #[test]
    fn rotation_under_a_long_lived_second_writer() {
        let dir = audit_dir_for("audit-rot-b");
        let mut b = AuditLog::open(&dir, WriterKind::Serve, None, JournalTarget::Off).unwrap();
        b.record(cmd("b", 0)).unwrap();
        let mut a = AuditLog::open(&dir, WriterKind::Instance("a".into()), None, JournalTarget::Off).unwrap();
        a.rotate_bytes = 1;
        a.record(cmd("a", 1)).unwrap();
        assert!(
            !dir.join(CURRENT).exists(),
            "a's record passed the size and rotated the file"
        );
        let rotated = files(&dir);
        let a_last = rotated.last().unwrap().1.last().unwrap().clone();
        b.record(cmd("b", 2)).unwrap();
        let current = std::fs::read_to_string(dir.join(CURRENT)).unwrap();
        let b_line = current.lines().next().unwrap();
        assert_eq!(current.lines().count(), 1, "b's record is in the new file");
        assert_eq!(object(b_line)["writer"], "serve");
        assert_eq!(object(b_line)["seq"], 2);
        assert_eq!(object(b_line)["prev"].as_str().unwrap(), hash_of(&a_last));
        let rotated_text = std::fs::read_to_string(dir.join(rotated_name(1))).unwrap();
        assert_eq!(rotated_text.lines().count(), 2, "nothing more reached the rotated file");
        assert_chained(&lines(&dir));
    }

    #[test]
    fn rotation_keeps_the_chain_across_files() {
        let dir = audit_dir_for("audit-rot-chain");
        let mut log = AuditLog::open(&dir, WriterKind::Serve, None, JournalTarget::Off).unwrap();
        log.rotate_bytes = 1200;
        // Four records fill a file: four rotated files and the current one, nothing dropped yet.
        for n in 0..18 {
            log.record(cmd("d1", n)).unwrap();
        }
        assert_eq!(files(&dir).len(), 5, "{:?}", names(&dir));
        let lines = lines(&dir);
        assert_chained(&lines);
        let seqs: Vec<u64> = lines.iter().map(|l| object(l)["seq"].as_u64().unwrap()).collect();
        assert_eq!(seqs, (1..=18).collect::<Vec<_>>());
        let report = verify(&dir, &Entries(Vec::new()), "/usr/bin/eitri").unwrap();
        assert!(report.chain_breaks.is_empty(), "{report:?}");
    }

    #[test]
    fn rotation_drops_files_older_than_keep_days() {
        let dir = audit_dir_for("audit-rot-keep");
        let clock = FakeClock::at_ms(days_ms(20_000));
        let mut log = open_at(&dir, WriterKind::Serve, Some(2), JournalTarget::Off, &clock);
        log.rotate_bytes = 1200;
        for n in 0..12 {
            log.record(cmd("old", n)).unwrap();
        }
        assert!(dir.join(rotated_name(2)).exists(), "{:?}", names(&dir));
        clock.set_ms(days_ms(20_001));
        for n in 0..6 {
            log.record(cmd("mid", n)).unwrap();
        }
        let before = names(&dir);
        assert!(
            lines(&dir).iter().any(|l| object(l)["device"] == "old"),
            "a day later nothing is dropped"
        );
        clock.set_ms(days_ms(20_005));
        for n in 0..6 {
            log.record(cmd("new", n)).unwrap();
        }
        let left = lines(&dir);
        let count = |device: &str| left.iter().filter(|l| object(l)["device"] == device).count();
        assert_eq!(
            (count("old"), count("mid"), count("new")),
            // Four records fill a file: the file of four `mid` records went with the `old` ones, and the two kept
            // share a file with newer records.
            (0, 2, 6),
            "files whose newest record is over two days old are gone: before {before:?}, after {:?}",
            names(&dir)
        );
        // What is left is contiguous: no rotated number is skipped.
        let rotated: Vec<u32> = (1..=AUDIT_ROTATED_FILES)
            .filter(|n| dir.join(rotated_name(*n)).exists())
            .collect();
        assert_eq!(rotated, (1..=rotated.len() as u32).collect::<Vec<_>>());
        assert_chained(&left);
    }

    #[test]
    fn opening_with_keep_days_prunes() {
        let dir = audit_dir_for("audit-rot-open");
        let clock = FakeClock::at_ms(days_ms(20_000));
        let mut log = open_at(&dir, WriterKind::Serve, None, JournalTarget::Off, &clock);
        log.rotate_bytes = 1;
        log.record(cmd("old", 0)).unwrap();
        assert!(dir.join(rotated_name(1)).exists());
        clock.set_ms(days_ms(20_100));
        let _windows = open_at(&dir, WriterKind::Instance("w".into()), None, JournalTarget::Off, &clock);
        assert!(
            dir.join(rotated_name(1)).exists(),
            "a writer without keep_days removes nothing"
        );
        let _serve = open_at(&dir, WriterKind::Serve, Some(90), JournalTarget::Off, &clock);
        assert!(!dir.join(rotated_name(1)).exists(), "serve prunes when it opens");
    }

    #[test]
    fn at_most_five_rotated_files() {
        let dir = audit_dir_for("audit-rot-five");
        let mut log = AuditLog::open(&dir, WriterKind::Serve, None, JournalTarget::Off).unwrap();
        log.rotate_bytes = 1;
        for n in 0..20 {
            log.record(cmd("d1", n)).unwrap();
        }
        assert_eq!(
            names(&dir),
            (1..=5).rev().map(rotated_name).collect::<Vec<_>>(),
            "the oldest is dropped"
        );
        assert!(!dir.join(rotated_name(6)).exists());
        let seqs: Vec<u64> = lines(&dir).iter().map(|l| object(l)["seq"].as_u64().unwrap()).collect();
        assert_eq!(seqs, (16..=20).collect::<Vec<_>>());
        assert_chained(&lines(&dir));
    }
}
