//! `eitri remote log`: the remote audit log read back, and checked against the journal.
//!
//! `eitri remote log [--since 24h|7d|<RFC 3339>] [--device NAME] [--event KIND] [--json]` prints the retained
//! records newest first, one line each: `time writer event device fields`, the device as `name (id)` (or `-`), the
//! rest as `key=value`; every string from the record goes through `cli::shown`, since names, `Host`, `Origin` and
//! peers came from the network. `--json` prints the stored lines as they are.
//!
//! `eitri remote log --verify` checks the chain of the retained files and compares them with the journal's copy both
//! ways (`journalctl` by its absolute path, [`JOURNALCTL_PATHS`], run here and never by `serve`; entries count as the
//! log's only when the journal's `_EXE` is this binary): exit 0 when nothing is found and every retained record was
//! checked, 1 when something is found, 2 on bad arguments, 3 when the journal could not be read (or holds none of the
//! log's records) and the chain alone showed nothing, which is no proof the log is whole, and 4
//! ([`VERIFY_INCOMPLETE`]) when nothing is found but not everything was checked: records counted as older than the
//! journal or as written during the check, or one of the journal's two readings failed. A record counted as written
//! during the check makes it read the journal once more after [`RECHECK_AFTER`], so only those still missing then
//! stay unchecked.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use eitri_core::remote_audit::{
    read, verify_from, Filter, JournalEntry, JournalSource, Record, AUDIT_ROTATED_FILES, WRITTEN_DURING_CHECK,
};
use serde_json::Value;

use crate::cli::shown;

const USAGE: &str = "usage: eitri remote log [--since 24h|7d|<RFC 3339>] [--device NAME] [--event KIND] [--json]
       eitri remote log --verify    (exit 0 clean, 1 something found, 3 the journal could not be read or
                                    holds none of the log's records, 4 nothing found but not everything checked)";

/// `--verify`'s exit when the journal could not be read and the chain alone showed nothing.
pub const VERIFY_JOURNAL_UNREAD: i32 = 3;

/// `--verify`'s exit when nothing was found but not every record was checked against the journal: some were counted
/// as older than it or as written during the check, or a part of the journal could not be read. Its first line is
/// `incomplete: ...`, then one `  not checked: ...` line per reason.
pub const VERIFY_INCOMPLETE: i32 = 4;

/// How long `--verify` waits before reading the journal again when records were counted as written during the check:
/// a writer's send waits at most 50 ms for the journal (`SEND_WITHIN`), so a copy still on its way is there by then.
#[cfg(not(test))]
pub const RECHECK_AFTER: Duration = Duration::from_secs(1);
/// No wait in this crate's own tests, whose second reading answers at once.
#[cfg(test)]
pub const RECHECK_AFTER: Duration = Duration::ZERO;

/// Where `journalctl` is looked for, first found wins: the merged-`/usr` path, the old `/bin` one, and NixOS's system
/// profile. Never through `PATH`, which a same-uid process (or the caller's shell) can point at a `journalctl` that
/// prints whatever matches the files.
pub const JOURNALCTL_PATHS: &[&str] = &[
    "/usr/bin/journalctl",
    "/bin/journalctl",
    "/run/current-system/sw/bin/journalctl",
];

/// The feature that holds the test-only overrides; never enabled by a shipped build.
#[cfg(test)]
const HOOKS: &str = "remote-test-hooks";

/// `eitri remote log <args>`.
pub fn log(args: &[String]) -> i32 {
    let state = eitri_core::state_paths::state_subdir(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
        "remote",
    );
    let Some(state) = state else {
        eprintln!("eitri remote log: neither XDG_STATE_HOME nor HOME names an absolute directory");
        return 1;
    };
    let dir = eitri_core::remote_audit::audit_dir(&state);
    let our_exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (mut out, mut err) = (io::stdout().lock(), io::stderr().lock());
    log_at(&dir, args, &Journalctl::find(), &our_exe, &mut out, &mut err)
}

/// What the arguments ask for.
#[derive(Debug, Default)]
struct Asked {
    filter: Filter,
    json: bool,
    verify: bool,
}

fn parse_args(args: &[String], now: SystemTime) -> Result<Asked, String> {
    let mut asked = Asked::default();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let mut value = || rest.next().cloned().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--since" if asked.filter.since.is_none() => {
                let text = value()?;
                let since = parse_since(&text, now)
                    .ok_or_else(|| format!("--since takes 24h, 7d or an RFC 3339 time, not {text:?}"))?;
                asked.filter.since = Some(since);
            }
            "--device" if asked.filter.device.is_none() => asked.filter.device = Some(value()?),
            "--event" if asked.filter.event.is_none() => asked.filter.event = Some(value()?),
            "--json" if !asked.json => asked.json = true,
            "--verify" if !asked.verify => asked.verify = true,
            other => return Err(format!("{other:?} is not taken here, or is given twice")),
        }
    }
    let filtered = asked.filter.since.is_some() || asked.filter.device.is_some() || asked.filter.event.is_some();
    if asked.verify && (filtered || asked.json) {
        return Err("--verify takes nothing else".into());
    }
    Ok(asked)
}

/// [`log`] on the audit directory `dir`, with `journal` for `--verify` (whose entries count when sent by `our_exe`).
pub fn log_at(
    dir: &Path,
    args: &[String],
    journal: &dyn JournalSource,
    our_exe: &str,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let asked = match parse_args(args, SystemTime::now()) {
        Ok(asked) => asked,
        Err(why) => {
            let _ = writeln!(err, "eitri remote log: {why}\n{USAGE}");
            return 2;
        }
    };
    // Checked whatever the files say: with every file gone, each record the journal holds is missing from them.
    if asked.verify {
        return verify_at(dir, journal, our_exe, out, err);
    }
    if !any_file(dir) {
        let _ = writeln!(
            err,
            "eitri remote log: no remote audit log in {} (remote control was never on)",
            shown(&dir.display().to_string())
        );
        return 0;
    }
    let records = match read(dir, &asked.filter, None) {
        Ok(records) => records,
        Err(e) => {
            let _ = writeln!(err, "eitri remote log: {e}");
            return 1;
        }
    };
    for record in &records {
        let line = if asked.json {
            record.line.clone()
        } else {
            line_of(record)
        };
        if writeln!(out, "{line}").is_err() {
            return 1;
        }
    }
    0
}

