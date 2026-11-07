//! `verify`: the chain of the retained files, and the file against the journal's copy.
//!
//! The journal is the trail a same-uid process cannot rewrite; its `_EXE` is set by the journal itself, so only
//! entries sent by the Eitri binary count as records. A window still running across a package upgrade sends from a
//! binary the journal names `<path> (deleted)`, which is still ours. A process that runs code inside the Eitri
//! binary (`LD_PRELOAD`, `ptrace`) can send entries with that `_EXE` too; nothing here can tell those apart.

use super::{chain, rotate, Record};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime};

/// How long before the journal's reading began a record may be dated and still be taken as written while the check
/// ran, before its journal copy could be read back (see [`verify_from`]).
pub const WRITTEN_DURING_CHECK: Duration = Duration::from_secs(10);

/// How far past the files' reading a record may be dated: every record read was stamped before it, so this only
/// allows for the millisecond the time is written in.
const READ_SLACK: Duration = Duration::from_secs(1);

/// One journal entry of `SYSLOG_IDENTIFIER=eitri-remote`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalEntry {
    /// `_EXE`, set by the journal.
    pub exe: Option<String>,
    /// `EITRI_RECORD`.
    pub record: Option<String>,
}

/// Where the journal's entries come from (`journalctl` in the CLI; a fixture in tests).
pub trait JournalSource {
    fn entries(&self) -> io::Result<Vec<JournalEntry>>;
}

/// What [`verify`] found; all empty and zero on a log nobody touched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifyReport {
    /// `(file, line number from 1)` of each line whose `prev` is not the hash of the line before it, or that is not
    /// a record.
    pub chain_breaks: Vec<(String, u64)>,
    /// Records the journal has that no retained file holds, as the journal has them.
    pub missing_from_file: Vec<String>,
    /// Records the files hold whose `(writer, run, seq)` none of the journal's records has, as the file has them:
    /// lines no Eitri writer sent the journal, or whose copy the journal dropped.
    pub missing_from_journal: Vec<String>,
    /// Records whose `(writer, run, seq)` the file holds with other bytes, as the journal has them.
    pub altered: Vec<String>,
    /// Entries not sent by the Eitri binary, or that are not records.
    pub forged: usize,
    /// Those entries by the `_EXE` the journal gave them (empty for an entry without one), so another Eitri build of
    /// the user's own can be told apart from a stranger.
    pub forged_senders: BTreeMap<String, usize>,
    /// `("<writer> <run>", seq, next seq)` where a run's journal copies skip numbers: messages the journal dropped.
    pub journal_gaps: Vec<(String, u64, u64)>,
    /// Not findings: records only the files hold that come before every record the journal holds, while the journal
    /// holds none of the log's older ones, so the journal may no longer reach back that far (vacuumed, or a journal
    /// that is not persistent, since a reboot). Not checked against the journal.
    pub older_than_journal: usize,
    /// Not findings: records only the files hold dated within [`WRITTEN_DURING_CHECK`] of the check: taken as
    /// written while it ran (two writers' journal copies may arrive in the other order than their lines, so where in
    /// the files does not matter). Not checked against the journal.
    pub written_during_check: usize,
    /// Not a finding, but nothing was checked: the files hold records and the journal holds none of the log's (a
    /// journal with no files, one that keeps none of these entries, a volatile one after a reboot before any new
    /// record, or the check run as another user). Every record is then in `older_than_journal`.
    pub journal_holds_none: bool,
}

impl VerifyReport {
    /// Whether nothing was found (the two counts of records not checked are not findings).
    pub fn is_clean(&self) -> bool {
        VerifyReport {
            older_than_journal: 0,
            written_during_check: 0,
            journal_holds_none: false,
            ..self.clone()
        } == VerifyReport::default()
    }
}

