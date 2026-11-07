//! Reading the log back: every retained file, newest record first.

use super::{chain, parse_time, rotate, AUDIT_ROTATE_BYTES};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::time::SystemTime;

/// The largest file read; rotation keeps each near [`AUDIT_ROTATE_BYTES`], so a much bigger one is not the log's.
const FILE_READ_MAX: u64 = 4 * AUDIT_ROTATE_BYTES;

/// One line of the log.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub v: u32,
    pub time: String,
    pub writer: String,
    pub run: String,
    pub seq: u64,
    pub event: String,
    pub prev: String,
    /// The event's own fields.
    pub fields: Map<String, Value>,
    /// The line as it is in the file, without its newline.
    pub line: String,
}

impl Record {
    /// The record a line holds, `None` for a line that is not one.
    pub fn parse(line: &str) -> Option<Record> {
        let mut fields: Map<String, Value> = serde_json::from_str(line).ok()?;
        let mut text = |key: &str| match fields.remove(key)? {
            Value::String(s) => Some(s),
            _ => None,
        };
        let (time, writer, run, event, prev) = (
            text("time")?,
            text("writer")?,
            text("run")?,
            text("event")?,
            text("prev")?,
        );
        let v = u32::try_from(fields.remove("v")?.as_u64()?).ok()?;
        let seq = fields.remove("seq")?.as_u64()?;
        Some(Record {
            v,
            time,
            writer,
            run,
            seq,
            event,
            prev,
            fields,
            line: line.to_string(),
        })
    }

    /// The record's time.
    pub fn at(&self) -> Option<SystemTime> {
        parse_time(&self.time)
    }

    /// The device the record is about, by id.
    pub fn device(&self) -> Option<&str> {
        self.fields.get("device").and_then(Value::as_str)
    }
}

/// Which records [`read`] returns.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    /// At or after this time.
    pub since: Option<SystemTime>,
    /// About this device, by its id or its name.
    pub device: Option<String>,
    /// About this device, by its id only.
    pub device_id: Option<String>,
    /// Of this event.
    pub event: Option<String>,
}

/// The records of every retained file that pass `filter`, newest first, at most `limit`. A line that is not a
/// record is skipped (`verify` names it).
pub fn read(dir: &Path, filter: &Filter, limit: Option<usize>) -> io::Result<Vec<Record>> {
    let mut out = Vec::new();
    for (_, bytes) in rotate::snapshot(dir)?.into_iter().rev() {
        for line in chain::split_lines(&bytes).into_iter().rev() {
            if limit.is_some_and(|n| out.len() >= n) {
                return Ok(out);
            }
            let Some(record) = std::str::from_utf8(line).ok().and_then(Record::parse) else {
                continue;
            };
            if filter.passes(&record) {
                out.push(record);
            }
        }
    }
    Ok(out)
}

/// The newest records about each of `devices` (by id), at most `per_device` of each, newest first, from one pass over
/// the retained files: what a list of devices shows, without reading the log once per device. A device with no record
/// has an empty list; the pass stops once every device has its `per_device`.
pub fn newest_by_device(dir: &Path, devices: &[&str], per_device: usize) -> io::Result<HashMap<String, Vec<Record>>> {
    let mut out: HashMap<String, Vec<Record>> = devices.iter().map(|d| ((*d).to_owned(), Vec::new())).collect();
    let mut wanting = if per_device == 0 { 0 } else { out.len() };
    for (_, bytes) in rotate::snapshot(dir)?.into_iter().rev() {
        for line in chain::split_lines(&bytes).into_iter().rev() {
            if wanting == 0 {
                return Ok(out);
            }
            let Some(record) = std::str::from_utf8(line).ok().and_then(Record::parse) else {
                continue;
            };
            let Some(kept) = record.device().and_then(|d| out.get_mut(d)) else {
                continue;
            };
            if kept.len() < per_device {
                kept.push(record);
                if kept.len() == per_device {
                    wanting -= 1;
                }
            }
        }
    }
    Ok(out)
}

impl Filter {
    fn passes(&self, record: &Record) -> bool {
        let since = self.since.is_none_or(|since| record.at().is_some_and(|at| at >= since));
        let device = self.device.as_deref().is_none_or(|wanted| {
            ["device", "device_name"]
                .iter()
                .any(|key| record.fields.get(*key).and_then(Value::as_str) == Some(wanted))
        });
        let device_id = self
            .device_id
            .as_deref()
            .is_none_or(|wanted| record.device() == Some(wanted));
        let event = self.event.as_deref().is_none_or(|wanted| record.event == wanted);
        since && device && device_id && event
    }
}

