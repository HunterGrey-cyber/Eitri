use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use super::segment::invalid;
use super::FileSystem;

type Poisoned = BTreeMap<PathBuf, (String, u64, u64)>;
pub(super) static POISONED: OnceLock<Mutex<Poisoned>> = OnceLock::new();

pub(super) struct Poison {
    dir: PathBuf,
    boot: String,
    pub recovered: Option<(u64, u64)>,
    pub stale: Option<(u64, u64)>,
}
impl Poison {
    pub fn open(fs: &dyn FileSystem, dir: &Path) -> io::Result<Self> {
        let boot = fs.boot_id()?;
        if boot.is_empty() || boot.len() > 128 || boot.bytes().any(|b| b.is_ascii_whitespace()) {
            return Err(invalid("invalid boot identity"));
        }
        let dir = fs.state_root(dir)?;
        let mut recovered = None;
        let mut stale = None;
        match fs.read(&dir.join("poisoned")) {
            Ok(bytes) => {
                let fields: Vec<_> = std::str::from_utf8(&bytes)
                    .map_err(|_| invalid("invalid poison marker"))?
                    .split_whitespace()
                    .collect();
                if fields.len() != 3 || bytes.len() > 256 {
                    return Err(invalid("invalid poison marker"));
                }
                let first = fields[1].parse::<u64>().map_err(|_| invalid("invalid poison range"))?;
                let last = fields[2].parse::<u64>().map_err(|_| invalid("invalid poison range"))?;
                if first == 0 || last < first {
                    return Err(invalid("invalid poison range"));
                }
                if fields[0] == boot {
                    recovered = Some((first, last));
                } else {
                    stale = Some((first, last));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        // A full or failing disk can refuse the marker too. Keep the refusal for in-process reopens.
        if let Some((saved_boot, first, last)) = POISONED.get_or_init(Mutex::default).lock().unwrap().get(&dir) {
            if *saved_boot == boot {
                recovered = Some(match recovered {
                    Some((a, b)) => (a.min(*first), b.max(*last)),
                    None => (*first, *last),
                });
            }
        }
        Ok(Self {
            dir,
            boot,
            recovered,
            stale,
        })
    }
    pub fn clear_stale(&self, fs: &dyn FileSystem) -> io::Result<()> {
        if self.stale.is_some() && self.recovered.is_none() {
            fs.remove_file(&self.dir.join("poisoned"))?;
            fs.sync_dir(&self.dir)?;
        }
        Ok(())
    }
    // The caller serializes saves with the syncer's I/O lock. Preserve the original write error even
    // if persisting its refusal fails; the process registry still prevents an unsafe local reopen.
    pub fn save(&self, fs: &dyn FileSystem, first: u64, last: u64) {
        let mut registry = POISONED.get_or_init(Mutex::default).lock().unwrap();
        let range = registry
            .entry(self.dir.clone())
            .or_insert((self.boot.clone(), first, last));
        if range.0 != self.boot {
            *range = (self.boot.clone(), first, last);
        } else {
            range.1 = range.1.min(first);
            range.2 = range.2.max(last);
        }
        let bytes = format!("{}\n{} {}\n", range.0, range.1, range.2);
        drop(registry);
        let _ = fs.replace(&self.dir.join("poisoned"), bytes.as_bytes());
    }
}