/// One record as a line: `time writer event device key=value...`, every string shown with its hidden characters
/// spelled out.
fn line_of(record: &Record) -> String {
    let text = |value: &Value| match value {
        Value::String(s) => shown(s),
        other => shown(&other.to_string()),
    };
    let device = match (record.fields.get("device_name"), record.fields.get("device")) {
        (Some(name), Some(id)) => format!("{} ({})", text(name), text(id)),
        (None, Some(id)) => text(id),
        _ => "-".to_owned(),
    };
    let mut line = format!(
        "{} {} {} {device}",
        shown(&record.time),
        shown(&record.writer),
        shown(&record.event)
    );
    for (key, value) in &record.fields {
        if key == "device" || key == "device_name" {
            continue;
        }
        line.push(' ');
        line.push_str(&shown(key));
        line.push('=');
        line.push_str(&text(value));
    }
    line
}

/// Whether any file of the log is in `dir`: the current one or a rotated one.
fn any_file(dir: &Path) -> bool {
    std::iter::once("audit.jsonl".to_owned())
        .chain((1..=AUDIT_ROTATED_FILES).map(|n| format!("audit.jsonl.{n}")))
        .any(|name| dir.join(name).symlink_metadata().is_ok())
}

fn verify_at(dir: &Path, journal: &dyn JournalSource, our_exe: &str, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    if !any_file(dir) {
        let _ = writeln!(
            err,
            "eitri remote log: no remote audit log in {}; checking the journal's records against none",
            shown(&dir.display().to_string())
        );
    }
    // The journal is read once (each reading runs `journalctl` twice) and its entries handed to `verify`, with the
    // time the reading began, from which a record counts as written during the check.
    let started = SystemTime::now();
    let (entries, unread) = match journal.entries() {
        Ok(entries) => (entries, None),
        Err(e) => {
            let _ = writeln!(
                err,
                "eitri remote log: the journal could not be read ({e}); checking the chain only"
            );
            (Vec::new(), Some(e))
        }
    };
    let mut partial = journal.unread_parts();
    let mut report = match verify_from(dir, &Fetched(entries.clone()), our_exe, started) {
        Ok(report) => report,
        Err(e) => {
            let _ = writeln!(err, "eitri remote log: {e}");
            return 1;
        }
    };
    // A record counted as written during the check may only have had its copy on the way: once more, a moment later,
    // so only the records still missing then are left to the window.
    if unread.is_none() && report.written_during_check > 0 {
        std::thread::sleep(RECHECK_AFTER);
        match journal.entries() {
            Ok(again) => {
                // The journal only grows (its oldest entries aside), so both readings together are what it held: a
                // part that failed in one reading and was read in the other is read, and a record the first reading
                // saw is kept when the second misses it.
                let again_partial = journal.unread_parts();
                partial.retain(|(part, _)| again_partial.iter().any(|(p, _)| p == part));
                let mut both = entries;
                let mut seen: std::collections::HashSet<(Option<String>, Option<String>)> =
                    both.iter().map(|e| (e.exe.clone(), e.record.clone())).collect();
                both.extend(
                    again
                        .into_iter()
                        .filter(|e| seen.insert((e.exe.clone(), e.record.clone()))),
                );
                report = match verify_from(dir, &Fetched(both), our_exe, started) {
                    Ok(report) => report,
                    Err(e) => {
                        let _ = writeln!(err, "eitri remote log: {e}");
                        return 1;
                    }
                };
            }
            Err(e) => {
                let _ = writeln!(
                    err,
                    "eitri remote log: the journal's second reading failed ({e}); the first one stands"
                );
            }
        }
    }
    for (part, why) in &partial {
        let _ = writeln!(
            err,
            "eitri remote log: part of the journal could not be read (journalctl {}: {}); records only that part \
             holds were not checked",
            shown(part),
            shown(why)
        );
    }
    let mut lines = Vec::new();
    for (file, n) in &report.chain_breaks {
        lines.push(format!("chain broken: {} line {n}", shown(file)));
    }
    for line in &report.missing_from_file {
        lines.push(format!("missing from the file: {}", shown(line)));
    }
    for line in &report.altered {
        lines.push(format!("altered in the file: {}", shown(line)));
    }
    // With no journal read every record is only the files', which says nothing.
    if unread.is_none() {
        for line in &report.missing_from_journal {
            lines.push(format!("not in the journal: {}", shown(line)));
        }
    }
    if report.forged > 0 {
        lines.push(format!(
            "journal entries not sent by this binary or not records: {}",
            report.forged
        ));
        // Who sent them, as the journal names the executable: another Eitri build reads as its path.
        for (exe, n) in report.forged_senders.iter().take(FORGED_SENDERS_SHOWN) {
            match exe.as_str() {
                "" => lines.push(format!("  from no executable the journal named: {n}")),
                exe if exe == our_exe => lines.push(format!("  from this binary, not records: {n}")),
                exe => lines.push(format!("  from {}: {n}", shown(exe))),
            }
        }
        if report.forged_senders.len() > FORGED_SENDERS_SHOWN {
            lines.push(format!(
                "  and {} more executables",
                report.forged_senders.len() - FORGED_SENDERS_SHOWN
            ));
        }
    }
    for (run, from, to) in &report.journal_gaps {
        lines.push(format!(
            "journal gap: {} seq {from} to {to} (messages the journal dropped)",
            shown(run)
        ));
    }
    if lines.is_empty() {
        if let Some(e) = unread {
            let _ = writeln!(
                out,
                "not verified: the chain holds, but the journal could not be read ({}), so a record deleted, added or \
                 rewritten with every later line is not seen",
                shown(&e.to_string())
            );
            return VERIFY_JOURNAL_UNREAD;
        }
        if report.journal_holds_none {
            let _ = writeln!(
                out,
                "not verified: the chain holds, but the journal holds none of the log's {} records (no journal files, \
                 none kept, a volatile journal since a reboot, or another user), so none was checked against it",
                report.older_than_journal
            );
            return VERIFY_JOURNAL_UNREAD;
        }
        let mut unchecked = Vec::new();
        if report.older_than_journal > 0 {
            unchecked.push(format!(
                "  not checked: {} oldest records, older than anything of the log's the journal still holds",
                report.older_than_journal
            ));
        }
        if report.written_during_check > 0 {
            unchecked.push(format!(
                "  not checked: {} records written during the check (within {} s) and not in the journal yet, also \
                 when read again; run it again",
                report.written_during_check,
                WRITTEN_DURING_CHECK.as_secs()
            ));
        }
        for (part, why) in &partial {
            unchecked.push(format!(
                "  not checked: what only this part of the journal holds: journalctl {}: {}",
                shown(part),
                shown(why)
            ));
        }
        if unchecked.is_empty() {
            let _ = writeln!(out, "clean");
            return 0;
        }
        let _ = writeln!(out, "incomplete: nothing found, but not everything was checked");
        for line in unchecked {
            let _ = writeln!(out, "{line}");
        }
        return VERIFY_INCOMPLETE;
    }
    for line in lines {
        let _ = writeln!(out, "{line}");
    }
    1
}