/// Checks the log in `dir` against `journal`, whose entries count as ours when their `_EXE` is `our_exe`.
///
/// Nothing the file says is taken on trust to excuse a record the journal has. The oldest retained record's `prev`
/// is not checked against a line before it, since that line may have been pruned. What may have been pruned is
/// worked out from the *anchor*, the oldest retained record the journal holds byte for byte, so its time and `prev`
/// are the journal's own: its predecessors, followed back through the journal's copies of the chain (which tells
/// apart records of the anchor's millisecond), and the journal records older than it. A record whose
/// `(writer, run, seq)` some retained line holds is never excused, since a pruned record is not also kept: other
/// bytes there are `altered`. With no anchor nothing is excused. A journal gap is reported unless the record after
/// it was excused, so a period long pruned from the files does not keep the report from being clean.
///
/// What this cannot tell from pruning: whole records deleted from before the anchor, with every line after them
/// left as it was.
///
/// The other way, a record the files hold under a `(writer, run, seq)` none of the journal's records has is
/// `missing_from_journal`: every writer sends each record to the journal as it writes it, and the journal drops its
/// oldest entries first, so once it holds one of the files' records (or one older than them) it holds every later
/// one, unless it dropped a message (a gap) or a send failed. Two kinds are not reported, only counted: records
/// before the anchor (all of them, with no anchor) while the journal holds no record of the log from before the
/// anchor that the files no longer hold (none at all, with no anchor), which a journal that no longer reaches back
/// that far lacks (`older_than_journal`; a record the files still hold that is merely dated before the anchor, a
/// clock set back, does not count); and records dated from [`WRITTEN_DURING_CHECK`] before `started` to a second
/// after the files were read, which a writer appended while the check ran (`written_during_check`; anywhere in the
/// files, since a writer appends its line and only then sends its copy, so another writer's later copy can arrive
/// first). A record before the anchor cannot have been added later, since the
/// anchor's `prev` pins every line before it through the chain.
pub fn verify(dir: &Path, journal: &dyn JournalSource, our_exe: &str) -> io::Result<VerifyReport> {
    verify_from(dir, journal, our_exe, SystemTime::now())
}

