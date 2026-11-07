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
    /// Records whose `(writer, run, seq)` the file holds with other bytes, as the journal has them.
    pub altered: Vec<String>,
    /// Entries not sent by the Eitri binary, or that are not records.
    pub forged: usize,
    /// `("<writer> <run>", seq, next seq)` where a run's journal copies skip numbers: messages the journal dropped.
    pub journal_gaps: Vec<(String, u64, u64)>,
}

impl VerifyReport {
    /// Whether nothing was found.
    pub fn is_clean(&self) -> bool {
        *self == VerifyReport::default()
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
pub fn verify(dir: &Path, journal: &dyn JournalSource, our_exe: &str) -> io::Result<VerifyReport> {
    // The journal first: a record written while this runs is then in the file but not yet in what was read of the
    // journal, which is not reported, rather than the other way round, which would be.
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
            _ => report.forged += 1,
        }
    }
    let in_journal: HashSet<&str> = ours.iter().map(|r| r.line.as_str()).collect();

    // What may have been pruned, from the anchor.
    let mut before_anchor: HashSet<&str> = HashSet::new();
    if let Some(anchor) = retained.iter().find(|r| in_journal.contains(r.line.as_str())) {
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
        assert!(report.chain_breaks.is_empty(), "{report:?}");
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
        assert_eq!(verify(&dir, &Entries(other), EXE).unwrap().forged, 3);
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
