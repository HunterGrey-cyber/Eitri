use std::io::{self, Write};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use sha2::{Digest, Sha256};

use super::{GapWhy, Record};

pub const FRAME_BYTES: usize = 29;
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

/// Storage operations are injectable so short writes, sync failures and replacement failures are reproducible.
pub trait FileSystem: Send + Sync {
    fn boot_id(&self) -> io::Result<String> {
        Ok(std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
            .trim()
            .to_owned())
    }
    fn create_dir(&self, dir: &Path) -> io::Result<()> {
        let mut missing = Vec::new();
        for ancestor in dir.ancestors() {
            if ancestor.as_os_str().is_empty() || ancestor.try_exists()? {
                break;
            }
            missing.push(ancestor);
        }
        agent::private_fs::create_private_dir_all(dir, dir)?;
        for created in missing.into_iter().rev() {
            self.sync_dir(
                created
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new(".")),
            )?;
        }
        Ok(())
    }
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        agent::private_fs::read_private_nofollow_bytes_capped(path, u64::MAX - 1)
    }
    fn open_segment(&self, path: &Path) -> io::Result<Box<dyn SegmentReader>> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)?;
        let meta = file.metadata()?;
        if !meta.is_file() || meta.uid() != agent::private_fs::euid() || meta.mode() & 0o022 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "record is not a private regular file",
            ));
        }
        Ok(Box::new(file))
    }
    fn read_at(&self, path: &Path, offset: u64, size: usize) -> io::Result<Vec<u8>> {
        self.open_segment(path)?.read_at(offset, size)
    }
    /// Accounting may inspect a non-private directory, but never follows a directory link.
    fn accounting_entries(&self, dir: &Path) -> io::Result<Vec<FileEntry>> {
        let directory = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(dir)?;
        std::fs::read_dir(format!("/proc/self/fd/{}", std::os::fd::AsRawFd::as_raw_fd(&directory)))?
            .map(|entry| {
                let entry = entry?;
                let path = dir.join(entry.file_name());
                let meta = path.symlink_metadata()?;
                Ok(FileEntry {
                    path,
                    directory: meta.is_dir(),
                    bytes: meta.len(),
                })
            })
            .collect()
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }
    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_dir(path)
    }
    fn state_root(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }
    fn append(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        agent::private_fs::append_private(path)?.write_all(bytes)
    }
    fn truncate(&self, path: &Path, len: u64) -> io::Result<()> {
        agent::private_fs::append_private(path)?.set_len(len)
    }
    fn sync(&self, path: &Path) -> io::Result<()> {
        agent::private_fs::append_private(path)?.sync_all()
    }
    fn sync_dir(&self, path: &Path) -> io::Result<()> {
        std::fs::File::open(path)?.sync_all()
    }
    fn entries(&self, dir: &Path) -> io::Result<Vec<FileEntry>> {
        dir.symlink_metadata()?;
        if !agent::private_fs::is_private_dir(dir) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "state directory is not private",
            ));
        }
        std::fs::read_dir(dir)?
            .map(|entry| {
                let entry = entry?;
                let meta = entry.path().symlink_metadata()?;
                Ok(FileEntry {
                    path: entry.path(),
                    directory: meta.is_dir(),
                    bytes: meta.len(),
                })
            })
            .collect()
    }
    /// A failed replacement preserves the previous file, except when only the final directory sync failed.
    fn replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let parent = path.parent().ok_or_else(|| invalid("file has no parent"))?;
        self.create_dir(parent)?;
        let temp = parent.join(format!(
            ".journal-{}-{}.tmp",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| {
            let mut file = agent::private_fs::open_private(&temp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            self.rename(&temp, path)?;
            self.sync_dir(parent)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(temp);
        }
        result
    }
}

/// A positioned reader kept open for consecutive records in one segment.
pub trait SegmentReader {
    fn read_at(&self, offset: u64, size: usize) -> io::Result<Vec<u8>>;
}
impl SegmentReader for std::fs::File {
    fn read_at(&self, offset: u64, size: usize) -> io::Result<Vec<u8>> {
        let mut bytes = vec![0; size];
        self.read_exact_at(&mut bytes, offset)?;
        Ok(bytes)
    }
}

pub(super) fn sync_parent(fs: &dyn FileSystem, path: &Path) -> io::Result<()> {
    fs.sync_dir(
        path.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )
}

pub struct FileEntry {
    pub path: PathBuf,
    pub directory: bool,
    pub bytes: u64,
}

pub struct Disk;
impl FileSystem for Disk {}

pub trait Clock: Send + Sync {
    fn elapsed(&self) -> Duration;
    fn now(&self) -> SystemTime;
}

pub struct SystemClock(Instant);
impl Default for SystemClock {
    fn default() -> Self {
        Self(Instant::now())
    }
}
impl Clock for SystemClock {
    fn elapsed(&self) -> Duration {
        self.0.elapsed()
    }
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

pub(super) fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

pub(super) fn corruption(path: &Path, offset: usize, why: &str) -> io::Error {
    invalid(format!("{} at offset {offset}: {why}", path.display()))
}

pub(super) fn bounds(record: &Record) -> (u64, u64) {
    match record {
        Record::Event { sequence, .. } => (*sequence, *sequence),
        Record::Gap { first, last, .. } => (*first, *last),
    }
}

pub(super) fn encode(record: &Record) -> io::Result<Vec<u8>> {
    let (kind, bytes): (u8, &[u8]) = match record {
        Record::Event { bytes, .. } => (0, bytes),
        Record::Gap {
            why: GapWhy::WriteFailed,
            ..
        } => (1, &[]),
        Record::Gap {
            why: GapWhy::NotObserved,
            ..
        } => (2, &[]),
        Record::Gap {
            why: GapWhy::Evicted, ..
        } => (3, &[]),
    };
    let length = u32::try_from(bytes.len()).map_err(|_| invalid("event exceeds the frame length field"))?;
    let (first, last) = bounds(record);
    if first == 0 || last < first {
        return Err(invalid("invalid journal range"));
    }
    let mut frame = Vec::with_capacity(FRAME_BYTES + bytes.len());
    frame.push(kind);
    frame.extend(first.to_le_bytes());
    frame.extend(last.to_le_bytes());
    frame.extend(length.to_le_bytes());
    frame.extend(bytes);
    let digest = Sha256::digest(&frame);
    frame.extend(&digest[..8]);
    Ok(frame)
}

/// `None` means an incomplete frame. A complete frame with an invalid checksum is always an error.
pub(super) fn decode(path: &Path, data: &[u8], offset: usize) -> io::Result<Option<(Record, usize)>> {
    let tail = &data[offset..];
    if tail.iter().all(|byte| *byte == 0) || tail.len() < 21 {
        return Ok(None);
    }
    let first = u64::from_le_bytes(tail[1..9].try_into().unwrap());
    let last = u64::from_le_bytes(tail[9..17].try_into().unwrap());
    let length = u32::from_le_bytes(tail[17..21].try_into().unwrap()) as usize;
    if first == 0 || last < first || tail[0] > 3 || (tail[0] == 0 && first != last) || (tail[0] != 0 && length != 0) {
        return Err(corruption(path, offset, "invalid frame header"));
    }
    let size = length
        .checked_add(FRAME_BYTES)
        .ok_or_else(|| corruption(path, offset, "length overflow"))?;
    if size > tail.len() {
        // A valid later frame proves this is damaged framing in the middle, rather than a torn final write.
        for next in 21..tail.len().saturating_sub(FRAME_BYTES - 1) {
            let candidate = &tail[next..];
            let n = u32::from_le_bytes(candidate[17..21].try_into().unwrap()) as usize;
            if let Some(end) = n.checked_add(FRAME_BYTES).filter(|end| *end <= candidate.len()) {
                if candidate[0] <= 3 && Sha256::digest(&candidate[..end - 8])[..8] == candidate[end - 8..end] {
                    return Err(corruption(path, offset, "damaged length before a later frame"));
                }
            }
        }
        return Ok(None);
    }
    if Sha256::digest(&tail[..size - 8])[..8] != tail[size - 8..size] {
        return Err(corruption(path, offset, "checksum mismatch"));
    }
    let record = if tail[0] == 0 {
        Record::Event {
            sequence: first,
            bytes: tail[21..size - 8].to_vec(),
        }
    } else {
        let why = match tail[0] {
            1 => GapWhy::WriteFailed,
            2 => GapWhy::NotObserved,
            _ => GapWhy::Evicted,
        };
        Record::Gap { first, last, why }
    };
    Ok(Some((record, size)))
}

// The name carries the refusal before truncation, so eviction needs no newly allocated file.
pub(super) fn evicted_path(dir: &Path, first: u64, last: u64) -> PathBuf {
    dir.join(format!("evicted-{first:020}-{last:020}.log"))
}
pub(super) fn evicted_range(path: &Path) -> io::Result<Option<(u64, u64)>> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let Some(rest) = name.strip_prefix("evicted-") else {
        return Ok(None);
    };
    let parsed = (|| {
        let rest = rest.strip_suffix(".log")?;
        let (first, last) = rest.split_once('-')?;
        if first.len() != 20 || last.len() != 20 || !first.bytes().chain(last.bytes()).all(|b| b.is_ascii_digit()) {
            return None;
        }
        let first: u64 = first.parse().ok()?;
        let last: u64 = last.parse().ok()?;
        (first > 0 && last >= first).then_some((first, last))
    })();
    parsed
        .map(Some)
        .ok_or_else(|| corruption(path, 0, "invalid eviction range"))
}

// Length flips within the remaining segment space (or any length flip at offset zero), and a final
// record zeroed through EOF, cannot always be distinguished from a torn write by this frame format.
pub(super) fn validate_tail(
    path: &Path,
    data: &[u8],
    offset: usize,
    expected: u64,
    segment_bytes: u64,
) -> io::Result<()> {
    let tail = &data[offset..];
    if tail.iter().all(|byte| *byte == 0) {
        return Ok(());
    }
    if tail.len() < 21 {
        let prefix = expected.to_le_bytes();
        let first_end = tail.len().min(9);
        if tail[0] > 3 || tail[1..first_end] != prefix[..first_end - 1] {
            return Err(corruption(path, offset, "unreadable final header"));
        }
        if tail.len() > 9 {
            let last_end = tail.len().min(17);
            let last_prefix = &tail[9..last_end];
            let mut largest_last = [255; 8];
            largest_last[..last_prefix.len()].copy_from_slice(last_prefix);
            if (tail[0] == 0 && last_prefix != &prefix[..last_prefix.len()])
                || u64::from_le_bytes(largest_last) < expected
                || (tail[0] != 0 && tail[17.min(tail.len())..].iter().any(|byte| *byte != 0))
            {
                return Err(corruption(path, offset, "damaged final header"));
            }
        }
        return Ok(());
    }
    let first = u64::from_le_bytes(tail[1..9].try_into().unwrap());
    let size = u64::from(u32::from_le_bytes(tail[17..21].try_into().unwrap())) + FRAME_BYTES as u64;
    if first != expected || (offset > 0 && (offset as u64).saturating_add(size) > segment_bytes) {
        return Err(corruption(path, offset, "damaged final header"));
    }
    Ok(())
}

pub(super) fn crash_temp(path: &Path) -> bool {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let Some(rest) = name
        .strip_prefix(".journal-")
        .and_then(|rest| rest.strip_suffix(".tmp"))
    else {
        return false;
    };
    if let Some(pid) = rest
        .split_once('-')
        .and_then(|(pid, _)| pid.parse::<i32>().ok())
        .filter(|pid| *pid > 0)
    {
        // A live writer may still be using its temporary in a different session directory.
        // SAFETY: signal zero only probes the process; it sends no signal and requires no pointers.
        if unsafe { libc::kill(pid, 0) } == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
            return false;
        }
    }
    true
}