/// [`verify`], with `started` the time the journal's reading began (a caller that read it already says when).
pub fn verify_from(
    dir: &Path,
    journal: &dyn JournalSource,
    our_exe: &str,
    started: SystemTime,
) -> io::Result<VerifyReport> {
    // The journal first: a record written while this runs is then in the file but not yet in what was read of the
    // journal, which is excused at the files' end within a bounded window, rather than the other way round, which
    // would be reported as missing from the file.
    let entries = journal.entries()?;
    let mut report = VerifyReport::default();

    // The chain, across the files oldest first.
    let mut retained: Vec<Record> = Vec::new();
    let mut before: Option<String> = None;
    for (name, bytes) in rotate::snapshot(dir)? {
        for (n, line) in chain::split_lines(&bytes).into_iter().enumerate() {
            let record = std::str::from_utf8(line).ok().and_then(Record::parse);
            let linked = match (&record, &before) {
                (Some(record), Some(hash)) => record.prev == *hash,
                (Some(_), None) => true,
                (None, _) => false,
            };
            if !linked {
                report.chain_breaks.push((name.clone(), n as u64 + 1));
            }
            before = Some(chain::line_hash(line));
            retained.extend(record);
        }
    }
    let read_at = SystemTime::now().max(started);
    // Every line the files hold under each key: a key held twice with different bytes is an edit too.
    let mut held: HashMap<Key<'_>, Vec<&str>> = HashMap::new();
    for record in &retained {
        held.entry(key(record)).or_default().push(record.line.as_str());
    }

    // The journal's records, ours apart from the rest.
    let ours_exe = |exe: &Option<String>| {
        exe.as_deref()
            .is_some_and(|exe| exe == our_exe || exe.strip_suffix(" (deleted)") == Some(our_exe))
    };
    let mut ours = Vec::new();
    for entry in &entries {
        match entry.record.as_deref().and_then(Record::parse) {
            Some(record) if ours_exe(&entry.exe) => ours.push(record),
            _ => {
                report.forged += 1;
                let sender = entry.exe.clone().unwrap_or_default();
                *report.forged_senders.entry(sender).or_default() += 1;
            }
        }
    }
    let in_journal: HashSet<&str> = ours.iter().map(|r| r.line.as_str()).collect();

    // What may have been pruned, from the anchor.
    let mut before_anchor: HashSet<&str> = HashSet::new();
    let anchor_at = retained.iter().position(|r| in_journal.contains(r.line.as_str()));
    if let Some(anchor) = anchor_at.map(|i| &retained[i]) {
        let by_hash: HashMap<String, &Record> = ours.iter().map(|r| (chain::line_hash(r.line.as_bytes()), r)).collect();
        let mut at = anchor.prev.clone();
        while let Some(record) = by_hash.get(&at) {
            if !before_anchor.insert(record.line.as_str()) {
                break;
            }
            at = record.prev.clone();
        }
        if let Some(anchor_at) = anchor.at() {
            before_anchor.extend(
                ours.iter()
                    .filter(|r| r.at().is_some_and(|at| at < anchor_at))
                    .map(|r| r.line.as_str()),
            );
        }
    }
    let pruned = |record: &Record| before_anchor.contains(record.line.as_str()) && !held.contains_key(&key(record));

    let mut seqs: BTreeMap<(String, String), BTreeMap<u64, bool>> = BTreeMap::new();
    let mut seen = HashSet::new();
    for record in &ours {
        let excused = pruned(record);
        seqs.entry((record.writer.clone(), record.run.clone()))
            .or_default()
            .entry(record.seq)
            .and_modify(|e| *e &= excused)
            .or_insert(excused);
        if !seen.insert(record.line.as_str()) || excused {
            continue;
        }
        match held.get(&key(record)) {
            None => report.missing_from_file.push(record.line.clone()),
            Some(lines) if lines.iter().any(|line| *line != record.line) => report.altered.push(record.line.clone()),
            Some(_) => {}
        }
    }

    // The files' records the journal has no copy of under their key.
    let journal_keys: HashSet<Key<'_>> = ours.iter().map(key).collect();
    // The journal reaches back past the anchor only through a record of the log the files no longer hold: one the
    // files still hold that is merely dated before the anchor (a clock set back) says nothing about what was
    // vacuumed.
    let journal_reaches_back = match anchor_at {
        Some(_) => ours.iter().any(pruned),
        None => !ours.is_empty(),
    };
    let just_written = |record: &Record| {
        record.at().is_some_and(|at| {
            at >= started.checked_sub(WRITTEN_DURING_CHECK).unwrap_or(started)
                && at <= read_at.checked_add(READ_SLACK).unwrap_or(read_at)
        })
    };
    for (i, record) in retained.iter().enumerate() {
        if journal_keys.contains(&key(record)) {
            continue;
        }
        if anchor_at.is_none_or(|anchor| i < anchor) && !journal_reaches_back {
            report.older_than_journal += 1;
        } else if just_written(record) {
            report.written_during_check += 1;
        } else {
            report.missing_from_journal.push(record.line.clone());
        }
    }

    report.journal_holds_none = ours.is_empty() && !retained.is_empty();

    for ((writer, run), seqs) in seqs {
        let seqs: Vec<(u64, bool)> = seqs.into_iter().collect();
        for pair in seqs.windows(2) {
            let ((from, _), (to, to_excused)) = (pair[0], pair[1]);
            if to - from > 1 && !to_excused {
                report.journal_gaps.push((format!("{writer} {run}"), from, to));
            }
        }
    }
    Ok(report)
}

/// A record's `(writer, run, seq)`, which no two records share.
type Key<'a> = (&'a str, &'a str, u64);