/// How many of the executables that sent entries not counted as records `--verify` names.
const FORGED_SENDERS_SHOWN: usize = 8;

/// The journal's entries as already read (none when it could not be read: the chain alone is checked).
struct Fetched(Vec<JournalEntry>);

impl JournalSource for Fetched {
    fn entries(&self) -> io::Result<Vec<JournalEntry>> {
        Ok(self.0.clone())
    }
}

/// The journal's `eitri-remote` entries through `journalctl`, found once ([`Journalctl::find`]): the user's journal,
/// then the system journal's entries of this uid. `Err` when there is no `journalctl` or neither could be read; when
/// one of the two fails, the other's entries, and the failure in [`JournalSource::unread_parts`].
pub struct Journalctl {
    bin: Option<PathBuf>,
    failed: std::cell::RefCell<Vec<(String, String)>>,
}

/// The variable a `remote-test-hooks` build reads for the `journalctl` to run instead of [`JOURNALCTL_PATHS`]; with it
/// unset or empty such a build finds none, so no test reads the machine's journal. A release build never reads it.
pub const JOURNALCTL_VAR: &str = "EITRI_REMOTE_TEST_JOURNALCTL";

impl Journalctl {
    /// The first of [`JOURNALCTL_PATHS`] that is a file (in a `remote-test-hooks` build, [`JOURNALCTL_VAR`] alone).
    pub fn find() -> Journalctl {
        Journalctl::at(journalctl_path_with(
            |name| std::env::var_os(name),
            |path| path.metadata().is_ok_and(|m| m.is_file()),
        ))
    }

    /// The `journalctl` at `bin`, or none.
    fn at(bin: Option<PathBuf>) -> Journalctl {
        Journalctl {
            bin,
            failed: Default::default(),
        }
    }
}

fn journalctl_path_with(
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
    is_file: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    #[cfg(feature = "remote-test-hooks")]
    {
        let _ = is_file;
        env(JOURNALCTL_VAR).filter(|v| !v.is_empty()).map(PathBuf::from)
    }
    #[cfg(not(feature = "remote-test-hooks"))]
    {
        let _ = env;
        JOURNALCTL_PATHS.iter().map(PathBuf::from).find(|path| is_file(path))
    }
}

impl JournalSource for Journalctl {
    fn entries(&self) -> io::Result<Vec<JournalEntry>> {
        self.failed.borrow_mut().clear();
        let Some(bin) = &self.bin else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no journalctl at {}", JOURNALCTL_PATHS.join(", ")),
            ));
        };
        let uid = agent::private_fs::euid().to_string();
        let user = ["--user".to_owned(), "SYSLOG_IDENTIFIER=eitri-remote".to_owned()];
        let system = [format!("_UID={uid}"), "SYSLOG_IDENTIFIER=eitri-remote".to_owned()];
        let (mut entries, mut failed) = (Vec::new(), Vec::new());
        for matches in [user, system] {
            let matches: Vec<&str> = matches.iter().map(String::as_str).collect();
            match journalctl(bin, &matches) {
                Ok(found) => entries.extend(found),
                Err(e) => failed.push((matches.join(" "), e.to_string())),
            }
        }
        if failed.len() == 2 {
            let all: Vec<String> = failed
                .iter()
                .map(|(part, why)| format!("journalctl {part}: {why}"))
                .collect();
            return Err(io::Error::other(all.join("; ")));
        }
        *self.failed.borrow_mut() = failed;
        // An entry both readings return (a journal that is not split by user) is one entry.
        let mut seen = std::collections::HashSet::new();
        entries.retain(|e| seen.insert((e.exe.clone(), e.record.clone())));
        Ok(entries)
    }

    fn unread_parts(&self) -> Vec<(String, String)> {
        self.failed.borrow().clone()
    }
}

/// One `<bin> -o json` reading with `matches`.
fn journalctl(bin: &Path, matches: &[&str]) -> io::Result<Vec<JournalEntry>> {
    let output = std::process::Command::new(bin)
        .args(["--no-pager", "--all", "-o", "json"])
        .args(matches)
        .stdin(std::process::Stdio::null())
        .output()?;
    if !output.status.success() {
        // The caller names the reading; this says why it failed.
        let said = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(io::Error::other(if said.is_empty() {
            output.status.to_string()
        } else {
            said
        }));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .map(|entry| JournalEntry {
            exe: field_text(&entry["_EXE"]),
            record: field_text(&entry["EITRI_RECORD"]),
        })
        .collect())
}

/// A journal field as `journalctl -o json` gives it: a string, or an array of bytes for one that is not UTF-8.
fn field_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Array(bytes) => {
            let bytes: Option<Vec<u8>> = bytes
                .iter()
                .map(|b| b.as_u64().and_then(|b| u8::try_from(b).ok()))
                .collect();
            Some(String::from_utf8_lossy(&bytes?).into_owned())
        }
        _ => None,
    }
}

/// `24h`, `7d` (hours or days before `now`), or an RFC 3339 time (`2026-11-07T10:00:00Z`, fractions and offsets
/// taken).
fn parse_since(text: &str, now: SystemTime) -> Option<SystemTime> {
    let ago = |n: &str, unit: u64| -> Option<SystemTime> {
        if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        now.checked_sub(Duration::from_secs(n.parse::<u64>().ok()?.checked_mul(unit)?))
    };
    if let Some(n) = text.strip_suffix('h') {
        return ago(n, 3600);
    }
    if let Some(n) = text.strip_suffix('d') {
        return ago(n, 86_400);
    }
    parse_rfc3339(text)
}