/// A file of the log, or `None` when it is not there: a regular file of this user, not followed through a link.
pub(super) fn read_file(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match agent::private_fs::read_private_nofollow_bytes_capped(path, FILE_READ_MAX) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::super::*;
    use super::*;

    #[test]
    fn read_filters_since_device_event_newest_first() {
        let dir = audit_dir_for("audit-read");
        let clock = FakeClock::at_ms(days_ms(20_000));
        let mut log = open_at(&dir, WriterKind::Serve, None, JournalTarget::Off, &clock);
        log.rotate_bytes = 1200;
        for n in 0..10 {
            clock.set_ms(days_ms(20_000) + n * 1000);
            log.record(cmd(if n % 2 == 0 { "even" } else { "odd" }, n)).unwrap();
        }
        clock.set_ms(days_ms(20_000) + 10_000);
        log.record(Event::Revoke {
            device: device("odd"),
            by: "desk".into(),
        })
        .unwrap();
        assert!(dir.join(rotated_name(1)).exists(), "the read spans files");

        let all = read(&dir, &Filter::default(), None).unwrap();
        let revisions: Vec<_> = all
            .iter()
            .map(|r| r.fields.get("revision").and_then(Value::as_u64))
            .collect();
        assert_eq!(all.len(), 11);
        assert_eq!(all[0].event, "revoke", "newest first");
        assert_eq!(
            revisions[1..],
            (0..10).rev().map(Some).collect::<Vec<_>>()[..],
            "newest first across files"
        );
        assert_eq!(all[0].line, lines(&dir).last().unwrap().as_str());
        assert_eq!(all[0].seq, 11);
        assert_eq!(all[0].writer, "serve");

        let since = UNIX_EPOCH + Duration::from_millis(days_ms(20_000) + 7000);
        let recent = read(
            &dir,
            &Filter {
                since: Some(since),
                ..Filter::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(recent.iter().map(|r| r.seq).collect::<Vec<_>>(), vec![11, 10, 9, 8]);

        let odd = read(
            &dir,
            &Filter {
                device: Some("odd".into()),
                ..Filter::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(odd.len(), 6);
        assert!(odd.iter().all(|r| r.device() == Some("odd")));
        let by_name = read(
            &dir,
            &Filter {
                device: Some("phone odd".into()),
                ..Filter::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(by_name, odd, "a device is found by its name too");

        let revokes = read(
            &dir,
            &Filter {
                event: Some("revoke".into()),
                device: Some("odd".into()),
                ..Filter::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(revokes.len(), 1);

        let two = read(&dir, &Filter::default(), Some(2)).unwrap();
        assert_eq!(two.iter().map(|r| r.seq).collect::<Vec<_>>(), vec![11, 10]);
    }

    #[test]
    fn newest_by_device_matches_a_read_per_device_in_one_pass() {
        let dir = audit_dir_for("audit-read-by-device");
        let clock = FakeClock::at_ms(days_ms(20_000));
        let mut log = open_at(&dir, WriterKind::Serve, None, JournalTarget::Off, &clock);
        log.rotate_bytes = 1200;
        for n in 0..12 {
            clock.set_ms(days_ms(20_000) + n * 1000);
            log.record(cmd(["a", "b", "c"][(n % 3) as usize], n)).unwrap();
        }
        assert!(dir.join(rotated_name(1)).exists(), "the read spans files");
        let found = newest_by_device(&dir, &["a", "b", "none"], 3).unwrap();
        for id in ["a", "b", "none"] {
            let one = read(
                &dir,
                &Filter {
                    device_id: Some(id.into()),
                    ..Filter::default()
                },
                Some(3),
            )
            .unwrap();
            assert_eq!(found[id], one, "{id}");
        }
        assert_eq!(found["a"].len(), 3);
        assert!(found["none"].is_empty());
        assert!(!found.contains_key("c"), "only the devices asked for");
        assert!(newest_by_device(&dir, &["a"], 0).unwrap()["a"].is_empty());
        assert!(newest_by_device(&audit_dir_for("audit-read-by-device-none"), &["a"], 3).unwrap()["a"].is_empty());
    }

    #[test]
    fn a_line_that_is_not_a_record_is_skipped() {
        let dir = audit_dir_for("audit-read-bad");
        let mut log = AuditLog::open(&dir, WriterKind::Serve, None, JournalTarget::Off).unwrap();
        log.record(cmd("d1", 1)).unwrap();
        let mut text = std::fs::read_to_string(dir.join(CURRENT)).unwrap();
        text.push_str("not json\n{\"v\":1}\n");
        std::fs::write(dir.join(CURRENT), text).unwrap();
        let all = read(&dir, &Filter::default(), None).unwrap();
        assert_eq!(all.len(), 1);
        assert!(read(&audit_dir_for("audit-read-none"), &Filter::default(), None)
            .unwrap()
            .is_empty());
    }
}
