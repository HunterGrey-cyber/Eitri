use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::segment::{clean_temps, invalid, sync_parent};
use super::{Disk, FileSystem};

type Requests = BTreeMap<String, (u64, Vec<u8>)>;
const MAX_PENDING_BYTES: usize = 16 * 1024 * 1024;

/// Independently durable permission requests, in `pending/<session>.bin`.
/// The caller clears them when a turn ends or a session closes, before accepting any later answer.
pub struct Pending {
    path: PathBuf,
    fs: Arc<dyn FileSystem>,
    requests: Requests,
    dirty: bool,
}
impl Pending {
    pub fn open(path: &Path) -> io::Result<Self> {
        Self::open_with(path, Arc::new(Disk))
    }
    pub fn open_with(path: &Path, fs: Arc<dyn FileSystem>) -> io::Result<Self> {
        let parent = path.parent().ok_or_else(|| invalid("pending file has no parent"))?;
        fs.create_dir(parent)?;
        sync_parent(&*fs, parent)?;
        clean_temps(&*fs, parent)?;
        let bytes = match fs.read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        if bytes.len() > MAX_PENDING_BYTES {
            return Err(invalid("pending requests exceed the size limit"));
        }
        let requests = decode_requests(&bytes)?;
        if requests
            .iter()
            .any(|(id, (sequence, _))| id.is_empty() || *sequence == 0)
        {
            return Err(invalid("invalid pending request identity"));
        }
        Ok(Self {
            path: path.into(),
            fs,
            requests,
            dirty: false,
        })
    }
    pub fn request(&mut self, sequence: u64, event: &[u8], permission_id: &str) -> io::Result<()> {
        if sequence == 0 || permission_id.is_empty() {
            return Err(invalid("invalid pending request identity"));
        }
        if let Some((previous, bytes)) = self.requests.get(permission_id) {
            return if *previous == sequence && bytes == event {
                if self.dirty {
                    self.rewrite(self.requests.clone())
                } else {
                    Ok(())
                }
            } else {
                Err(invalid("permission id is already pending"))
            };
        }
        let mut next = self.requests.clone();
        next.insert(permission_id.into(), (sequence, event.to_vec()));
        self.rewrite(next)
    }
    pub fn resolved(&mut self, permission_id: &str) -> io::Result<()> {
        let mut next = self.requests.clone();
        next.remove(permission_id);
        self.rewrite(next)
    }
    /// A turn's end or the session's close settles every request still listed: none can be answered after it.
    pub fn clear(&mut self) -> io::Result<()> {
        self.rewrite(BTreeMap::new())
    }
    fn rewrite(&mut self, next: Requests) -> io::Result<()> {
        let bytes = encode_requests(&next)?;
        match self.fs.replace(&self.path, &bytes) {
            Ok(()) => {
                self.requests = next;
                self.dirty = false;
                Ok(())
            }
            Err(error) => {
                self.requests = next;
                self.dirty = true;
                Err(error)
            }
        }
    }
    pub fn list(&self) -> Vec<(u64, Vec<u8>)> {
        let mut requests: Vec<_> = self.requests.values().cloned().collect();
        requests.sort_by_key(|(sequence, _)| *sequence);
        requests
    }
}

fn encode_requests(requests: &Requests) -> io::Result<Vec<u8>> {
    if requests.is_empty() {
        return Ok(Vec::new());
    }
    let size = requests
        .iter()
        .try_fold(8usize, |size, (id, (_, event))| {
            size.checked_add(16)?.checked_add(id.len())?.checked_add(event.len())
        })
        .filter(|size| *size <= MAX_PENDING_BYTES)
        .ok_or_else(|| invalid("pending requests exceed the size limit"))?;
    let mut bytes = Vec::with_capacity(size);
    bytes.extend(b"EPN1");
    bytes.extend((requests.len() as u32).to_le_bytes());
    for (id, (sequence, event)) in requests {
        bytes.extend((id.len() as u32).to_le_bytes());
        bytes.extend((event.len() as u32).to_le_bytes());
        bytes.extend(sequence.to_le_bytes());
        bytes.extend(id.as_bytes());
        bytes.extend(event);
    }
    Ok(bytes)
}
fn decode_requests(bytes: &[u8]) -> io::Result<Requests> {
    if bytes.is_empty() {
        return Ok(BTreeMap::new());
    }
    let Some(mut tail) = bytes.strip_prefix(b"EPN1") else {
        return Err(invalid("invalid pending request format"));
    };
    if tail.len() < 4 {
        return Err(invalid("incomplete pending request count"));
    }
    let count = u32::from_le_bytes(tail[..4].try_into().unwrap());
    tail = &tail[4..];
    let mut requests = BTreeMap::new();
    for _ in 0..count {
        if tail.len() < 16 {
            return Err(invalid("incomplete pending request header"));
        }
        let id_len = u32::from_le_bytes(tail[..4].try_into().unwrap()) as usize;
        let event_len = u32::from_le_bytes(tail[4..8].try_into().unwrap()) as usize;
        let sequence = u64::from_le_bytes(tail[8..16].try_into().unwrap());
        tail = &tail[16..];
        let size = id_len
            .checked_add(event_len)
            .filter(|size| *size <= tail.len())
            .ok_or_else(|| invalid("incomplete pending request"))?;
        let id = std::str::from_utf8(&tail[..id_len]).map_err(|_| invalid("invalid pending request id"))?;
        if requests
            .insert(id.into(), (sequence, tail[id_len..size].to_vec()))
            .is_some()
        {
            return Err(invalid("duplicate pending request id"));
        }
        tail = &tail[size..];
    }
    if !tail.is_empty() {
        return Err(invalid("trailing pending request bytes"));
    }
    Ok(requests)
}
