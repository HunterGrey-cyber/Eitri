//! Durable per-session event history. Callers serialize writers and never evict a live session.
mod budget;
mod pending;
mod poison;
mod segment;

pub use budget::{
    enforce_budget, enforce_budget_with, BudgetOutcome, BudgetPressure, Evicted, FreeSpace, SessionUsage,
};
pub use pending::Pending;
pub use segment::{Clock, Disk, FileEntry, FileSystem, SegmentReader, SystemClock};

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use poison::Poison;
use segment::{
    bounds, clean_temps, corruption, decode, encode, evicted_range, invalid, segment_paths, sync_parent, validate_tail,
};

#[derive(Clone, Debug)]
pub struct JournalConfig {
    pub segment_bytes: u64,
    pub fsync_every: Duration,
    pub cap_bytes: u64,
    pub free_floor_bytes: u64,
    pub closed_keep: Duration,
}
impl Default for JournalConfig {
    fn default() -> Self {
        Self {
            segment_bytes: 4 * 1024 * 1024,
            fsync_every: Duration::from_millis(200),
            cap_bytes: 1024 * 1024 * 1024,
            free_floor_bytes: 1024 * 1024 * 1024,
            closed_keep: Duration::from_secs(2 * 24 * 60 * 60),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GapWhy {
    WriteFailed,
    NotObserved,
    Evicted,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Record {
    Event { sequence: u64, bytes: Vec<u8> },
    Gap { first: u64, last: u64, why: GapWhy },
}

pub type JournalError = io::Error;

#[derive(Debug)]
pub enum ReadError {
    Gap { first: u64, last: u64, why: GapWhy },
    Io(io::Error),
}
impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Gap { first, last, why } => write!(f, "journal gap {first}..={last}: {why:?}"),
            Self::Io(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for ReadError {}
impl From<io::Error> for ReadError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

struct Location {
    path: PathBuf,
    offset: usize,
    size: usize,
}
struct Entry {
    first: u64,
    last: u64,
    why: Option<GapWhy>,
    location: Option<Location>,
}
struct Segment {
    path: PathBuf,
    bytes: u64,
    sealed: bool,
}

struct Dirty {
    deadline: Duration,
    first: u64,
    last: u64,
}
#[derive(Default)]
struct SyncState {
    dirty: BTreeMap<PathBuf, Dirty>,
    error: Option<io::Error>,
    inflight: BTreeMap<PathBuf, (u64, u64)>,
    failed: Vec<(u64, u64)>,
    poison_last: u64,
    stop: bool,
}
#[cfg(test)]
enum FailureIoEvent {
    Waiting,
    Returned,
}
struct Syncer {
    state: Arc<(Mutex<SyncState>, Condvar)>,
    io_lock: Arc<Mutex<()>>,
    worker: Option<JoinHandle<()>>,
    poison: Arc<Poison>,
    fs: Arc<dyn FileSystem>,
    clock: Arc<dyn Clock>,
    #[cfg(test)]
    failure_wait: Mutex<Option<std::sync::mpsc::Sender<FailureIoEvent>>>,
}
fn copy_error(error: &io::Error) -> io::Error {
    error
        .raw_os_error()
        .map(io::Error::from_raw_os_error)
        .unwrap_or_else(|| io::Error::new(error.kind(), error.to_string()))
}
impl Syncer {
    fn new(fs: Arc<dyn FileSystem>, clock: Arc<dyn Clock>, poison: Poison) -> io::Result<Self> {
        let mut initial = SyncState::default();
        if let Some((first, last)) = poison.recovered {
            initial.error = Some(io::Error::other("journal was poisoned in this boot"));
            initial.failed.push((first, last));
            initial.poison_last = last;
        }
        let poison = Arc::new(poison);
        let marker = poison.clone();
        let state = Arc::new((Mutex::new(initial), Condvar::new()));
        let io_lock = Arc::new(Mutex::new(()));
        let (shared, disk, time, lock) = (state.clone(), fs.clone(), clock.clone(), io_lock.clone());
        let worker = std::thread::Builder::new().name("journal-sync".into()).spawn(move || {
            loop {
                let mut state = shared.0.lock().unwrap();
                if state.stop {
                    break;
                }
                let deadline = state.dirty.values().map(|dirty| dirty.deadline).min();
                if state.error.is_none() && deadline.is_some_and(|deadline| deadline <= time.elapsed()) {
                    drop(state);
                    flush_due(&shared, &lock, &*disk, &*time, &marker, false);
                    continue;
                }
                state = if state.error.is_none() {
                    if let Some(deadline) = deadline {
                        shared
                            .1
                            .wait_timeout(state, deadline.saturating_sub(time.elapsed()))
                            .unwrap()
                            .0
                    } else {
                        shared.1.wait(state).unwrap()
                    }
                } else {
                    shared.1.wait(state).unwrap()
                };
                drop(state);
            }
            flush_due(&shared, &lock, &*disk, &*time, &marker, true);
        })?;
        Ok(Self {
            state,
            io_lock,
            worker: Some(worker),
            poison,
            fs,
            clock,
            #[cfg(test)]
            failure_wait: Mutex::new(None),
        })
    }
    fn failure_io(&self) -> std::sync::MutexGuard<'_, ()> {
        #[cfg(test)]
        if let Some(observer) = self.failure_wait.lock().unwrap().as_ref() {
            if matches!(self.io_lock.try_lock(), Err(std::sync::TryLockError::WouldBlock)) {
                observer.send(FailureIoEvent::Waiting).unwrap();
            }
        }
        self.io_lock.lock().unwrap()
    }
    fn cover_failure(&self, last: u64) {
        if self.state.0.lock().unwrap().error.is_none() {
            return;
        }
        let _io = self.failure_io();
        let range = {
            let mut state = self.state.0.lock().unwrap();
            if state.error.is_none() {
                return;
            }
            state.poison_last = state.poison_last.max(last);
            poison_range(&state)
        };
        if let Some((first, last)) = range {
            self.poison.save(&*self.fs, first, last);
        }
    }
    fn flush(&self, all: bool) -> io::Result<()> {
        flush_due(&self.state, &self.io_lock, &*self.fs, &*self.clock, &self.poison, all);
        self.state
            .0
            .lock()
            .unwrap()
            .error
            .as_ref()
            .map_or(Ok(()), |error| Err(copy_error(error)))
    }
}
fn flush_due(
    shared: &(Mutex<SyncState>, Condvar),
    io_lock: &Mutex<()>,
    fs: &dyn FileSystem,
    clock: &dyn Clock,
    poison: &Poison,
    all: bool,
) {
    // Only flushers serialize on I/O. Writers can mark another generation dirty while fsync is in flight.
    let _io = io_lock.lock().unwrap();
    let batch = {
        let mut state = shared.0.lock().unwrap();
        if state.error.is_some() {
            return;
        }
        let paths: Vec<_> = state
            .dirty
            .iter()
            .filter(|(_, dirty)| all || dirty.deadline <= clock.elapsed())
            .map(|(path, _)| path.clone())
            .collect();
        paths
            .into_iter()
            .map(|path| {
                let dirty = state.dirty.remove(&path).unwrap();
                state.inflight.insert(path.clone(), (dirty.first, dirty.last));
                (path, dirty)
            })
            .collect::<Vec<_>>()
    };
    for (path, _) in &batch {
        if let Err(error) = fs.sync(path).and_then(|()| fs.sync_dir(path.parent().unwrap())) {
            let mut state = shared.0.lock().unwrap();
            fail_sync(&mut state, error);
            let range = poison_range(&state);
            drop(state);
            if let Some((first, last)) = range {
                poison.save(fs, first, last);
            }
            shared.1.notify_all();
            return;
        }
        let mut state = shared.0.lock().unwrap();
        state.inflight.remove(path);
        if state.error.is_some() {
            return;
        }
    }
}
fn fail_sync(state: &mut SyncState, error: io::Error) {
    if state.error.is_none() {
        state.error = Some(error);
    }
    let mut failed = std::mem::take(&mut state.failed);
    failed.extend(state.dirty.values().map(|dirty| (dirty.first, dirty.last)));
    failed.extend(state.inflight.values().copied());
    failed.sort_unstable();
    for (first, last) in failed {
        if let Some((_, end)) = state
            .failed
            .last_mut()
            .filter(|(_, end)| end.saturating_add(1) >= first)
        {
            *end = (*end).max(last);
        } else {
            state.failed.push((first, last));
        }
    }
    state.dirty.clear();
    state.inflight.clear();
}
fn poison_range(state: &SyncState) -> Option<(u64, u64)> {
    let first = state.failed.first()?.0;
    let last = state.failed.iter().map(|(_, last)| *last).max()?.max(state.poison_last);
    Some((first, last))
}
impl Drop for Syncer {
    fn drop(&mut self) {
        self.state.0.lock().unwrap().stop = true;
        self.state.1.notify_one();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// One writer owns a session directory such as `journal/<session>` for this object's lifetime.
/// A sync failure covers unsynced sequences with WriteFailed and poisons same-boot reopens via a
/// boot-tagged marker. After reboot, only frames that verify on disk are recovered. If the failing
/// disk also refuses the marker, only in-process reopens are protected; restart requires a reboot.
pub struct Journal {
    dir: PathBuf,
    config: JournalConfig,
    fs: Arc<dyn FileSystem>,
    syncer: Syncer,
    entries: Vec<Entry>,
    unwritten: VecDeque<usize>,
    segments: Vec<Segment>,
    head: u64,
}
impl Journal {
    pub fn open(dir: &Path, config: &JournalConfig) -> io::Result<Self> {
        Self::open_with(dir, config, Arc::new(Disk), Arc::new(SystemClock::default()))
    }
    pub fn open_with(
        dir: &Path,
        config: &JournalConfig,
        fs: Arc<dyn FileSystem>,
        clock: Arc<dyn Clock>,
    ) -> io::Result<Self> {
        if config.segment_bytes == 0 || config.fsync_every.is_zero() || config.fsync_every > Duration::from_millis(200)
        {
            return Err(invalid(
                "segment size must be positive and fsync interval must be in (0, 200 ms]",
            ));
        }
        fs.create_dir(dir)?;
        sync_parent(&*fs, dir)?;
        if let Some(parent) = dir.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            sync_parent(&*fs, parent)?;
        }
        let mut poison = Poison::open(&*fs, dir)?;
        clean_temps(&*fs, dir)?;
        let paths = segment_paths(&*fs, dir)?;
        let mut entries = Vec::new();
        let mut segments = Vec::new();
        let mut head = 0u64;
        let mut repaired_through = None;
        for (index, (name_first, path)) in paths.iter().enumerate() {
            // A replacement gap is durable before later uncertain segments are removed. Repeating
            // cleanup after an interrupted open must not try to replay those overlapping segments.
            if repaired_through.is_some_and(|last| *name_first <= last) {
                fs.remove_file(path)?;
                fs.sync_dir(dir)?;
                continue;
            }
            if let Some((first, last)) = evicted_range(path)? {
                if Some(first) != head.checked_add(1) {
                    return Err(corruption(path, 0, "eviction range is not dense"));
                }
                fs.read_at(path, 0, 0)?;
                entries.push(Entry {
                    first,
                    last,
                    why: Some(GapWhy::Evicted),
                    location: None,
                });
                segments.push(Segment {
                    path: path.clone(),
                    bytes: 0,
                    sealed: true,
                });
                head = last;
                continue;
            }
            if poison
                .recovered
                .is_some_and(|(first, last)| head < last && *name_first >= first)
            {
                break;
            }
            let mut data = fs.read(path)?;
            let mut offset = 0;
            if *name_first != head.checked_add(1).ok_or_else(|| invalid("sequence overflow"))? {
                return Err(corruption(path, 0, "segment is not dense"));
            }
            while offset < data.len() || (offset == 0 && data.is_empty() && poison.stale.is_some()) {
                if poison
                    .recovered
                    .is_some_and(|(first, _)| head.checked_add(1) == Some(first))
                {
                    break;
                }
                let decoded = decode(path, &data, offset);
                let expected = head.checked_add(1).ok_or_else(|| invalid("sequence overflow"))?;
                if let Some((_, last)) = poison
                    .stale
                    .filter(|(first, last)| expected >= *first && expected <= *last)
                {
                    if !matches!(&decoded, Ok(Some((record, _))) if bounds(record).0 == expected) {
                        // Atomic replacement keeps either the damaged bytes and their marker, or
                        // a verified prefix followed by a durable refusal, across a helper crash.
                        data.truncate(offset);
                        data.extend(encode(&Record::Gap {
                            first: expected,
                            last,
                            why: GapWhy::WriteFailed,
                        })?);
                        fs.replace(path, &data)?;
                        continue;
                    }
                }
                let Some((record, size)) = decoded? else {
                    if index + 1 != paths.len() {
                        return Err(corruption(path, offset, "torn record before the final segment"));
                    }
                    validate_tail(
                        path,
                        &data,
                        offset,
                        head.checked_add(1).ok_or_else(|| invalid("sequence overflow"))?,
                        config.segment_bytes,
                    )?;
                    fs.truncate(path, offset as u64)?;
                    fs.sync(path)?;
                    break;
                };
                let (first, last) = bounds(&record);
                if Some(first) != head.checked_add(1) {
                    return Err(corruption(path, offset, "record is not dense"));
                }
                let why = match record {
                    Record::Gap { why, .. } => Some(why),
                    _ => None,
                };
                if why == Some(GapWhy::WriteFailed) && poison.stale.is_some_and(|(a, b)| first >= a && last == b) {
                    repaired_through = Some(last);
                }
                entries.push(Entry {
                    first,
                    last,
                    why,
                    location: Some(Location {
                        path: path.clone(),
                        offset,
                        size,
                    }),
                });
                head = last;
                offset += size;
            }
            if offset == 0 && index + 1 != paths.len() {
                return Err(corruption(path, 0, "empty segment before the final segment"));
            }
            segments.push(Segment {
                path: path.clone(),
                bytes: offset as u64,
                sealed: false,
            });
        }
        if let Some((first, last)) = poison.recovered {
            if head >= last {
                poison.recovered = None;
            } else if head.saturating_add(1) < first {
                return Err(invalid("poisoned range does not follow the trusted journal prefix"));
            } else {
                let first = head + 1;
                poison.recovered = Some((first, last));
                entries.push(Entry {
                    first,
                    last,
                    why: Some(GapWhy::WriteFailed),
                    location: None,
                });
                head = last;
            }
        }
        poison.clear_stale(&*fs)?;
        let syncer = Syncer::new(fs.clone(), clock, poison)?;
        {
            let mut state = syncer.state.0.lock().unwrap();
            for entry in &entries {
                if let Some(location) = &entry.location {
                    state
                        .dirty
                        .entry(location.path.clone())
                        .and_modify(|dirty| dirty.last = entry.last)
                        .or_insert(Dirty {
                            deadline: syncer.clock.elapsed().saturating_add(config.fsync_every),
                            first: entry.first,
                            last: entry.last,
                        });
                }
            }
        }
        syncer.state.1.notify_one();
        Ok(Self {
            dir: dir.into(),
            config: config.clone(),
            fs,
            syncer,
            entries,
            unwritten: VecDeque::new(),
            segments,
            head,
        })
    }
    pub fn head(&self) -> u64 {
        self.head
    }

    /// Duplicate and backwards sequences are refused. Unobserved sequences become an explicit gap first.
    pub fn append(&mut self, sequence: u64, bytes: &[u8]) -> Result<(), JournalError> {
        if sequence <= self.head {
            return Err(invalid("sequence must advance the journal head"));
        }
        if bytes.len() > u32::MAX as usize {
            return Err(invalid("event exceeds the frame length field"));
        }
        if sequence > self.head + 1 {
            self.queue_gap(self.head + 1, sequence - 1, GapWhy::NotObserved);
        }
        let record = Record::Event {
            sequence,
            bytes: bytes.to_vec(),
        };
        let result = self.persist_unwritten().and_then(|()| self.write_record(&record));
        match result {
            Ok(location) => {
                self.entries.push(Entry {
                    first: sequence,
                    last: sequence,
                    why: None,
                    location: Some(location),
                });
                self.head = sequence;
                Ok(())
            }
            Err(error) => {
                self.queue_gap(sequence, sequence, GapWhy::WriteFailed);
                // A completely full filesystem can refuse even this small frame. Keep it visible in memory
                // and retry it before any later event; a pending permission lives in a separate file.
                let _ = self.persist_unwritten();
                Err(error)
            }
        }
    }
    pub fn record_gap(&mut self, first: u64, last: u64, why: GapWhy) -> io::Result<()> {
        if Some(first) != self.head.checked_add(1) || first == 0 || last < first {
            return Err(invalid("gap must extend the journal densely"));
        }
        self.queue_gap(first, last, why);
        self.persist_unwritten()
    }
    fn queue_gap(&mut self, first: u64, last: u64, why: GapWhy) {
        if why == GapWhy::WriteFailed {
            self.syncer.cover_failure(last);
            let queued_last = self.unwritten.back().copied() == self.entries.len().checked_sub(1);
            if let Some(previous) = self.entries.last_mut().filter(|entry| {
                entry.location.is_none()
                    && entry.why == Some(why)
                    && entry.last.checked_add(1) == Some(first)
                    && queued_last
            }) {
                previous.last = last;
                self.head = last;
                return;
            }
        }
        self.unwritten.push_back(self.entries.len());
        self.entries.push(Entry {
            first,
            last,
            why: Some(why),
            location: None,
        });
        self.head = last;
    }
    fn persist_unwritten(&mut self) -> io::Result<()> {
        while let Some(&i) = self.unwritten.front() {
            let entry = &self.entries[i];
            let record = Record::Gap {
                first: entry.first,
                last: entry.last,
                why: entry.why.unwrap(),
            };
            let location = self.write_record(&record)?;
            self.entries[i].location = Some(location);
            self.unwritten.pop_front();
        }
        Ok(())
    }
    fn write_record(&mut self, record: &Record) -> io::Result<Location> {
        let frame = encode(record)?;
        let mut sync = self.syncer.state.0.lock().unwrap();
        if let Some(error) = &sync.error {
            let error = copy_error(error);
            drop(sync);
            self.syncer.cover_failure(self.head);
            return Err(error);
        }
        if self.segments.last().is_none_or(|segment| {
            segment.sealed
                || (segment.bytes != 0 && segment.bytes.saturating_add(frame.len() as u64) > self.config.segment_bytes)
        }) {
            let path = self.dir.join(format!("seg-{:020}.log", bounds(record).0));
            self.segments.push(Segment {
                path,
                bytes: 0,
                sealed: false,
            });
        }
        let segment = self.segments.last_mut().unwrap();
        if let Err(error) = self.fs.append(&segment.path, &frame) {
            if self.fs.truncate(&segment.path, segment.bytes).is_err() {
                sync.failed.push(bounds(record));
                fail_sync(&mut sync, copy_error(&error));
                drop(sync);
                self.syncer.cover_failure(bounds(record).1);
            }
            return Err(error);
        }
        let location = Location {
            path: segment.path.clone(),
            offset: segment.bytes as usize,
            size: frame.len(),
        };
        segment.bytes += frame.len() as u64;
        let (first, last) = bounds(record);
        sync.dirty
            .entry(segment.path.clone())
            .and_modify(|dirty| dirty.last = last)
            .or_insert_with(|| Dirty {
                deadline: self.syncer.clock.elapsed().saturating_add(self.config.fsync_every),
                first,
                last,
            });
        drop(sync);
        self.syncer.state.1.notify_one();
        Ok(location)
    }
    /// Returns the prefix before a gap; reading again after that prefix reports the gap.
    /// A positive byte budget always permits one event so an oversized event cannot stall replay.
    pub fn read_after(&self, after: u64, max_bytes: usize) -> Result<Vec<Record>, ReadError> {
        if max_bytes == 0 {
            return Ok(Vec::new());
        }
        let mut records = Vec::new();
        let mut used = 0usize;
        let failed = {
            let state = self.syncer.state.0.lock().unwrap();
            if state.failed.is_empty() {
                Vec::new()
            } else {
                drop(state);
                let _io = self.syncer.failure_io();
                self.syncer.state.0.lock().unwrap().failed.clone()
            }
        };
        let mut reader: Option<(&Path, Box<dyn SegmentReader>)> = None;
        for entry in &self.entries[self.entries.partition_point(|entry| entry.last <= after)..] {
            if let Some(&(first, last)) = failed
                .iter()
                .find(|(first, last)| entry.first <= *last && entry.last >= *first)
            {
                if !records.is_empty() {
                    break;
                }
                return Err(ReadError::Gap {
                    first,
                    last,
                    why: GapWhy::WriteFailed,
                });
            }
            if let Some(why) = entry.why {
                if !records.is_empty() {
                    break;
                }
                return Err(ReadError::Gap {
                    first: entry.first,
                    last: entry.last,
                    why,
                });
            }
            let location = entry.location.as_ref().unwrap();
            if !records.is_empty() && location.size > max_bytes.saturating_sub(used) {
                break;
            }
            if reader.as_ref().is_none_or(|(path, _)| *path != location.path) {
                reader = Some((
                    &location.path,
                    self.fs
                        .open_segment(&location.path)
                        .map_err(|error| corruption(&location.path, location.offset, &error.to_string()))?,
                ));
            }
            let data = reader
                .as_ref()
                .unwrap()
                .1
                .read_at(location.offset as u64, location.size)
                .map_err(|error| corruption(&location.path, location.offset, &error.to_string()))?;
            let (record, size) = decode(&location.path, &data, 0)
                .map_err(|error| corruption(&location.path, location.offset, &error.to_string()))?
                .ok_or_else(|| corruption(&location.path, location.offset, "incomplete record"))?;
            if bounds(&record) != (entry.first, entry.last)
                || size != location.size
                || !matches!(record, Record::Event { .. })
            {
                return Err(corruption(&location.path, location.offset, "record changed").into());
            }
            used = used.saturating_add(size);
            records.push(record);
            if used >= max_bytes {
                break;
            }
        }
        Ok(records)
    }
    pub fn turn_ended(&mut self) -> io::Result<()> {
        let written = self.persist_unwritten();
        let synced = self.syncer.flush(true);
        written.and(synced)
    }
    /// Drives the same deadline check as the worker, useful with an injected clock.
    pub fn sync_due(&self) -> io::Result<()> {
        self.syncer.flush(false)
    }
}

impl Drop for Journal {
    fn drop(&mut self) {
        let _ = self.persist_unwritten();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let mut random = [0u8; 16];
            std::fs::File::open("/dev/urandom")
                .unwrap()
                .read_exact(&mut random)
                .unwrap();
            let name: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
            let root = std::env::temp_dir().join(format!("eitri-journal-{name}"));
            Disk.create_dir(&root).unwrap();
            Self(root)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    #[derive(Default)]
    struct ManualClock(AtomicU64);
    impl ManualClock {
        fn advance(&self, ms: u64) {
            self.0.fetch_add(ms, Ordering::SeqCst);
        }
    }
    impl Clock for ManualClock {
        fn elapsed(&self) -> Duration {
            Duration::from_millis(self.0.load(Ordering::SeqCst))
        }
        fn now(&self) -> SystemTime {
            UNIX_EPOCH + Duration::from_secs(10 * 24 * 60 * 60) + self.elapsed()
        }
    }
    #[derive(Default)]
    struct Faults {
        fail_sequence: AtomicU64,
        full: AtomicBool,
        fail_replace: AtomicBool,
        fail_rename: AtomicBool,
        fail_sync: AtomicBool,
        syncs: AtomicUsize,
    }
    impl FileSystem for Faults {
        fn append(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
            if self.full.load(Ordering::SeqCst) {
                return Err(io::Error::from_raw_os_error(libc::ENOSPC));
            }
            let sequence = u64::from_le_bytes(bytes[1..9].try_into().unwrap());
            if bytes[0] == 0
                && self
                    .fail_sequence
                    .compare_exchange(sequence, 0, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            {
                Disk.append(path, &bytes[..7])?;
                return Err(io::Error::from_raw_os_error(libc::ENOSPC));
            }
            Disk.append(path, bytes)
        }
        fn sync(&self, path: &Path) -> io::Result<()> {
            self.syncs.fetch_add(1, Ordering::SeqCst);
            if self.fail_sync.load(Ordering::SeqCst) {
                return Err(io::Error::from_raw_os_error(libc::EIO));
            }
            Disk.sync(path)
        }
        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            if self.fail_rename.load(Ordering::SeqCst) {
                return Err(io::Error::from_raw_os_error(libc::EIO));
            }
            Disk.rename(from, to)
        }
        fn replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
            if self.full.load(Ordering::SeqCst) || self.fail_replace.load(Ordering::SeqCst) {
                return Err(io::Error::from_raw_os_error(libc::ENOSPC));
            }
            Disk.replace(path, bytes)
        }
    }
    fn config() -> JournalConfig {
        JournalConfig {
            segment_bytes: 1024,
            free_floor_bytes: 0,
            ..JournalConfig::default()
        }
    }
    fn gap(result: Result<Vec<Record>, ReadError>, first: u64, last: u64, why: GapWhy) {
        assert!(matches!(result, Err(ReadError::Gap { first: f, last: l, why: w }) if (f, l, w) == (first, last, why)));
    }
    fn create_session(root: &Path, name: &str, closed_at: Option<SystemTime>) -> SessionUsage {
        let dir = root.join("journal").join(name);
        let mut journal = Journal::open(
            &dir,
            &JournalConfig {
                segment_bytes: 4096,
                ..config()
            },
        )
        .unwrap();
        for sequence in 1..=8 {
            journal.append(sequence, &[7; 100]).unwrap();
        }
        journal.turn_ended().unwrap();
        let bytes = Disk.entries(&dir).unwrap().iter().map(|entry| entry.bytes).sum();
        SessionUsage {
            session: name.into(),
            bytes,
            closed_at,
        }
    }
    fn plentiful(_: &Path) -> io::Result<u64> {
        Ok(u64::MAX)
    }
    fn bytes_under(dir: &Path) -> u64 {
        Disk.entries(dir)
            .unwrap()
            .into_iter()
            .map(|entry| {
                if entry.directory {
                    bytes_under(&entry.path)
                } else {
                    entry.bytes
                }
            })
            .sum()
    }

    #[test]
    fn append_then_read_after_any_point_returns_the_rest() {
        let root = Scratch::new();
        let mut journal = Journal::open(root.path(), &config()).unwrap();
        let mut expected = Vec::new();
        let mut seed = 17u64;
        for sequence in 1..=2000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let bytes = vec![(seed >> 32) as u8; (seed % 64) as usize];
            journal.append(sequence, &bytes).unwrap();
            expected.push(Record::Event { sequence, bytes });
        }
        journal.turn_ended().unwrap();
        drop(journal);
        let journal = Journal::open(root.path(), &config()).unwrap();
        for after in 0..=2000 {
            assert_eq!(
                journal.read_after(after, usize::MAX).unwrap(),
                expected[after as usize..]
            );
        }
        assert_eq!(journal.head(), 2000);
    }
    #[test]
    fn a_torn_tail_is_cut_at_open() {
        let root = Scratch::new();
        let first = encode(&Record::Event {
            sequence: 1,
            bytes: b"first".to_vec(),
        })
        .unwrap();
        let last = encode(&Record::Event {
            sequence: 2,
            bytes: b"last".to_vec(),
        })
        .unwrap();
        let path = root.path().join("seg-00000000000000000001.log");
        for cut in 1..last.len() {
            let mut bytes = first.clone();
            bytes.extend(&last[..cut]);
            Disk.replace(&path, &bytes).unwrap();
            let mut journal = Journal::open(root.path(), &config()).unwrap();
            assert_eq!(journal.head(), 1);
            assert_eq!(Disk.read(&path).unwrap(), first);
            journal.append(2, b"replacement").unwrap();
            journal.turn_ended().unwrap();
        }
    }
    #[test]
    fn a_corrupted_middle_record_is_an_error_not_a_skip() {
        let root = Scratch::new();
        let mut journal = Journal::open(root.path(), &config()).unwrap();
        for sequence in 1..=3 {
            journal.append(sequence, b"payload").unwrap();
        }
        drop(journal);
        let path = segment_paths(&Disk, root.path()).unwrap()[0].1.clone();
        let mut bytes = Disk.read(&path).unwrap();
        let offset = 29 + b"payload".len();
        bytes[offset + 21] ^= 1;
        Disk.replace(&path, &bytes).unwrap();
        let error = Journal::open(root.path(), &config()).err().unwrap();
        assert!(error.to_string().contains(&path.display().to_string()));
        assert!(error.to_string().contains(&format!("offset {offset}")));
        assert_eq!(Disk.read(&path).unwrap(), bytes);
    }
    #[test]
    fn a_damaged_middle_length_is_not_treated_as_a_torn_tail() {
        let root = Scratch::new();
        let mut journal = Journal::open(root.path(), &config()).unwrap();
        for sequence in 1..=3 {
            journal.append(sequence, b"payload").unwrap();
        }
        drop(journal);
        let path = segment_paths(&Disk, root.path()).unwrap()[0].1.clone();
        let mut bytes = Disk.read(&path).unwrap();
        bytes[36 + 17..36 + 21].copy_from_slice(&u32::MAX.to_le_bytes());
        Disk.replace(&path, &bytes).unwrap();
        assert!(Journal::open(root.path(), &config()).is_err());
        assert_eq!(Disk.read(&path).unwrap(), bytes);
    }
    #[test]
    fn fsync_happens_by_200_ms_and_at_turn_end() {
        let root = Scratch::new();
        let clock = Arc::new(ManualClock::default());
        let fs = Arc::new(Faults::default());
        let mut journal = Journal::open_with(root.path(), &config(), fs.clone(), clock.clone()).unwrap();
        journal.append(1, b"one").unwrap();
        clock.advance(199);
        journal.sync_due().unwrap();
        assert_eq!(fs.syncs.load(Ordering::SeqCst), 0);
        journal.append(2, b"two").unwrap();
        clock.advance(1);
        journal.sync_due().unwrap();
        assert_eq!(fs.syncs.load(Ordering::SeqCst), 1);
        journal.append(3, b"three").unwrap();
        journal.turn_ended().unwrap();
        assert_eq!(fs.syncs.load(Ordering::SeqCst), 2);
        journal.sync_due().unwrap();
        assert_eq!(fs.syncs.load(Ordering::SeqCst), 2);
    }
    #[test]
    fn an_idle_write_is_synced_without_another_append() {
        struct Notify(std::sync::mpsc::Sender<()>);
        impl FileSystem for Notify {
            fn sync(&self, path: &Path) -> io::Result<()> {
                Disk.sync(path)?;
                self.0.send(()).unwrap();
                Ok(())
            }
        }
        let root = Scratch::new();
        let (tx, rx) = std::sync::mpsc::channel();
        let mut journal = Journal::open_with(
            root.path(),
            &config(),
            Arc::new(Notify(tx)),
            Arc::new(SystemClock::default()),
        )
        .unwrap();
        journal.append(1, b"idle").unwrap();
        rx.recv_timeout(Duration::from_secs(60))
            .expect("sync worker did not process the dirty segment");
    }
    #[test]
    fn enospc_records_a_gap_and_keeps_the_pending_request() {
        let root = Scratch::new();
        let fs = Arc::new(Faults::default());
        fs.fail_sequence.store(40, Ordering::SeqCst);
        let dir = root.path().join("journal/session");
        let pending_path = root.path().join("pending/session.bin");
        let mut pending = Pending::open(&pending_path).unwrap();
        pending.request(41, b"permission event", "permission-41").unwrap();
        let mut journal = Journal::open_with(&dir, &config(), fs, Arc::new(ManualClock::default())).unwrap();
        for sequence in 1..40 {
            journal.append(sequence, b"event").unwrap();
        }
        assert_eq!(
            journal.append(40, b"lost").unwrap_err().raw_os_error(),
            Some(libc::ENOSPC)
        );
        journal.append(41, b"permission event").unwrap();
        journal.turn_ended().unwrap();
        drop(journal);
        let journal = Journal::open(&dir, &config()).unwrap();
        let prefix = journal.read_after(30, usize::MAX).unwrap();
        assert_eq!(
            prefix,
            (31..40)
                .map(|sequence| Record::Event {
                    sequence,
                    bytes: b"event".to_vec()
                })
                .collect::<Vec<_>>()
        );
        gap(journal.read_after(39, usize::MAX), 40, 40, GapWhy::WriteFailed);
        assert_eq!(journal.read_after(40, usize::MAX).unwrap().len(), 1);
        assert_eq!(
            Pending::open(&pending_path).unwrap().list(),
            vec![(41, b"permission event".to_vec())]
        );
    }
    #[test]
    fn a_gap_that_cannot_be_written_is_visible_and_retried_before_later_events() {
        let root = Scratch::new();
        let fs = Arc::new(Faults::default());
        let mut journal =
            Journal::open_with(root.path(), &config(), fs.clone(), Arc::new(ManualClock::default())).unwrap();
        journal.append(1, b"first").unwrap();
        fs.full.store(true, Ordering::SeqCst);
        assert!(journal.append(2, b"lost").is_err());
        gap(journal.read_after(1, 100), 2, 2, GapWhy::WriteFailed);
        assert!(journal.turn_ended().is_err());
        fs.full.store(false, Ordering::SeqCst);
        journal.append(3, b"third").unwrap();
        drop(journal);
        let journal = Journal::open(root.path(), &config()).unwrap();
        gap(journal.read_after(1, 100), 2, 2, GapWhy::WriteFailed);
        assert_eq!(journal.head(), 3);
    }
    #[test]
    fn pending_clear_empties_the_file() {
        let root = Scratch::new();
        let path = root.path().join("pending/session.bin");
        let mut pending = Pending::open(&path).unwrap();
        pending.request(4, b"four", "four").unwrap();
        pending.request(2, b"two", "two").unwrap();
        assert_eq!(pending.list(), vec![(2, b"two".to_vec()), (4, b"four".to_vec())]);
        pending.resolved("two").unwrap();
        assert_eq!(Pending::open(&path).unwrap().list(), vec![(4, b"four".to_vec())]);
        pending.clear().unwrap();
        assert!(Disk.read(&path).unwrap().is_empty());
        assert!(Pending::open(&path).unwrap().list().is_empty());
    }
    #[test]
    fn a_failed_pending_clear_preserves_disk_and_removes_memory() {
        let root = Scratch::new();
        let path = root.path().join("pending/session.bin");
        let fs = Arc::new(Faults::default());
        let mut pending = Pending::open_with(&path, fs.clone()).unwrap();
        pending.request(1, b"one", "one").unwrap();
        fs.fail_replace.store(true, Ordering::SeqCst);
        assert!(pending.clear().is_err());
        assert_eq!(Pending::open(&path).unwrap().list(), vec![(1, b"one".to_vec())]);
        assert!(pending.list().is_empty());
        assert_eq!(Disk.entries(path.parent().unwrap()).unwrap().len(), 1);
    }
    #[test]
    fn eviction_takes_only_closed_sessions() {
        let root = Scratch::new();
        let clock = ManualClock::default();
        let older = create_session(root.path(), "older", Some(clock.now() - Duration::from_secs(2)));
        let newer = create_session(root.path(), "newer", Some(clock.now() - Duration::from_secs(1)));
        let live = create_session(root.path(), "live", None);
        let cfg = JournalConfig {
            cap_bytes: live.bytes + newer.bytes + 29,
            ..config()
        };
        let result = enforce_budget_with(root.path(), &[newer, live, older], &cfg, &plentiful, &Disk, &clock);
        assert!(
            matches!(result, BudgetOutcome::Evicted(ref ranges) if ranges.len() == 1 && ranges[0].session == "older")
        );
        let journal = Journal::open(&root.path().join("journal/live"), &cfg).unwrap();
        assert_eq!(journal.read_after(0, usize::MAX).unwrap().len(), 8);
        let journal = Journal::open(&root.path().join("journal/newer"), &cfg).unwrap();
        assert_eq!(journal.read_after(0, usize::MAX).unwrap().len(), 8);
    }
    #[test]
    fn live_sessions_over_the_cap_warn_and_keep_everything() {
        let root = Scratch::new();
        let live = create_session(root.path(), "live", None);
        let before = bytes_under(root.path());
        let cfg = JournalConfig {
            cap_bytes: 1,
            ..config()
        };
        assert!(
            matches!(enforce_budget(root.path(), &[live], &cfg, &plentiful), BudgetOutcome::LiveOverCap { live_bytes } if live_bytes == before)
        );
        assert_eq!(bytes_under(root.path()), before);
    }
    #[test]
    fn below_the_floor_with_only_live_sessions_warns() {
        let root = Scratch::new();
        let live = create_session(root.path(), "live", None);
        let before = bytes_under(root.path());
        let cfg = JournalConfig {
            free_floor_bytes: 100,
            ..config()
        };
        assert!(matches!(
            enforce_budget(root.path(), &[live], &cfg, &|_: &Path| Ok(99)),
            BudgetOutcome::BelowFloor { free: 99 }
        ));
        assert_eq!(bytes_under(root.path()), before);
    }
    #[test]
    fn evicted_ranges_read_as_gaps() {
        let root = Scratch::new();
        let clock = ManualClock::default();
        let closed = create_session(root.path(), "closed", Some(clock.now()));
        let cfg = JournalConfig {
            cap_bytes: 29,
            ..config()
        };
        assert!(matches!(
            enforce_budget_with(root.path(), &[closed], &cfg, &plentiful, &Disk, &clock),
            BudgetOutcome::Evicted(_)
        ));
        let journal = Journal::open(&root.path().join("journal/closed"), &cfg).unwrap();
        gap(journal.read_after(0, usize::MAX), 1, 8, GapWhy::Evicted);
        gap(journal.read_after(4, usize::MAX), 1, 8, GapWhy::Evicted);
        assert!(journal.read_after(8, usize::MAX).unwrap().is_empty());
        assert_eq!(journal.head(), 8);
    }
    #[test]
    fn budget_counts_review_and_logs_in_the_cap() {
        let root = Scratch::new();
        let clock = ManualClock::default();
        let closed = create_session(root.path(), "closed", Some(clock.now()));
        Disk.replace(&root.path().join("review/session.bin"), &[1; 800])
            .unwrap();
        Disk.replace(&root.path().join("logs/helper.log"), &[2; 800]).unwrap();
        let cfg = JournalConfig {
            cap_bytes: 1700,
            ..config()
        };
        assert!(closed.bytes < cfg.cap_bytes);
        assert!(matches!(
            enforce_budget_with(root.path(), &[closed], &cfg, &plentiful, &Disk, &clock),
            BudgetOutcome::Evicted(_)
        ));
        assert_eq!(bytes_under(root.path()), 1600);
    }
    #[test]
    fn free_floor_triggers_eviction_of_closed_sessions_before_the_cap() {
        let root = Scratch::new();
        let clock = ManualClock::default();
        let closed = create_session(root.path(), "closed", Some(clock.now()));
        let before = bytes_under(root.path());
        let free = |dir: &Path| Ok(10 + before - bytes_under(dir));
        let cfg = JournalConfig {
            free_floor_bytes: 100,
            ..config()
        };
        assert!(before < cfg.cap_bytes);
        assert!(matches!(
            enforce_budget_with(root.path(), &[closed], &cfg, &free, &Disk, &clock),
            BudgetOutcome::Evicted(_)
        ));
        assert!(free.available(root.path()).unwrap() >= cfg.free_floor_bytes);
    }
    #[test]
    fn closed_keep_expires_without_disk_pressure() {
        let root = Scratch::new();
        let clock = ManualClock::default();
        let cfg = config();
        let closed = create_session(root.path(), "closed", Some(clock.now() - cfg.closed_keep));
        assert!(matches!(
            enforce_budget_with(root.path(), &[closed], &cfg, &plentiful, &Disk, &clock),
            BudgetOutcome::Evicted(_)
        ));
    }
    #[test]
    fn accounting_and_eviction_errors_are_visible_and_preserve_the_segment() {
        let root = Scratch::new();
        let clock = ManualClock::default();
        let closed = create_session(root.path(), "closed", Some(clock.now()));
        let before = bytes_under(root.path());
        let fs = Faults::default();
        fs.fail_rename.store(true, Ordering::SeqCst);
        let cfg = JournalConfig {
            cap_bytes: 29,
            ..config()
        };
        assert!(matches!(
            enforce_budget_with(root.path(), &[closed], &cfg, &plentiful, &fs, &clock),
            BudgetOutcome::Partial { ref errors, .. } if !errors.is_empty()
        ));
        assert_eq!(bytes_under(root.path()), before);
        assert!(matches!(
            enforce_budget(root.path(), &[], &cfg, &|_: &Path| Err(io::Error::from_raw_os_error(
                libc::EIO
            ))),
            BudgetOutcome::Failed(_)
        ));
    }
    #[test]
    fn sequence_jumps_are_gaps_and_duplicate_sequences_are_refused() {
        let root = Scratch::new();
        let mut journal = Journal::open(root.path(), &config()).unwrap();
        journal.append(3, b"three").unwrap();
        gap(journal.read_after(0, 100), 1, 2, GapWhy::NotObserved);
        assert!(journal.append(3, b"duplicate").is_err());
        assert!(journal.record_gap(2, 4, GapWhy::NotObserved).is_err());
        assert!(journal.read_after(3, 100).unwrap().is_empty());
        assert_eq!(journal.read_after(2, 1).unwrap().len(), 1);
        assert!(journal.read_after(0, 0).unwrap().is_empty());
    }
    #[test]
    fn segments_rotate_at_the_configured_size_and_stay_private() {
        let root = Scratch::new();
        let cfg = JournalConfig {
            segment_bytes: 30,
            ..config()
        };
        let mut journal = Journal::open(root.path(), &cfg).unwrap();
        journal.append(1, b"a").unwrap();
        journal.append(2, b"b").unwrap();
        assert_eq!(segment_paths(&Disk, root.path()).unwrap().len(), 2);
        journal.turn_ended().unwrap();
        for entry in Disk.entries(root.path()).unwrap() {
            assert_eq!(
                std::fs::metadata(entry.path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(
            std::fs::metadata(root.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    #[test]
    fn background_sync_errors_remain_poisoned() {
        let root = Scratch::new();
        let fs = Arc::new(Faults::default());
        let mut journal =
            Journal::open_with(root.path(), &config(), fs.clone(), Arc::new(ManualClock::default())).unwrap();
        journal.append(1, b"first").unwrap();
        fs.fail_sync.store(true, Ordering::SeqCst);
        assert_eq!(journal.turn_ended().unwrap_err().raw_os_error(), Some(libc::EIO));
        fs.fail_sync.store(false, Ordering::SeqCst);
        assert_eq!(journal.turn_ended().unwrap_err().raw_os_error(), Some(libc::EIO));
        assert_eq!(journal.append(2, b"later").unwrap_err().raw_os_error(), Some(libc::EIO));
        gap(journal.read_after(0, 100), 1, 1, GapWhy::WriteFailed);
    }

    #[test]
    fn a_failed_write_after_a_sequence_jump_preserves_both_gaps() {
        let root = Scratch::new();
        let fs = Arc::new(Faults::default());
        let mut journal =
            Journal::open_with(root.path(), &config(), fs.clone(), Arc::new(ManualClock::default())).unwrap();
        journal.append(1, b"first").unwrap();
        fs.full.store(true, Ordering::SeqCst);
        assert!(journal.append(4, b"lost").is_err());
        assert_eq!(journal.head(), 4);
        gap(journal.read_after(1, 100), 2, 3, GapWhy::NotObserved);
        gap(journal.read_after(3, 100), 4, 4, GapWhy::WriteFailed);
        // The successful prefix still gets its turn-end sync when a gap cannot be persisted.
        assert!(journal.turn_ended().is_err());
        assert_eq!(fs.syncs.load(Ordering::SeqCst), 1);
        fs.full.store(false, Ordering::SeqCst);
        journal.append(5, b"last").unwrap();
        drop(journal);
        let journal = Journal::open(root.path(), &config()).unwrap();
        gap(journal.read_after(3, 100), 4, 4, GapWhy::WriteFailed);
        assert_eq!(journal.head(), 5);
    }

    #[test]
    fn complete_final_checksum_damage_and_earlier_torn_tails_are_refused() {
        let root = Scratch::new();
        let cfg = JournalConfig {
            segment_bytes: 30,
            ..config()
        };
        let mut journal = Journal::open(root.path(), &cfg).unwrap();
        journal.append(1, b"a").unwrap();
        journal.append(2, b"b").unwrap();
        drop(journal);
        let paths = segment_paths(&Disk, root.path()).unwrap();
        let mut bytes = Disk.read(&paths[1].1).unwrap();
        bytes[29] ^= 1;
        Disk.replace(&paths[1].1, &bytes).unwrap();
        assert!(Journal::open(root.path(), &cfg).is_err());
        bytes[29] ^= 1;
        Disk.replace(&paths[1].1, &bytes).unwrap();
        Disk.truncate(&paths[0].1, 29).unwrap();
        let error = Journal::open(root.path(), &cfg).err().unwrap();
        assert!(error.to_string().contains("offset 0"));
        assert_eq!(Disk.read(&paths[0].1).unwrap().len(), 29);
    }

    #[test]
    fn duplicate_session_usage_cannot_make_a_live_session_eligible_for_eviction() {
        let root = Scratch::new();
        let live = create_session(root.path(), "session", None);
        let closed = SessionUsage {
            session: live.session.clone(),
            bytes: live.bytes,
            closed_at: Some(UNIX_EPOCH),
        };
        let before = bytes_under(root.path());
        assert!(matches!(
            enforce_budget(root.path(), &[live, closed], &config(), &plentiful),
            BudgetOutcome::Failed(_)
        ));
        assert_eq!(bytes_under(root.path()), before);
    }

    #[test]
    fn replay_pages_make_progress_and_keep_the_byte_budget() {
        let root = Scratch::new();
        let mut journal = Journal::open(root.path(), &config()).unwrap();
        for sequence in 1..=20 {
            journal.append(sequence, &[7; 10]).unwrap();
        }
        let mut after = 0;
        while after < 20 {
            let records = journal.read_after(after, 80).unwrap();
            assert_eq!(records.len(), 2);
            assert_eq!(bounds(&records[0]).0, after + 1);
            assert_eq!(bounds(&records[1]).0, after + 2);
            after += 2;
        }
    }

    #[test]
    fn pending_replacement_failure_removes_its_temporary_and_preserves_other_files() {
        let root = Scratch::new();
        let target = root.path().join("target");
        Disk.create_dir(&target).unwrap();
        Disk.replace(&target.join("kept"), b"kept").unwrap();
        assert!(Disk.replace(&target, b"cannot replace a directory").is_err());
        assert_eq!(Disk.entries(root.path()).unwrap().len(), 1);
        assert_eq!(Disk.read(&target.join("kept")).unwrap(), b"kept");
    }

    #[test]
    fn budget_refuses_a_linked_session_directory() {
        let root = Scratch::new();
        let clock = ManualClock::default();
        let closed = create_session(root.path(), "closed", Some(clock.now()));
        let dir = root.path().join("journal/closed");
        let moved = root.path().join("kept");
        std::fs::rename(&dir, &moved).unwrap();
        std::os::unix::fs::symlink(&moved, &dir).unwrap();
        let before = bytes_under(&moved);
        let cfg = JournalConfig {
            cap_bytes: 1,
            ..config()
        };
        assert!(matches!(
            enforce_budget_with(root.path(), &[closed], &cfg, &plentiful, &Disk, &clock),
            BudgetOutcome::Partial { ref errors, .. } if !errors.is_empty()
        ));
        assert_eq!(bytes_under(&moved), before);
    }
    include!("regressions.rs");
}