fn key(record: &Record) -> Key<'_> {
    (record.writer.as_str(), record.run.as_str(), record.seq)
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::super::*;
    use super::*;

    const EXE: &str = "/usr/bin/eitri";

    /// `n` records through a fake journal; the journal's entries as they arrived.
    fn written(dir: &Path, n: u64) -> Vec<JournalEntry> {
        let journal = FakeJournal::bind();
        let mut log = AuditLog::open(dir, WriterKind::Serve, None, journal.target()).unwrap();
        (0..n)
            .map(|i| {
                log.record(cmd("d1", i)).unwrap();
                journal.next_entry(EXE)
            })
            .collect()
    }

    /// `lines` with each `prev` set to the hash of the line before it, the first one's to `first_prev`: the chain an
    /// editor of the file would rebuild.
    fn rechain(mut lines: Vec<String>, first_prev: &str) -> Vec<String> {
        for i in 0..lines.len() {
            let want = if i == 0 {
                first_prev.to_string()
            } else {
                hash_of(&lines[i - 1])
            };
            let have = object(&lines[i])["prev"].as_str().unwrap().to_string();
            lines[i] = lines[i].replace(&format!(r#""prev":"{have}""#), &format!(r#""prev":"{want}""#));
        }
        lines
    }

    fn write_lines(dir: &Path, lines: &[String]) {
        std::fs::write(dir.join(CURRENT), lines.join("\n") + "\n").unwrap();
    }

    fn record_line(entry: &JournalEntry) -> String {
        entry.record.clone().unwrap()
    }

    #[test]
    fn a_record_the_file_refuses_still_reaches_the_journal_and_is_named() {
        let dir = audit_dir_for("audit-verify-file-refused");
        let journal = FakeJournal::bind();
        let mut log = AuditLog::open(&dir, WriterKind::Serve, None, journal.target()).unwrap();
        log.record(cmd("d1", 0)).unwrap();
        let mut entries = vec![journal.next_entry(EXE)];
        // A link in the file's place, which the writer never follows.
        let current = dir.join(CURRENT);
        let kept = std::fs::read(&current).unwrap();
        std::fs::remove_file(&current).unwrap();
        std::os::unix::fs::symlink(dir.join("elsewhere"), &current).unwrap();
        assert!(
            log.record(cmd("d1", 1)).is_err(),
            "the file took a record through a link"
        );
        entries.push(journal.next_entry(EXE));
        // The file put back as it was, and the writer goes on.
        std::fs::remove_file(&current).unwrap();
        agent::private_fs::write_private(&current, &kept).unwrap();
        log.record(cmd("d1", 2)).unwrap();
        entries.push(journal.next_entry(EXE));
        let seqs: Vec<u64> = entries
            .iter()
            .map(|e| Record::parse(&record_line(e)).unwrap().seq)
            .collect();
        assert_eq!(seqs, [1, 2, 3], "the refused record keeps its number");
        let report = verify(&dir, &Entries(entries.clone()), EXE).unwrap();
        assert_eq!(report.missing_from_file, vec![record_line(&entries[1])], "{report:?}");
        assert!(
            report.chain_breaks.is_empty() && report.altered.is_empty() && report.journal_gaps.is_empty(),
            "{report:?}"
        );
    }

    #[test]
    fn a_first_line_pointing_at_the_newest_record_hides_nothing() {
        let dir = audit_dir_for("audit-verify-fake-prev");
        let entries = written(&dir, 6);
        let original = lines(&dir);
        let mut tampered = original.clone();
        tampered.remove(3);
        tampered[1] = tampered[1].replace(r#""device":"d1""#, r#""device":"d9""#);
        // The rewritten chain claims the newest journal record as the first line's predecessor, so following the
        // journal back from it would reach every record.
        let tampered = rechain(tampered, &hash_of(&record_line(entries.last().unwrap())));
        write_lines(&dir, &tampered);
        let report = verify(&dir, &Entries(entries), EXE).unwrap();
        assert!(report.chain_breaks.is_empty(), "{report:?}");
        assert_eq!(report.missing_from_file, vec![original[3].clone()], "{report:?}");
        let altered: Vec<String> = [0, 1, 2, 4, 5].iter().map(|i| original[*i].clone()).collect();
        assert_eq!(report.altered, altered, "every rewritten line: {report:?}");
    }

    #[test]
    fn a_first_line_dated_in_the_future_hides_nothing() {
        let dir = audit_dir_for("audit-verify-fake-time");
        let entries = written(&dir, 6);
        let original = lines(&dir);
        let mut tampered = original.clone();
        tampered.remove(3);
        tampered[1] = tampered[1].replace(r#""device":"d1""#, r#""device":"d9""#);
        let time = object(&tampered[0])["time"].as_str().unwrap().to_string();
        tampered[0] = tampered[0].replace(&time, "2099-01-01T00:00:00.000Z");
        let tampered = rechain(tampered, chain::ZERO_PREV);
        write_lines(&dir, &tampered);
        let report = verify(&dir, &Entries(entries), EXE).unwrap();
        assert!(report.chain_breaks.is_empty(), "{report:?}");
        assert_eq!(report.missing_from_file, vec![original[3].clone()], "{report:?}");
        let altered: Vec<String> = [0, 1, 2, 4, 5].iter().map(|i| original[*i].clone()).collect();
        assert_eq!(report.altered, altered, "{report:?}");
    }

    #[test]
    fn a_made_up_first_record_hides_nothing() {
        let dir = audit_dir_for("audit-verify-fake-first");
        let entries = written(&dir, 6);
        let original = lines(&dir);
        let mut tampered = original.clone();
        tampered.remove(1);
        let time = object(&original[0])["time"].as_str().unwrap().to_string();
        let made_up = original[0]
            .replace(r#""seq":1,"#, r#""seq":60,"#)
            .replace(&time, "2099-01-01T00:00:00.000Z");
        assert_ne!(made_up, original[0]);
        tampered.insert(0, made_up);
        let tampered = rechain(tampered, &hash_of(&record_line(entries.last().unwrap())));
        write_lines(&dir, &tampered);
        let report = verify(&dir, &Entries(entries), EXE).unwrap();
        assert!(report.chain_breaks.is_empty(), "{report:?}");
        assert_eq!(report.missing_from_file, vec![original[1].clone()], "{report:?}");
        let altered: Vec<String> = [0, 2, 3, 4, 5].iter().map(|i| original[*i].clone()).collect();
        assert_eq!(report.altered, altered, "{report:?}");
    }

    #[test]
    fn an_edited_last_record_claiming_its_own_original_as_prev_is_altered() {
        let dir = audit_dir_for("audit-verify-fake-last");
        let entries = written(&dir, 6);
        let original = lines(&dir);
        let have = object(&original[5])["prev"].as_str().unwrap().to_string();
        let edited = original[5]
            .replace(r#""device":"d1""#, r#""device":"d9""#)
            .replace(&have, &hash_of(&original[5]));
        assert_ne!(edited, original[5]);
        write_lines(&dir, &[edited]);
        let report = verify(&dir, &Entries(entries), EXE).unwrap();
        assert_eq!(report.altered, vec![original[5].clone()], "{report:?}");
        assert_eq!(report.missing_from_file, original[..5].to_vec(), "{report:?}");
    }

    #[test]
    fn a_gap_among_pruned_records_is_not_reported() {
        let dir = audit_dir_for("audit-verify-old-gap");
        let journal = FakeJournal::bind();
        let clock = FakeClock::at_ms(days_ms(20_000));
        let mut log = open_at(&dir, WriterKind::Serve, None, journal.target(), &clock);
        log.rotate_bytes = 1;
        let mut entries = Vec::new();
        for n in 0..20 {
            clock.set_ms(days_ms(20_000) + n * 1000);
            log.record(cmd("d1", n)).unwrap();
            entries.push(journal.next_entry(EXE));
        }
        let retained = lines(&dir);
        assert_eq!(object(&retained[0])["seq"], 16, "the oldest records were dropped");
        let run = object(&record_line(&entries[0]))["run"].as_str().unwrap().to_string();
        // The journal lost seq 2, long since dropped from the files, and seq 18, still in them.
        entries.remove(17);
        entries.remove(1);
        let report = verify(&dir, &Entries(entries), EXE).unwrap();
        assert_eq!(
            report.journal_gaps,
            vec![(format!("serve {run}"), 17, 19)],
            "{report:?}"
        );
        assert!(
            report.missing_from_file.is_empty() && report.altered.is_empty(),
            "{report:?}"
        );
        // The record the journal dropped is still in the files, which say so too.
        assert_eq!(report.missing_from_journal, vec![retained[2].clone()], "{report:?}");
        assert!(report.chain_breaks.is_empty(), "{report:?}");
    }

    /// `template` as a record of `seq` chained after `after`: what a same-uid process appends to the file.
    fn forged_after(template: &str, seq: u64, after: &str) -> String {
        let have_seq = object(template)["seq"].as_u64().unwrap();
        let have_prev = object(template)["prev"].as_str().unwrap().to_string();
        let line = template
            .replace(&format!(r#""seq":{have_seq},"#), &format!(r#""seq":{seq},"#))
            .replace(
                &format!(r#""prev":"{have_prev}""#),
                &format!(r#""prev":"{}""#, hash_of(after)),
            )
            .replace(r#""device":"d1""#, r#""device":"d9""#);
        assert_ne!(line, template);
        line
    }

    /// `n` records on a fake clock one second apart from `from_ms`, and their journal entries.
    fn written_at(dir: &Path, n: u64, from_ms: u64) -> Vec<JournalEntry> {
        let journal = FakeJournal::bind();
        let clock = FakeClock::at_ms(from_ms);
        let mut log = open_at(dir, WriterKind::Serve, None, journal.target(), &clock);
        (0..n)
            .map(|i| {
                clock.set_ms(from_ms + i * 1000);
                log.record(cmd("d1", i)).unwrap();
                journal.next_entry(EXE)
            })
            .collect()
    }

    fn ms(at_ms: u64) -> SystemTime {
        std::time::UNIX_EPOCH + Duration::from_millis(at_ms)
    }

    #[test]
    fn a_record_appended_after_disable_is_missing_from_the_journal() {
        let dir = audit_dir_for("audit-verify-forged-tail");
        let journal = FakeJournal::bind();
        let clock = FakeClock::at_ms(days_ms(20_000));
        let mut log = open_at(&dir, WriterKind::Serve, None, journal.target(), &clock);
        let mut entries = Vec::new();
        for n in 0..3 {
            log.record(cmd("d1", n)).unwrap();
            entries.push(journal.next_entry(EXE));
        }
        log.record(Event::Disable { why: "stopped".into() }).unwrap();
        entries.push(journal.next_entry(EXE));
        drop(log);
        let original = lines(&dir);
        // A command no phone sent, with the next free number of the run that ended and its `prev` the `disable`, so
        // the chain holds and no key the journal has is reused; and one of a run that never was.
        let forged = forged_after(&original[1], 5, &original[3]);
        let other_run = object(&original[1])["run"].as_str().unwrap().to_string();
        let second = forged_after(&original[1], 1, &forged).replace(&other_run, "0123456789abcdef");
        std::fs::write(
            dir.join(CURRENT),
            [&original[..], &[forged.clone(), second.clone()]].concat().join("\n") + "\n",
        )
        .unwrap();
        let report = verify(&dir, &Entries(entries), EXE).unwrap();
        assert!(report.chain_breaks.is_empty(), "{report:?}");
        assert_eq!(report.missing_from_journal, vec![forged, second], "{report:?}");
        assert!(
            report.missing_from_file.is_empty() && report.altered.is_empty() && report.journal_gaps.is_empty(),
            "{report:?}"
        );
        assert!(!report.is_clean());
    }

    #[test]
    fn records_written_during_the_check_are_counted_not_reported() {
        let dir = audit_dir_for("audit-verify-during");
        let from = days_ms(20_000);
        let entries = written_at(&dir, 6, from);
        let original = lines(&dir);
        // The journal's reading began as the fifth record was written: the last two are not in it yet.
        let read = entries[..4].to_vec();
        let report = verify_from(&dir, &Entries(read.clone()), EXE, ms(from + 4000)).unwrap();
        assert_eq!(report.written_during_check, 2, "{report:?}");
        assert!(report.is_clean(), "{report:?}");
        // Dated outside the window, they are not taken as written during the check.
        let late = from + 5000 + WRITTEN_DURING_CHECK.as_millis() as u64 + 1;
        let report = verify_from(&dir, &Entries(read.clone()), EXE, ms(late)).unwrap();
        assert_eq!(report.missing_from_journal, original[4..].to_vec(), "{report:?}");
        assert_eq!(report.written_during_check, 0);
        // In the middle too, since another writer's later copy can reach the journal first, but only in the window.
        let mut middle = entries.clone();
        middle.remove(3);
        let report = verify_from(&dir, &Entries(middle.clone()), EXE, ms(from + 3000)).unwrap();
        assert_eq!(report.written_during_check, 1, "{report:?}");
        assert!(report.missing_from_journal.is_empty(), "{report:?}");
        let report = verify_from(&dir, &Entries(middle), EXE, ms(late - 1000)).unwrap();
        assert_eq!(report.missing_from_journal, vec![original[3].clone()], "{report:?}");
        assert_eq!(report.written_during_check, 0);
        // Nor dated past the end of the check.
        let time = object(&original[5])["time"].as_str().unwrap().to_string();
        let future = forged_after(&original[5], 7, &original[5]).replace(&time, "2099-01-01T00:00:00.000Z");
        std::fs::write(
            dir.join(CURRENT),
            [&original[..], &[future.clone()]].concat().join("\n") + "\n",
        )
        .unwrap();
        let report = verify_from(&dir, &Entries(entries), EXE, SystemTime::now()).unwrap();
        assert_eq!(report.missing_from_journal, vec![future], "{report:?}");
        assert_eq!(report.written_during_check, 0);
    }

    #[test]
    fn a_clock_set_back_does_not_make_the_journal_reach_back() {
        let dir = audit_dir_for("audit-verify-clock-back");
        let journal = FakeJournal::bind();
        let clock = FakeClock::at_ms(days_ms(20_000));
        let mut log = open_at(&dir, WriterKind::Serve, None, journal.target(), &clock);
        let mut entries = Vec::new();
        // Dated 10, 20, then 15 seconds in: the clock went back before the third.
        for (n, s) in [10u64, 20, 15].into_iter().enumerate() {
            clock.set_ms(days_ms(20_000) + s * 1000);
            log.record(cmd("d1", n as u64)).unwrap();
            entries.push(journal.next_entry(EXE));
        }
        // The journal was vacuumed past the first; the third, still in the files, is dated before the anchor.
        let report = verify(&dir, &Entries(entries[1..].to_vec()), EXE).unwrap();
        assert_eq!(report.older_than_journal, 1, "{report:?}");
        assert!(report.is_clean(), "{report:?}");
    }

    #[test]
    fn records_older_than_the_journal_are_counted_unless_it_reaches_back() {
        let dir = audit_dir_for("audit-verify-older");
        let entries = written_at(&dir, 5, days_ms(20_000));
        // The journal was vacuumed past the first two (or is not persistent and the machine rebooted).
        let report = verify(&dir, &Entries(entries[2..].to_vec()), EXE).unwrap();
        assert_eq!(report.older_than_journal, 2, "{report:?}");
        assert!(report.is_clean(), "{report:?}");
        assert!(!report.journal_holds_none);
        // A journal that holds none of them: all five counted, and the report says nothing was checked.
        let report = verify(&dir, &Entries(Vec::new()), EXE).unwrap();
        assert_eq!(report.older_than_journal, 5, "{report:?}");
        assert!(report.journal_holds_none, "{report:?}");

        // With older records pruned from the files that the journal still holds, it reaches back past the files'
        // first records, so those it lacks are named.
        let dir = audit_dir_for("audit-verify-older-pruned");
        let journal = FakeJournal::bind();
        let clock = FakeClock::at_ms(days_ms(20_000));
        let mut log = open_at(&dir, WriterKind::Serve, None, journal.target(), &clock);
        log.rotate_bytes = 1;
        let mut entries = Vec::new();
        for n in 0..20 {
            clock.set_ms(days_ms(20_000) + n * 1000);
            log.record(cmd("d1", n)).unwrap();
            entries.push(journal.next_entry(EXE));
        }
        let retained = lines(&dir);
        assert_eq!(object(&retained[0])["seq"], 16, "the oldest records were dropped");
        entries.drain(15..17);
        let report = verify(&dir, &Entries(entries), EXE).unwrap();
        assert_eq!(report.missing_from_journal, retained[..2].to_vec(), "{report:?}");
        assert_eq!(report.older_than_journal, 0, "{report:?}");
    }

    #[test]
    fn a_log_nobody_touched_is_clean() {
        let dir = audit_dir_for("audit-verify-clean");
        let entries = written(&dir, 5);
        let report = verify(&dir, &Entries(entries), EXE).unwrap();
        assert!(report.is_clean(), "{report:?}");
    }

    #[test]
    fn verify_reports_edit_delete_and_forgery() {
        let dir = audit_dir_for("audit-verify-tamper");
        let mut entries = written(&dir, 6);
        let original = lines(&dir);
        let edited = original[1].replace(r#""device":"d1""#, r#""device":"d9""#);
        assert_ne!(edited, original[1]);
        let mut tampered = original.clone();
        tampered[1] = edited;
        tampered.remove(3);
        std::fs::write(dir.join(CURRENT), tampered.join("\n") + "\n").unwrap();
        let forged = original[4].replace(r#""seq":5"#, r#""seq":50"#);
        entries.push(JournalEntry {
            exe: Some("/usr/bin/python3".into()),
            record: Some(forged),
        });

        let report = verify(&dir, &Entries(entries), EXE).unwrap();
        assert_eq!(report.altered, vec![original[1].clone()], "{report:?}");
        assert_eq!(report.missing_from_file, vec![original[3].clone()], "{report:?}");
        assert_eq!(report.forged, 1);
        assert!(report.journal_gaps.is_empty(), "{report:?}");
        assert_eq!(
            report.chain_breaks,
            vec![(CURRENT.to_string(), 3), (CURRENT.to_string(), 4)],
            "the line after the edited one, and the line after the deleted one"
        );
    }

    #[test]
    fn a_dropped_journal_message_is_a_gap() {
        let dir = audit_dir_for("audit-verify-gap");
        let mut entries = written(&dir, 5);
        let run = object(entries[0].record.as_deref().unwrap())["run"]
            .as_str()
            .unwrap()
            .to_string();
        entries.remove(2);
        let report = verify(&dir, &Entries(entries), EXE).unwrap();
        assert_eq!(report.journal_gaps, vec![(format!("serve {run}"), 2, 4)]);
        assert!(report.missing_from_file.is_empty() && report.altered.is_empty() && report.chain_breaks.is_empty());
    }

    #[test]
    fn deleted_exe_is_still_ours() {
        let dir = audit_dir_for("audit-verify-upgrade");
        let entries: Vec<_> = written(&dir, 3)
            .into_iter()
            .map(|e| JournalEntry {
                exe: Some(format!("{EXE} (deleted)")),
                ..e
            })
            .collect();
        let report = verify(&dir, &Entries(entries.clone()), EXE).unwrap();
        assert!(report.is_clean(), "{report:?}");
        // And a binary with another name is not, even when its name starts the same.
        let other: Vec<_> = entries
            .into_iter()
            .map(|e| JournalEntry {
                exe: Some(format!("{EXE}-evil")),
                ..e
            })
            .collect();
        let report = verify(&dir, &Entries(other), EXE).unwrap();
        assert_eq!(report.forged, 3);
        assert_eq!(
            report.forged_senders,
            BTreeMap::from([(format!("{EXE}-evil"), 3)]),
            "the sender is named"
        );
    }

    #[test]
    fn verify_is_clean_after_restarts_and_pruning() {
        let dir = audit_dir_for("audit-verify-weeks");
        let journal = FakeJournal::bind();
        let clock = FakeClock::at_ms(days_ms(20_000));
        let mut entries = Vec::new();
        let mut write = |log: &mut AuditLog, n: u64, who: &str| {
            for i in 0..n {
                log.record(cmd(who, i)).unwrap();
                entries.push(journal.next_entry(EXE));
            }
        };
        // Three serve runs weeks apart, each from seq 1, and two revokes from a terminal between them; files rotate
        // as they fill, and serve prunes what is past its 30 days.
        for (day, n) in [(0, 9), (20, 7), (45, 8)] {
            clock.set_ms(days_ms(20_000 + day));
            let mut serve = open_at(&dir, WriterKind::Serve, Some(30), journal.target(), &clock);
            serve.rotate_bytes = 1200;
            write(&mut serve, n, "serve");
            clock.set_ms(days_ms(20_000 + day) + 60_000);
            let mut cli = open_at(&dir, WriterKind::Cli, None, journal.target(), &clock);
            cli.rotate_bytes = 1200;
            write(&mut cli, 1, "cli");
        }
        let retained = lines(&dir);
        assert!(
            retained.len() < entries.len(),
            "something was pruned: {} of {}",
            retained.len(),
            entries.len()
        );
        assert_ne!(
            object(&retained[0])["prev"],
            chain::ZERO_PREV,
            "the oldest kept record's predecessor went"
        );
        // Every record of a run here shares its run's millisecond, the pruned ones included, so what tells a pruned
        // record from a deleted one is the chain, not only the time.
        assert_eq!(
            object(&retained[0])["time"],
            object(&entries[0].record.clone().unwrap())["time"]
        );
        let report = verify(&dir, &Entries(entries), EXE).unwrap();
        assert!(report.is_clean(), "{report:?}");
    }
}