fn parse_rfc3339(text: &str) -> Option<SystemTime> {
    let b = text.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |range: std::ops::Range<usize>| -> Option<i64> {
        let part = text.get(range)?;
        part.bytes().all(|c| c.is_ascii_digit()).then(|| part.parse().ok())?
    };
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut rest = &text[19..];
    let mut nanos: u64 = 0;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        for (i, c) in frac[..digits].bytes().take(9).enumerate() {
            nanos += u64::from(c - b'0') * 10u64.pow(8 - i as u32);
        }
        rest = &frac[digits..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let hm = rest.get(1..)?;
            if hm.len() != 5 || hm.as_bytes()[2] != b':' {
                return None;
            }
            let (h, m) = (hm[..2].parse::<i64>().ok()?, hm[3..].parse::<i64>().ok()?);
            if h > 23 || m > 59 {
                return None;
            }
            sign * (h * 3600 + m * 60)
        }
    };
    let secs = days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second - offset;
    let secs = u64::try_from(secs).ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(secs) + Duration::from_nanos(nanos))
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days from 1970-01-01 to the given civil date (proleptic Gregorian).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// How a manifest names [`HOOKS`]: where a scan of every `Cargo.toml` allows it, and where it does not.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookUse {
    /// The feature's own definition, `remote-test-hooks = [...]` in `[features]`, and what it forwards to.
    Defined,
    /// Enabled in a `[dev-dependencies]` table: only test builds have it.
    DevDependency,
    /// Anywhere else: a `[dependencies]`, `[build-dependencies]` or `[workspace.dependencies]` entry, or another
    /// feature (`default`) that turns it on, any of which would put it in a shipped build.
    Elsewhere,
}

#[cfg(test)]
/// Every mention of [`HOOKS`] in a manifest, by the table it is in. Comments are left out; the table a mention
/// belongs to is the last header before it, so where a list breaks its lines does not matter.
fn hook_uses(text: &str) -> Vec<HookUse> {
    let mut table = String::new();
    let mut feature_key: Option<String> = None;
    let mut uses = Vec::new();
    for raw in text.lines() {
        let line = without_comment(raw).trim();
        if line.starts_with('[') && !line.starts_with("[\"") {
            table = line.trim_start_matches('[').trim_end_matches(']').trim().to_owned();
            feature_key = None;
            continue;
        }
        if table == "features" {
            if let Some((key, _)) = line.split_once('=') {
                let key = key.trim();
                if !key.is_empty() && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
                    feature_key = Some(key.to_owned());
                }
            }
        }
        for _ in line.matches(HOOKS) {
            let dev = table == "dev-dependencies"
                || table.ends_with(".dev-dependencies")
                || table.starts_with("dev-dependencies.")
                || table.contains(".dev-dependencies.");
            uses.push(if dev {
                HookUse::DevDependency
            } else if table == "features" && feature_key.as_deref() == Some(HOOKS) {
                HookUse::Defined
            } else {
                HookUse::Elsewhere
            });
        }
    }
    // A definition line names the feature twice (the key and what it forwards to): one definition.
    let mut out: Vec<HookUse> = Vec::new();
    let mut defined = false;
    for u in uses {
        if u == HookUse::Defined {
            if defined {
                continue;
            }
            defined = true;
        }
        out.push(u);
    }
    out
}

/// `line` up to a `#` that is not inside a string.
#[cfg(test)]
fn without_comment(line: &str) -> &str {
    let mut quoted = None;
    for (at, c) in line.char_indices() {
        match (quoted, c) {
            (None, '"' | '\'') => quoted = Some(c),
            (Some(q), c) if c == q => quoted = None,
            (None, '#') => return &line[..at],
            _ => {}
        }
    }
    line
}