pub(super) fn clean_temps(fs: &dyn FileSystem, dir: &Path) -> io::Result<()> {
    let mut removed = false;
    for entry in fs.entries(dir)? {
        if !entry.directory && crash_temp(&entry.path) {
            fs.remove_file(&entry.path)?;
            removed = true;
        }
    }
    if removed {
        fs.sync_dir(dir)?;
    }
    Ok(())
}

pub(super) fn segment_paths(fs: &dyn FileSystem, dir: &Path) -> io::Result<Vec<(u64, PathBuf)>> {
    let mut paths = Vec::new();
    for entry in fs.entries(dir)? {
        if let Some((first, _)) = evicted_range(&entry.path)? {
            if entry.directory {
                return Err(corruption(&entry.path, 0, "eviction marker is a directory"));
            }
            paths.push((first, entry.path));
            continue;
        }
        let name = entry.path.file_name().unwrap_or_default().to_string_lossy();
        if !name.starts_with("seg-") || !name.ends_with(".log") {
            continue;
        }
        let digits = &name[4..name.len() - 4];
        if entry.directory || digits.len() != 20 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid(format!("invalid segment name: {}", entry.path.display())));
        }
        let first = digits.parse().map_err(|_| invalid("segment sequence overflow"))?;
        paths.push((first, entry.path));
    }
    paths.sort_by_key(|(first, _)| *first);
    Ok(paths)
}