/// The workspace's `Cargo.toml` and each member's, from the root manifest's `members` list.
#[cfg(test)]
fn workspace_manifests(root: &Path) -> Vec<std::path::PathBuf> {
    let text = std::fs::read_to_string(root.join("Cargo.toml")).expect("the workspace manifest reads");
    let list = text
        .split_once("members = [")
        .and_then(|(_, rest)| rest.split_once(']'))
        .map(|(list, _)| list)
        .expect("the workspace manifest lists its members");
    let mut out = vec![root.join("Cargo.toml")];
    for member in list
        .split(',')
        .map(|m| without_comment(m).trim().trim_matches('"'))
        .filter(|m| !m.is_empty())
    {
        out.push(root.join(member).join("Cargo.toml"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::remote_audit::{
        AuditLog, DeviceRef, Event, JournalEntry, JournalTarget, Listener, Reason, WriterKind,
    };
    use std::path::PathBuf;

    const OUR_EXE: &str = "/usr/bin/eitri";

    fn scratch(name: &str) -> PathBuf {
        use std::os::unix::fs::DirBuilderExt;
        let base =
            PathBuf::from(option_env!("CARGO_TARGET_DIR").unwrap_or(concat!(env!("CARGO_MANIFEST_DIR"), "/../target")))
                .join("tmp/remote-log-tests")
                .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&base)
            .unwrap();
        base.join("audit")
    }

    fn device(id: &str, name: &str) -> DeviceRef {
        DeviceRef {
            id: id.into(),
            name: name.into(),
        }
    }

    fn open(dir: &Path) -> AuditLog {
        AuditLog::open(dir, WriterKind::Serve, None, JournalTarget::Off).unwrap()
    }

    fn conn_open(id: &str, name: &str) -> Event {
        Event::ConnOpen {
            device: device(id, name),
            listener: Listener::Socket,
            peer: "unix".into(),
            host: "localhost".into(),
            origin: None,
        }
    }

    /// A journal holding every line of the files, sent by `exe`.
    struct Copy(Vec<JournalEntry>);

    impl Copy {
        fn of(dir: &Path, exe: &str) -> Copy {
            let records = eitri_core::remote_audit::read(dir, &Default::default(), None).unwrap();
            Copy(
                records
                    .into_iter()
                    .map(|r| JournalEntry {
                        exe: Some(exe.to_owned()),
                        record: Some(r.line),
                    })
                    .collect(),
            )
        }
    }

    impl JournalSource for Copy {
        fn entries(&self) -> io::Result<Vec<JournalEntry>> {
            Ok(self.0.clone())
        }
    }

    struct NoJournal;

    impl JournalSource for NoJournal {
        fn entries(&self) -> io::Result<Vec<JournalEntry>> {
            Err(io::Error::new(io::ErrorKind::NotFound, "journalctl: not found"))
        }
    }

    fn run(dir: &Path, words: &[&str], journal: &dyn JournalSource) -> (i32, String, String) {
        let args: Vec<String> = words.iter().map(|w| (*w).to_owned()).collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = log_at(dir, &args, journal, OUR_EXE, &mut out, &mut err);
        (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
    }

    #[test]
    fn log_prints_newest_first_and_filters() {
        let dir = scratch("filters");
        let mut log = open(&dir);
        log.record(conn_open("d1", "phone")).unwrap();
        log.record(Event::Refused {
            reason: Reason::BadToken,
            device: Some(device("d2", "tablet")),
            listener: Some(Listener::Socket),
            peer: Some("unix".into()),
            key: None,
            kind: None,
            dropped: None,
        })
        .unwrap();
        log.record(Event::ConnClose {
            device: device("d1", "phone"),
            why: "closed".into(),
            secs: 3,
            bytes_in: 10,
            bytes_out: 20,
        })
        .unwrap();

        let (code, out, err) = run(&dir, &[], &NoJournal);
        assert_eq!(code, 0, "{err}");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "{out}");
        for (line, event) in lines.iter().zip(["conn_close", "refused", "conn_open"]) {
            let words: Vec<&str> = line.split(' ').collect();
            assert!(
                words[0].ends_with('Z') && words[1] == "serve" && words[2] == event,
                "{line}"
            );
        }
        assert!(
            lines[0].contains("phone (d1)") && lines[0].contains("why=closed") && lines[0].contains("bytes_out=20")
        );
        assert!(
            lines[1].contains("tablet (d2)") && lines[1].contains("reason=bad_token"),
            "{}",
            lines[1]
        );

        for (words, want) in [
            (vec!["--device", "phone"], vec!["conn_close", "conn_open"]),
            (vec!["--device", "d2"], vec!["refused"]),
            (vec!["--event", "refused"], vec!["refused"]),
            (vec!["--since", "24h"], vec!["conn_close", "refused", "conn_open"]),
            (vec!["--since", "7d", "--event", "conn_open"], vec!["conn_open"]),
            (vec!["--since", "2999-01-01T00:00:00Z"], vec![]),
            (
                vec!["--since", "2000-01-01T08:00:00+08:00", "--device", "nobody"],
                vec![],
            ),
        ] {
            let (code, out, err) = run(&dir, &words, &NoJournal);
            assert_eq!(code, 0, "{words:?}: {err}");
            let events: Vec<&str> = out.lines().map(|l| l.split(' ').nth(2).unwrap()).collect();
            assert_eq!(events, want, "{words:?}");
        }

        let (code, out, _) = run(&dir, &["--json"], &NoJournal);
        assert_eq!(code, 0);
        let file = std::fs::read_to_string(dir.join("audit.jsonl")).unwrap();
        let mut stored: Vec<&str> = file.lines().collect();
        stored.reverse();
        assert_eq!(
            out.lines().collect::<Vec<_>>(),
            stored,
            "--json prints the stored lines, newest first"
        );

        for words in [
            vec!["--since"],
            vec!["--since", "soon"],
            vec!["--since", "24x"],
            vec!["--event"],
            vec!["--device"],
            vec!["--bogus"],
            vec!["extra"],
            vec!["--json", "--json"],
        ] {
            let (code, _, err) = run(&dir, &words, &NoJournal);
            assert_eq!(code, 2, "{words:?}");
            assert!(err.contains("usage: eitri remote log"), "{words:?}: {err}");
        }
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn log_shows_escape_and_bidi_characters_visibly() {
        let dir = scratch("escapes");
        let mut log = open(&dir);
        log.record(Event::ConnOpen {
            device: device("d1", "evil\u{1b}[2J\u{202e}gnp.exe"),
            listener: Listener::Socket,
            peer: "unix".into(),
            host: "desk\nforged 2026-01-01T00:00:00.000Z serve enable".into(),
            origin: Some("http://x\u{2066}y".into()),
        })
        .unwrap();
        let (code, out, _) = run(&dir, &[], &NoJournal);
        assert_eq!(code, 0);
        assert_eq!(
            out.lines().count(),
            1,
            "a network string cannot start a line of its own: {out:?}"
        );
        for raw in ['\u{1b}', '\u{202e}', '\u{2066}', '\r'] {
            assert!(!out.contains(raw), "{raw:?} printed as is: {out:?}");
        }
        for shown in ["\\u{1b}", "\\u{202e}", "\\u{2066}", "\\u{a}"] {
            assert!(out.contains(shown), "{shown} missing: {out}");
        }
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn verify_names_who_sent_the_entries_it_does_not_count() {
        let dir = scratch("verify-senders");
        let mut log = open(&dir);
        for n in 0..2 {
            log.record(conn_open(&format!("d{n}"), "phone")).unwrap();
        }
        let mut journal = Copy::of(&dir, OUR_EXE);
        let other = Copy::of(&dir, "/home/me/eitri/target/debug/shell");
        journal.0.extend(other.0);
        journal.0.push(JournalEntry {
            exe: Some("/srv/x\u{1b}[2J".into()),
            record: Some("not a record".into()),
        });
        journal.0.push(JournalEntry {
            exe: None,
            record: None,
        });
        journal.0.push(JournalEntry {
            exe: Some(OUR_EXE.into()),
            record: Some("not a record".into()),
        });
        let (code, out, err) = run(&dir, &["--verify"], &journal);
        assert_eq!(code, 1, "{out}{err}");
        for line in [
            "journal entries not sent by this binary or not records: 5",
            "  from /home/me/eitri/target/debug/shell: 2",
            "  from no executable the journal named: 1",
            "  from this binary, not records: 1",
        ] {
            assert!(out.lines().any(|l| l == line), "{line:?} missing: {out}");
        }
        assert!(
            !out.contains('\u{1b}'),
            "a sender's path is shown, not printed as is: {out:?}"
        );
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn log_verify_exit_codes() {
        let dir = scratch("verify");
        let mut log = open(&dir);
        for n in 0..3 {
            log.record(conn_open(&format!("d{n}"), "phone")).unwrap();
        }
        let journal = Copy::of(&dir, OUR_EXE);
        let (code, out, err) = run(&dir, &["--verify"], &journal);
        assert_eq!(code, 0, "{out}{err}");
        assert!(out.contains("clean"), "{out}");

        // Without a journal it says so, checks the chain alone, and does not call that clean.
        let (code, out, err) = run(&dir, &["--verify"], &NoJournal);
        assert_eq!(code, VERIFY_JOURNAL_UNREAD, "{out}{err}");
        assert!(err.contains("chain only"), "{err}");
        assert!(out.starts_with("not verified: ") && !out.contains("clean"), "{out}");
        // And with no file either: nothing proves the log was never there.
        let empty = scratch("verify-no-journal-no-log");
        assert_eq!(run(&empty, &["--verify"], &NoJournal).0, VERIFY_JOURNAL_UNREAD);

        // An edit in the middle of the file.
        let path = dir.join("audit.jsonl");
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replacen("\"d1\"", "\"dX\"", 1)).unwrap();
        let (code, out, _) = run(&dir, &["--verify"], &journal);
        assert_eq!(code, 1, "{out}");
        assert!(out.contains("altered") && out.contains("chain"), "{out}");
        let (code, _, _) = run(&dir, &["--verify"], &NoJournal);
        assert_eq!(code, 1, "the chain alone shows the edit");

        for words in [
            vec!["--verify", "--json"],
            vec!["--verify", "--event", "cmd"],
            vec!["--verify", "x"],
        ] {
            assert_eq!(run(&dir, &words, &journal).0, 2, "{words:?}");
        }
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn verify_names_the_journals_records_when_the_files_are_gone() {
        let dir = scratch("gone");
        let mut log = open(&dir);
        for n in 0..3 {
            log.record(conn_open(&format!("d{n}"), "phone")).unwrap();
        }
        let journal = Copy::of(&dir, OUR_EXE);
        let missing = |out: &str| out.lines().filter(|l| l.starts_with("missing from the file:")).count();

        // Only an older file left: it is still read, and still checked.
        std::fs::rename(dir.join("audit.jsonl"), dir.join("audit.jsonl.2")).unwrap();
        let (code, out, err) = run(&dir, &[], &NoJournal);
        assert_eq!((code, out.lines().count()), (0, 3), "{out}{err}");
        let (code, out, err) = run(&dir, &["--verify"], &journal);
        assert_eq!(code, 0, "{out}{err}");
        assert!(out.contains("clean"), "{out}{err}");

        // Every file gone: each record the journal holds is named.
        std::fs::remove_file(dir.join("audit.jsonl.2")).unwrap();
        let (code, out, err) = run(&dir, &["--verify"], &journal);
        assert_eq!((code, missing(&out)), (1, 3), "{out}{err}");
        for n in 0..3 {
            assert!(out.contains(&format!("\"device\":\"d{n}\"")), "d{n}: {out}");
        }
        // And the directory too.
        std::fs::remove_dir_all(&dir).unwrap();
        let (code, out, err) = run(&dir, &["--verify"], &journal);
        assert_eq!((code, missing(&out)), (1, 3), "{out}{err}");
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn verify_names_a_record_only_the_file_holds() {
        let dir = scratch("verify-file-only");
        let mut log = open(&dir);
        for n in 0..3 {
            log.record(conn_open(&format!("d{n}"), "phone")).unwrap();
        }
        drop(log);
        let journal = Copy::of(&dir, OUR_EXE);
        // A record appended after the journal's last one, chained to it and dated an hour back: no writer sent it.
        let path = dir.join("audit.jsonl");
        let text = std::fs::read_to_string(&path).unwrap();
        let last = text.lines().last().unwrap().to_owned();
        let record = eitri_core::remote_audit::read(&dir, &Default::default(), Some(1))
            .unwrap()
            .remove(0);
        let hour_ago = {
            let at = SystemTime::now() - Duration::from_secs(3600);
            let secs = at.duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
            let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
            let (y, m, d) = civil_from_days(days);
            format!(
                "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.000Z",
                rest / 3600,
                rest % 3600 / 60,
                rest % 60
            )
        };
        let forged = last
            .replacen(&record.time, &hour_ago, 1)
            .replacen(&format!("\"seq\":{},", record.seq), "\"seq\":9,", 1)
            .replacen(&record.prev, &sha256_hex(last.as_bytes()), 1);
        std::fs::write(&path, format!("{text}{forged}\n")).unwrap();
        let (code, out, _) = run(&dir, &["--verify"], &journal);
        assert_eq!(code, 1, "{out}");
        assert_eq!(out.trim(), format!("not in the journal: {}", shown(&forged)), "{out}");
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    /// `(year, month, day)` of a day count from 1970-01-01.
    fn civil_from_days(z: i64) -> (i64, i64, i64) {
        let z = z + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        (yoe + era * 400 + i64::from(m <= 2), m, d)
    }

    /// A line's hash as the chain has it (the hex SHA-256 of its bytes).
    fn sha256_hex(bytes: &[u8]) -> String {
        eitri_core::remote_audit::text_digest(std::str::from_utf8(bytes).unwrap()).sha256
    }

    #[test]
    fn journalctl_is_run_by_its_absolute_path_only() {
        let env_with = |value: &'static str| move |name: &str| (name == JOURNALCTL_VAR).then(|| value.into());
        #[cfg(not(feature = "remote-test-hooks"))]
        {
            // The first listed path that is a file; never a name for `PATH` to find, and never the variable.
            let never_read = |_: &str| -> Option<std::ffi::OsString> { panic!("a release build read the variable") };
            let found = journalctl_path_with(never_read, |p| p == Path::new("/bin/journalctl"));
            assert_eq!(found, Some(PathBuf::from("/bin/journalctl")));
            assert_eq!(journalctl_path_with(env_with("/tmp/fake"), |_| false), None);
            assert!(JOURNALCTL_PATHS.iter().all(|p| p.starts_with('/')));
        }
        #[cfg(feature = "remote-test-hooks")]
        {
            // A test build runs only the variable's, and none without it, whatever the machine has.
            assert_eq!(
                journalctl_path_with(env_with("/tmp/fake"), |_| true),
                Some(PathBuf::from("/tmp/fake"))
            );
            assert_eq!(journalctl_path_with(env_with(""), |_| true), None);
        }
        // With none found, the reading fails, so `--verify` cannot call the log clean.
        assert!(Journalctl::at(None).entries().is_err());
    }

    #[test]
    fn a_journal_holding_none_of_the_records_is_not_verified() {
        let dir = scratch("verify-journal-empty");
        let mut log = open(&dir);
        log.record(conn_open("d1", "phone")).unwrap();
        drop(log);
        let (code, out, _) = run(&dir, &["--verify"], &Copy(Vec::new()));
        assert_eq!(code, VERIFY_JOURNAL_UNREAD, "{out}");
        assert!(out.starts_with("not verified: ") && !out.contains("clean"), "{out}");
        // With no file either, the two agree there is nothing: clean.
        let empty = scratch("verify-journal-empty-no-log");
        let (code, out, _) = run(&empty, &["--verify"], &Copy(Vec::new()));
        assert_eq!((code, out.trim()), (0, "clean"));
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn excused_records_are_incomplete_not_clean() {
        let dir = scratch("verify-excused");
        let mut log = open(&dir);
        for n in 0..3 {
            log.record(conn_open(&format!("d{n}"), "phone")).unwrap();
        }
        drop(log);
        // The journal was vacuumed past the first record (`Copy` lists them newest first).
        let mut journal = Copy::of(&dir, OUR_EXE);
        journal.0.pop();
        let (code, out, _) = run(&dir, &["--verify"], &journal);
        assert_eq!(code, VERIFY_INCOMPLETE, "{out}");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[0], "incomplete: nothing found, but not everything was checked",
            "{out}"
        );
        assert!(lines[1].starts_with("  not checked: 1 oldest records"), "{out}");
        assert!(!out.contains("clean"), "{out}");
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    /// The first reading lacks the newest record's copy, as when it was still on its way; later readings have it
    /// unless `never` says the copy never came.
    struct Late {
        whole: Copy,
        readings: std::cell::Cell<u32>,
        never: bool,
    }

    impl JournalSource for Late {
        fn entries(&self) -> io::Result<Vec<JournalEntry>> {
            self.readings.set(self.readings.get() + 1);
            let mut entries = self.whole.0.clone();
            if self.readings.get() == 1 || self.never {
                // `Copy` lists them newest first.
                entries.remove(0);
            }
            Ok(entries)
        }
    }

    #[test]
    fn a_record_counted_as_written_during_the_check_is_read_again() {
        let dir = scratch("verify-late");
        let mut log = open(&dir);
        for n in 0..3 {
            log.record(conn_open(&format!("d{n}"), "phone")).unwrap();
        }
        drop(log);
        // Its copy arrived by the second reading: clean.
        let late = Late {
            whole: Copy::of(&dir, OUR_EXE),
            readings: std::cell::Cell::new(0),
            never: false,
        };
        let (code, out, _) = run(&dir, &["--verify"], &late);
        assert_eq!((code, out.trim()), (0, "clean"));
        assert_eq!(late.readings.get(), 2, "read once more");
        // Still missing then: incomplete, and said so.
        let never = Late {
            whole: Copy::of(&dir, OUR_EXE),
            readings: std::cell::Cell::new(0),
            never: true,
        };
        let (code, out, _) = run(&dir, &["--verify"], &never);
        assert_eq!(code, VERIFY_INCOMPLETE, "{out}");
        assert!(
            out.contains("  not checked: 1 records written during the check"),
            "{out}"
        );
        assert_eq!(never.readings.get(), 2);
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_partial_second_reading_keeps_what_the_first_saw() {
        // First reading: every record but the newest. Second: the newest, but the part that held the others failed.
        struct Retry {
            whole: Copy,
            readings: std::cell::Cell<u32>,
        }
        impl JournalSource for Retry {
            fn entries(&self) -> io::Result<Vec<JournalEntry>> {
                self.readings.set(self.readings.get() + 1);
                let mut entries = self.whole.0.clone();
                if self.readings.get() == 1 {
                    entries.remove(0);
                } else {
                    entries.truncate(1);
                }
                Ok(entries)
            }
            fn unread_parts(&self) -> Vec<(String, String)> {
                match self.readings.get() {
                    1 => Vec::new(),
                    _ => vec![("_UID=1".into(), "gone".into())],
                }
            }
        }
        let dir = scratch("verify-partial-retry");
        let mut log = open(&dir);
        for n in 0..3 {
            log.record(conn_open(&format!("d{n}"), "phone")).unwrap();
        }
        drop(log);
        let journal = Retry {
            whole: Copy::of(&dir, OUR_EXE),
            readings: std::cell::Cell::new(0),
        };
        // Nothing missing: the first reading had the older records, the second the newest, and the part the second
        // could not read had been read whole the first time.
        let (code, out, err) = run(&dir, &["--verify"], &journal);
        assert_eq!((code, out.trim()), (0, "clean"), "{err}");
        assert_eq!(journal.readings.get(), 2);
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn one_failed_journal_reading_is_incomplete_not_clean() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("verify-partial");
        let mut log = open(&dir);
        log.record(conn_open("d1", "phone")).unwrap();
        drop(log);
        // A journalctl whose user reading fails and whose system reading holds every record.
        let base = dir.parent().unwrap();
        let entries = base.join("entries.jsonl");
        let text: String = Copy::of(&dir, OUR_EXE)
            .0
            .iter()
            .map(|e| serde_json::json!({ "_EXE": e.exe, "EITRI_RECORD": e.record }).to_string() + "\n")
            .collect();
        std::fs::write(&entries, text).unwrap();
        let bin = base.join("journalctl");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\ncase \"$*\" in *--user*) echo 'No journal files were opened' >&2; exit 1;; esac\ncat '{}'\n",
                entries.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let journal = Journalctl::at(Some(bin));
        let (code, out, err) = run(&dir, &["--verify"], &journal);
        assert_eq!(code, VERIFY_INCOMPLETE, "{out}{err}");
        assert!(err.contains("part of the journal could not be read"), "{err}");
        assert!(
            out.contains("No journal files were opened") && !out.contains("clean"),
            "{out}"
        );
        assert_eq!(journal.unread_parts().len(), 1);
        std::fs::remove_dir_all(base).unwrap();
    }

    /// Every reading returns `whole` (its newest record held back from the first reading when `late`), and reports
    /// the same `unread` parts each time.
    struct Parts {
        whole: Copy,
        unread: Vec<(String, String)>,
        late: bool,
        readings: std::cell::Cell<u32>,
    }

    impl JournalSource for Parts {
        fn entries(&self) -> io::Result<Vec<JournalEntry>> {
            self.readings.set(self.readings.get() + 1);
            let mut entries = self.whole.0.clone();
            if self.late && self.readings.get() == 1 {
                // `Copy` lists them newest first.
                entries.remove(0);
            }
            Ok(entries)
        }
        fn unread_parts(&self) -> Vec<(String, String)> {
            self.unread.clone()
        }
    }

    fn user_part_fails() -> Vec<(String, String)> {
        vec![("--user SYSLOG_IDENTIFIER=eitri-remote".into(), "nope".into())]
    }

    #[test]
    fn a_part_unread_in_both_readings_is_named_and_incomplete() {
        let dir = scratch("verify-unread-twice");
        let mut log = open(&dir);
        for n in 0..3 {
            log.record(conn_open(&format!("d{n}"), "phone")).unwrap();
        }
        drop(log);
        // The newest record's copy comes with the second reading; the user part fails both times.
        let journal = Parts {
            whole: Copy::of(&dir, OUR_EXE),
            unread: user_part_fails(),
            late: true,
            readings: std::cell::Cell::new(0),
        };
        let (code, out, err) = run(&dir, &["--verify"], &journal);
        assert_eq!(code, VERIFY_INCOMPLETE, "{out}{err}");
        assert_eq!(journal.readings.get(), 2, "read once more");
        assert_eq!(
            out.lines().collect::<Vec<_>>(),
            [
                "incomplete: nothing found, but not everything was checked",
                "  not checked: what only this part of the journal holds: journalctl --user \
                 SYSLOG_IDENTIFIER=eitri-remote: nope",
            ],
            "{out}"
        );
        assert!(
            err.contains("journalctl --user SYSLOG_IDENTIFIER=eitri-remote: nope"),
            "{err}"
        );
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_finding_beside_an_unread_part_is_a_finding() {
        let dir = scratch("verify-finding-and-unread");
        let mut log = open(&dir);
        for n in 0..3 {
            log.record(conn_open(&format!("d{n}"), "phone")).unwrap();
        }
        drop(log);
        let journal = Parts {
            whole: Copy::of(&dir, OUR_EXE),
            unread: user_part_fails(),
            late: false,
            readings: std::cell::Cell::new(0),
        };
        // An edit in the middle of the file.
        let path = dir.join("audit.jsonl");
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replacen("\"d1\"", "\"dX\"", 1)).unwrap();
        let (code, out, err) = run(&dir, &["--verify"], &journal);
        assert_eq!(code, 1, "{out}{err}");
        assert!(
            out.contains("altered in the file: ") && !out.contains("incomplete"),
            "{out}"
        );
        assert!(err.contains("part of the journal could not be read"), "{err}");
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn verify_reads_the_journal_once() {
        struct Counted(Copy, std::cell::Cell<u32>);
        impl JournalSource for Counted {
            fn entries(&self) -> io::Result<Vec<JournalEntry>> {
                self.1.set(self.1.get() + 1);
                self.0.entries()
            }
        }
        let dir = scratch("once");
        open(&dir).record(conn_open("d1", "phone")).unwrap();
        let journal = Counted(Copy::of(&dir, OUR_EXE), std::cell::Cell::new(0));
        assert_eq!(run(&dir, &["--verify"], &journal).0, 0);
        assert_eq!(journal.1.get(), 1, "each reading of the journal runs journalctl twice");
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_missing_log_is_said_and_is_no_error() {
        let dir = scratch("missing");
        let (code, out, err) = run(&dir, &[], &NoJournal);
        assert_eq!(code, 0, "{err}");
        assert!(out.is_empty(), "{out}");
        assert!(err.contains("no remote audit log"), "{err}");
        // `--verify` with no journal either cannot tell a log never written from one deleted whole.
        assert_eq!(run(&dir, &["--verify"], &NoJournal).0, VERIFY_JOURNAL_UNREAD);
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn since_takes_hours_days_and_rfc_3339() {
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(parse_since("24h", now), Some(now - Duration::from_secs(86_400)));
        assert_eq!(parse_since("7d", now), Some(now - Duration::from_secs(7 * 86_400)));
        assert_eq!(
            parse_since("2023-11-14T22:13:20Z", now),
            Some(UNIX_EPOCH + Duration::from_secs(1_700_000_000))
        );
        assert_eq!(
            parse_since("2023-11-15T06:13:20.250+08:00", now),
            Some(UNIX_EPOCH + Duration::from_millis(1_700_000_000_250))
        );
        for bad in [
            "",
            "h",
            "-1h",
            "24",
            "1w",
            "2023-11-14",
            "2023-13-01T00:00:00Z",
            "2023-11-14T22:13:20",
        ] {
            assert_eq!(parse_since(bad, now), None, "{bad:?}");
        }
    }

    #[test]
    fn remote_test_hooks_only_in_dev_dependencies() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let manifests = workspace_manifests(&root);
        assert!(
            manifests.len() > 10,
            "the workspace's manifests were not found: {manifests:?}"
        );
        let mut defined = 0;
        let mut enabled = 0;
        for path in &manifests {
            let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            let uses = hook_uses(&text);
            let wrong: Vec<&HookUse> = uses.iter().filter(|u| **u == HookUse::Elsewhere).collect();
            assert!(
                wrong.is_empty(),
                "{} names {HOOKS} outside a dev-dependency: {uses:?}",
                path.display()
            );
            defined += uses.iter().filter(|u| **u == HookUse::Defined).count();
            enabled += uses.iter().filter(|u| **u == HookUse::DevDependency).count();
        }
        assert_eq!(defined, 2, "eitri-core and eitri-remote each define the feature");
        assert!(
            enabled >= 1,
            "no test build enables the feature: the scan guards nothing"
        );

        // The scan itself: each way of reaching a shipped build is caught, wherever the line breaks.
        for text in [
            "[dependencies]\neitri-remote = { path = \"../remote\", features = [\"remote-test-hooks\"] }\n",
            "[dependencies.eitri-core]\npath = \"../core\"\nfeatures = [\n  \"remote-test-hooks\",\n]\n",
            "[target.'cfg(unix)'.dependencies]\neitri-core = { path = \"../core\", features = [\"remote-test-hooks\"] }\n",
            "[build-dependencies]\nx = { path = \"x\", features = [\"remote-test-hooks\"] }\n",
            "[features]\ndefault = [\n \"remote-test-hooks\"\n]\nremote-test-hooks = []\n",
            "[workspace.dependencies]\neitri-core = { path = \"core\", features = [\"remote-test-hooks\"] }\n",
        ] {
            assert!(hook_uses(text).contains(&HookUse::Elsewhere), "not caught: {text}");
        }
        for text in [
            "[dev-dependencies]\neitri-remote = { path = \"../remote\", features = [\"remote-test-hooks\"] }\n",
            "[target.'cfg(unix)'.dev-dependencies]\neitri-core = { path = \"../core\", features = [\n\"remote-test-hooks\"] }\n",
            "[features]\nremote-test-hooks = [\n  \"eitri-core/remote-test-hooks\",\n]\n",
            "[dependencies]\n# remote-test-hooks is never named here\nx = \"1\"\n",
        ] {
            assert!(!hook_uses(text).contains(&HookUse::Elsewhere), "refused: {text}");
        }
    }
}
