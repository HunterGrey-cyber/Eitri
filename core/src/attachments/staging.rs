use super::*;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// An opaque 32-hex token and its host-side ownership. Resolve page tokens with `Staging::resolve`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AttachmentId {
    token: uuid::Uuid,
    tab: u64,
    generation: u64,
}

impl AttachmentId {
    pub fn tab(self) -> u64 {
        self.tab
    }
    pub fn generation(self) -> u64 {
        self.generation
    }
}

impl fmt::Display for AttachmentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.token.simple().fmt(f)
    }
}

#[derive(Debug)]
pub enum StagingRefusal {
    Empty,
    TooLargeRaw { bytes: u64 },
    WindowQuota,
    UnknownId,
    WrongOwner,
    WrongOffset,
    ChunkTooLarge,
    Overrun,
    Incomplete,
    WrongDigest,
    WrongState,
    Io(io::Error),
}

impl StagingRefusal {
    pub fn sentence(&self) -> String {
        match self {
            Self::Empty => ImageRefusal::Empty.sentence(),
            Self::TooLargeRaw { bytes } => ImageRefusal::TooLargeRaw { bytes: *bytes }.sentence(),
            Self::WindowQuota => "this window has reached its 64 MB image limit".into(),
            Self::UnknownId => "this attachment is no longer available".into(),
            Self::WrongOwner => "this attachment belongs to another tab or draft".into(),
            Self::WrongOffset => "image chunks arrived out of order".into(),
            Self::ChunkTooLarge => "image chunk exceeds the 256 KB limit".into(),
            Self::Overrun => "image data exceeds its declared length".into(),
            Self::Incomplete => "image upload is incomplete".into(),
            Self::WrongDigest => "image upload failed its integrity check".into(),
            Self::WrongState => "this attachment is not ready for that operation".into(),
            Self::Io(_) => "image staging could not access its private files".into(),
        }
    }
}

impl From<io::Error> for StagingRefusal {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

// Only staging can create a readable job source; no path supplied by the page reaches the worker.
pub struct RawReady {
    pub id: AttachmentId,
    path: PathBuf,
    bytes: u64,
    sha256: [u8; 32],
}

impl RawReady {
    pub(super) fn read(&self) -> Result<Vec<u8>, ImageRefusal> {
        let raw = agent::private_fs::read_private_nofollow_bytes_capped(&self.path, self.bytes)
            .map_err(|_| ImageRefusal::Undecodable)?;
        if raw.len() as u64 != self.bytes || <[u8; 32]>::from(Sha256::digest(&raw)) != self.sha256 {
            return Err(ImageRefusal::Undecodable);
        }
        Ok(raw)
    }
}

enum State {
    Uploading {
        file: File,
        written: u64,
        digest: Sha256,
    },
    Ready,
    Stored,
    /// Read into memory by the caller and its file deleted: the bytes still count against the window's quota.
    InMemory,
}

struct Entry {
    id: AttachmentId,
    reserved: u64,
    state: State,
}

/// One window's private files. Its caller runs file operations off the window thread.
pub struct Staging {
    dir: PathBuf,
    entries: HashMap<uuid::Uuid, Entry>,
    reserved: u64,
}

impl Staging {
    #[cfg(test)]
    pub(super) fn reserved_bytes(&self) -> u64 {
        self.reserved
    }

    /// `base` is the host's runtime directory (`agent::state_dirs::runtime_dir`). A fresh window id
    /// creates `base/eitri/attachments/<pid>-<window id>`; no existing window directory is reused.
    pub fn open(base: &Path) -> io::Result<Self> {
        agent::private_fs::require_safe_parent(base, "image attachments")?;
        let root = base.join("eitri");
        let parent = root.join("attachments");
        // Establish each real parent before creating its children: the private-files helper also
        // supports user-configured directory links, which are inappropriate for transient uploads.
        ensure_private_directory(&root, &root)?;
        ensure_private_directory(&parent, &root)?;
        let dir = parent.join(format!("{}-{}", std::process::id(), uuid::Uuid::new_v4().simple()));
        match dir.symlink_metadata() {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "attachment window already exists",
                ))
            }
        }
        ensure_private_directory(&dir, &root)?;
        Ok(Self {
            dir,
            entries: HashMap::new(),
            reserved: 0,
        })
    }

    /// Binds a page's token to the tab and generation established by the host's admission check.
    pub fn resolve(&self, tab: u64, generation: u64, token: &str) -> Result<AttachmentId, StagingRefusal> {
        if token.len() != 32 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(StagingRefusal::UnknownId);
        }
        let token = uuid::Uuid::parse_str(token).map_err(|_| StagingRefusal::UnknownId)?;
        let id = self.entries.get(&token).ok_or(StagingRefusal::UnknownId)?.id;
        if id.tab != tab || id.generation != generation {
            return Err(StagingRefusal::WrongOwner);
        }
        Ok(id)
    }

    pub fn begin(&mut self, tab: u64, generation: u64, declared_len: u64) -> Result<AttachmentId, StagingRefusal> {
        if declared_len == 0 {
            return Err(StagingRefusal::Empty);
        }
        if declared_len > RAW_MAX_BYTES {
            return Err(StagingRefusal::TooLargeRaw { bytes: declared_len });
        }
        if declared_len > WINDOW_QUOTA_BYTES - self.reserved {
            return Err(StagingRefusal::WindowQuota);
        }
        let id = AttachmentId {
            token: uuid::Uuid::new_v4(),
            tab,
            generation,
        };
        let file = agent::private_fs::open_private(&self.raw_path(id))?;
        self.entries.insert(
            id.token,
            Entry {
                id,
                reserved: declared_len,
                state: State::Uploading {
                    file,
                    written: 0,
                    digest: Sha256::new(),
                },
            },
        );
        self.reserved += declared_len;
        Ok(id)
    }

    fn entry(&self, id: AttachmentId) -> Result<&Entry, StagingRefusal> {
        let entry = self.entries.get(&id.token).ok_or(StagingRefusal::UnknownId)?;
        if entry.id != id {
            return Err(StagingRefusal::WrongOwner);
        }
        Ok(entry)
    }

    pub fn append(&mut self, id: AttachmentId, offset: u64, chunk: &[u8]) -> Result<(), StagingRefusal> {
        self.entry(id)?;
        let entry = self.entries.get_mut(&id.token).unwrap();
        let State::Uploading { file, written, digest } = &mut entry.state else {
            return Err(StagingRefusal::WrongState);
        };
        if offset != *written {
            return Err(StagingRefusal::WrongOffset);
        }
        if chunk.len() > CHUNK_MAX_BYTES {
            return Err(StagingRefusal::ChunkTooLarge);
        }
        if chunk.len() as u64 > entry.reserved - *written {
            return Err(StagingRefusal::Overrun);
        }
        if let Err(error) = file.write_all(chunk) {
            self.remove(id);
            return Err(error.into());
        }
        digest.update(chunk);
        *written += chunk.len() as u64;
        Ok(())
    }

    pub fn finish(&mut self, id: AttachmentId, sha256_hex: &str) -> Result<RawReady, StagingRefusal> {
        let entry = self.entry(id)?;
        let State::Uploading { written, digest, .. } = &entry.state else {
            return Err(StagingRefusal::WrongState);
        };
        if *written != entry.reserved {
            return Err(StagingRefusal::Incomplete);
        }
        let sha256: [u8; 32] = digest.clone().finalize().into();
        let expected = sha256.iter().map(|b| format!("{b:02x}")).collect::<String>();
        if !expected.eq_ignore_ascii_case(sha256_hex) {
            return Err(StagingRefusal::WrongDigest);
        }
        let ready = RawReady {
            id,
            path: self.raw_path(id),
            bytes: entry.reserved,
            sha256,
        };
        // Dropping the writer before publishing readiness also closes the last writable handle.
        self.entries.get_mut(&id.token).unwrap().state = State::Ready;
        Ok(ready)
    }

    pub fn store_normalized(&mut self, id: AttachmentId, n: &Normalized) -> Result<StagedImage, StagingRefusal> {
        let entry = self.entry(id)?;
        if !matches!(entry.state, State::Ready) {
            return Err(StagingRefusal::WrongState);
        }
        let bytes = n.bytes.len() as u64;
        let reserved = self.reserved - entry.reserved;
        if bytes > WINDOW_QUOTA_BYTES - reserved {
            return Err(StagingRefusal::WindowQuota);
        }
        let path = self.output_path(id);
        let save =
            agent::private_fs::write_private(&path, &n.bytes).and_then(|()| std::fs::remove_file(self.raw_path(id)));
        if let Err(error) = save {
            let _ = std::fs::remove_file(path);
            return Err(error.into());
        }
        let entry = self.entries.get_mut(&id.token).unwrap();
        entry.reserved = bytes;
        entry.state = State::Stored;
        self.reserved = reserved + bytes;
        Ok(StagedImage {
            id,
            image_type: n.image_type,
            width: n.width,
            height: n.height,
            bytes,
            sha256: n.sha256,
            thumbnail_png: n.thumbnail_png.clone(),
        })
    }

    pub fn remove(&mut self, id: AttachmentId) {
        if self.entry(id).is_err() {
            return;
        }
        let entry = self.entries.remove(&id.token).unwrap();
        self.reserved -= entry.reserved;
        drop(entry);
        let _ = std::fs::remove_file(self.raw_path(id));
        let _ = std::fs::remove_file(self.output_path(id));
    }

    /// Reads only finished normalized output. Raw uploads are read only through their worker job.
    pub fn read(&self, id: AttachmentId) -> io::Result<Arc<[u8]>> {
        let entry = self
            .entry(id)
            .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "unknown attachment"))?;
        if !matches!(entry.state, State::Stored) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "attachment is not normalized",
            ));
        }
        let bytes = agent::private_fs::read_private_nofollow_bytes_capped(&self.output_path(id), entry.reserved)?;
        if bytes.len() as u64 != entry.reserved {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "attachment length changed"));
        }
        Ok(bytes.into())
    }

    /// The normalized image of `id` was read into memory ([`Staging::read`]) and is held there until it is sent or
    /// dropped: its file goes now, so the image is not kept twice (the runtime directory is usually memory too), and
    /// its room in the window's quota stays taken until [`Staging::remove`].
    pub fn release_file(&mut self, id: AttachmentId) {
        let Some(entry) = self.entries.get_mut(&id.token).filter(|entry| entry.id == id) else {
            return;
        };
        if matches!(entry.state, State::Stored) {
            entry.state = State::InMemory;
            let _ = std::fs::remove_file(self.output_path(id));
        }
    }

    fn raw_path(&self, id: AttachmentId) -> PathBuf {
        self.dir.join(format!("{id}.raw"))
    }
    fn output_path(&self, id: AttachmentId) -> PathBuf {
        self.dir.join(format!("{id}.image"))
    }
}

fn ensure_private_directory(dir: &Path, root: &Path) -> io::Result<()> {
    match dir.symlink_metadata() {
        Ok(meta) if !meta.is_dir() || meta.uid() != agent::private_fs::euid() => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe attachment directory",
            ));
        }
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    agent::private_fs::create_private_dir_all(dir, root)?;
    if !agent::private_fs::is_private_dir(dir) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe attachment directory",
        ));
    }
    Ok(())
}

impl Drop for Staging {
    fn drop(&mut self) {
        self.entries.clear();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Call once at startup, off the window thread, with the same runtime base as `Staging::open`.
pub fn sweep_stale_staging(base: &Path) {
    sweep_owned(base, agent::private_fs::current_uid());
}

fn sweep_owned(base: &Path, uid: u32) {
    let root = base.join("eitri");
    let parent = root.join("attachments");
    for dir in [&root, &parent] {
        let Ok(meta) = dir.symlink_metadata() else { return };
        if !meta.is_dir() || meta.uid() != agent::private_fs::current_uid() || meta.mode() & 0o022 != 0 {
            return;
        }
    }
    let Ok(entries) = std::fs::read_dir(parent) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some((pid, window)) = name.to_str().and_then(|n| n.split_once('-')) else {
            continue;
        };
        if window.is_empty() || !window.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            continue;
        }
        let Ok(pid) = pid.parse::<u32>() else { continue };
        let Ok(meta) = entry.path().symlink_metadata() else {
            continue;
        };
        if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o022 != 0 {
            continue;
        }
        if agent::process_probe::pid_is_alive(pid) {
            continue;
        }
        let _ = std::fs::remove_dir_all(entry.path());
    }
}

#[cfg(test)]
mod tests {
    use super::super::normalize::normalize;
    use super::*;
    use crate::test_scratch_dir::ScratchDir;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn digest(raw: &[u8]) -> String {
        format!("{:x}", Sha256::digest(raw))
    }

    fn upload(s: &mut Staging, raw: &[u8]) -> (AttachmentId, RawReady) {
        let id = s.begin(7, 3, raw.len() as u64).unwrap();
        for (i, chunk) in raw.chunks(CHUNK_MAX_BYTES).enumerate() {
            s.append(id, (i * CHUNK_MAX_BYTES) as u64, chunk).unwrap();
        }
        let ready = s.finish(id, &digest(raw)).unwrap();
        (id, ready)
    }

    #[test]
    fn staging_refuses_a_gap_an_overrun_and_a_wrong_digest() {
        let scratch = ScratchDir::new("attach", "order");
        let mut s = Staging::open(&scratch).unwrap();
        let id = s.begin(7, 3, 3).unwrap();
        assert!(matches!(s.append(id, 1, b"x"), Err(StagingRefusal::WrongOffset)));
        assert!(matches!(s.append(id, 0, b"abcd"), Err(StagingRefusal::Overrun)));
        assert!(matches!(
            s.append(id, 0, &vec![0; CHUNK_MAX_BYTES + 1]),
            Err(StagingRefusal::ChunkTooLarge)
        ));
        s.append(id, 0, b"a").unwrap();
        assert!(matches!(s.finish(id, &digest(b"a")), Err(StagingRefusal::Incomplete)));
        assert!(matches!(s.append(id, 0, b"a"), Err(StagingRefusal::WrongOffset)));
        s.append(id, 1, b"bc").unwrap();
        assert!(matches!(
            s.finish(id, &digest(b"xyz")),
            Err(StagingRefusal::WrongDigest)
        ));
        assert!(matches!(s.finish(id, "bad"), Err(StagingRefusal::WrongDigest)));
        let ready = s.finish(id, &digest(b"abc").to_uppercase()).unwrap();
        assert_eq!(ready.read().unwrap(), b"abc");
        assert!(matches!(s.append(id, 3, b""), Err(StagingRefusal::WrongState)));
        assert!(matches!(s.finish(id, &digest(b"abc")), Err(StagingRefusal::WrongState)));
        assert!(s.read(id).is_err());
    }

    #[test]
    fn the_window_quota_is_reserved_before_reading() {
        let scratch = ScratchDir::new("attach", "quota");
        let mut s = Staging::open(&scratch).unwrap();
        let id = s.begin(1, 1, RAW_MAX_BYTES).unwrap();
        s.begin(1, 1, RAW_MAX_BYTES).unwrap();
        assert!(matches!(s.begin(1, 1, 1), Err(StagingRefusal::WindowQuota)));
        assert_eq!(std::fs::metadata(s.raw_path(id)).unwrap().len(), 0);
        assert_eq!(std::fs::read_dir(&s.dir).unwrap().count(), 2);
        s.remove(id);
        assert!(s.begin(1, 1, 1).is_ok());
        assert!(matches!(s.begin(1, 1, 0), Err(StagingRefusal::Empty)));
        assert!(matches!(
            s.begin(1, 1, RAW_MAX_BYTES + 1),
            Err(StagingRefusal::TooLargeRaw { .. })
        ));
    }

    #[test]
    fn an_id_from_another_tab_or_generation_is_refused() {
        let scratch = ScratchDir::new("attach", "owner");
        let mut s = Staging::open(&scratch).unwrap();
        let id = s.begin(7, 3, 3).unwrap();
        assert_eq!(id.to_string().len(), 32);
        assert!(id.to_string().bytes().all(|b| b.is_ascii_hexdigit()));
        for (tab, generation) in [(8, 3), (7, 4)] {
            assert!(matches!(
                s.resolve(tab, generation, &id.to_string()),
                Err(StagingRefusal::WrongOwner)
            ));
            let forged = AttachmentId { tab, generation, ..id };
            assert!(matches!(s.append(forged, 0, b"abc"), Err(StagingRefusal::WrongOwner)));
            assert!(matches!(
                s.finish(forged, &digest(b"abc")),
                Err(StagingRefusal::WrongOwner)
            ));
            s.remove(forged);
            assert!(s.read(forged).is_err());
        }
        assert_eq!(s.resolve(7, 3, &id.to_string()).unwrap(), id);
        assert!(matches!(s.resolve(7, 3, "../anything"), Err(StagingRefusal::UnknownId)));
        let other = Staging::open(&scratch).unwrap();
        assert!(matches!(
            other.resolve(7, 3, &id.to_string()),
            Err(StagingRefusal::UnknownId)
        ));
        s.append(id, 0, b"abc").unwrap();
    }

    #[test]
    fn removing_or_dropping_deletes_the_files() {
        let scratch = ScratchDir::new("attach", "remove");
        let mut s = Staging::open(&scratch).unwrap();
        let dir = s.dir.clone();
        assert_eq!(dir.symlink_metadata().unwrap().mode() & 0o777, 0o700);
        let id = s.begin(1, 1, 2).unwrap();
        let path = s.raw_path(id);
        assert_eq!(path.symlink_metadata().unwrap().mode() & 0o777, 0o600);
        s.remove(id);
        assert!(!path.exists());
        assert_eq!(s.reserved, 0);
        s.begin(1, 1, 2).unwrap();
        drop(s);
        assert!(!dir.exists());
    }

    #[test]
    fn normalized_storage_replaces_raw_and_moves_the_reservation() {
        let scratch = ScratchDir::new("attach", "store");
        let mut s = Staging::open(&scratch).unwrap();
        let raw = super::super::normalize::tests::png(4, 3, 255);
        let (id, ready) = upload(&mut s, &raw);
        let n = normalize(&ready.read().unwrap(), RAW_MAX_BYTES).unwrap();
        let mut n = n;
        // A larger output exercises the reservation increase without assuming compressed sizes.
        n.bytes.extend_from_slice(b"extra");
        n.sha256 = Sha256::digest(&n.bytes).into();
        let staged = s.store_normalized(id, &n).unwrap();
        assert_eq!(staged.bytes, n.bytes.len() as u64);
        assert_eq!(staged.sha256, n.sha256);
        assert_eq!(staged.thumbnail_png, n.thumbnail_png);
        assert!(!s.raw_path(id).exists());
        assert_eq!(s.output_path(id).symlink_metadata().unwrap().mode() & 0o777, 0o600);
        assert_eq!(s.reserved, staged.bytes);
        assert_eq!(s.read(id).unwrap().as_ref(), n.bytes);
        assert!(matches!(s.store_normalized(id, &n), Err(StagingRefusal::WrongState)));
        s.remove(id);
        assert!(!s.output_path(id).exists());
        assert_eq!(s.reserved, 0);
    }

    #[test]
    fn a_failed_store_keeps_the_ready_raw_and_its_reservation() {
        let scratch = ScratchDir::new("attach", "failure");
        let mut s = Staging::open(&scratch).unwrap();
        let raw = super::super::normalize::tests::png(1, 1, 255);
        let (id, _) = upload(&mut s, &raw);
        let n = normalize(&raw, RAW_MAX_BYTES).unwrap();
        let target = scratch.join("untouched");
        agent::private_fs::write_private(&target, b"sentinel").unwrap();
        symlink(&target, s.output_path(id)).unwrap();
        assert!(matches!(s.store_normalized(id, &n), Err(StagingRefusal::Io(_))));
        assert_eq!(std::fs::read(&target).unwrap(), b"sentinel");
        assert_eq!(s.reserved, raw.len() as u64);
        assert!(s.raw_path(id).exists());
        s.store_normalized(id, &n).unwrap();
    }

    #[test]
    fn the_sweep_leaves_live_pids_and_other_owners_alone() {
        let scratch = ScratchDir::new("attach", "sweep");
        let live = Staging::open(&scratch).unwrap();
        let parent = live.dir.parent().unwrap();
        let dead = parent.join("2147483647-window");
        agent::private_fs::create_private_dir_all(&dead, &scratch.join("eitri")).unwrap();
        let writable = parent.join("2147483647-writable");
        agent::private_fs::create_private_dir_all(&writable, &scratch.join("eitri")).unwrap();
        std::fs::set_permissions(&writable, std::fs::Permissions::from_mode(0o770)).unwrap();
        let link = parent.join("2147483647-link");
        symlink(&dead, &link).unwrap();
        let unrelated = parent.join("not-a-staging-directory");
        std::fs::create_dir(&unrelated).unwrap();
        sweep_owned(&scratch, agent::private_fs::current_uid().wrapping_add(1));
        assert!(dead.exists());
        sweep_stale_staging(&scratch);
        assert!(live.dir.exists());
        assert!(!dead.exists());
        assert!(writable.exists());
        assert!(link.symlink_metadata().is_ok());
        assert!(unrelated.exists());
    }

    #[test]
    fn output_growth_cannot_overrun_the_window_quota_and_shrinkage_releases_it() {
        let scratch = ScratchDir::new("attach", "output-quota");
        let mut s = Staging::open(&scratch).unwrap();
        let raw = super::super::normalize::tests::png(4, 3, 255);
        let (id, _) = upload(&mut s, &raw);
        s.begin(1, 1, RAW_MAX_BYTES).unwrap();
        s.begin(1, 1, RAW_MAX_BYTES - raw.len() as u64).unwrap();
        assert_eq!(s.reserved, WINDOW_QUOTA_BYTES);
        let mut n = normalize(&raw, RAW_MAX_BYTES).unwrap();
        n.bytes.push(0);
        assert!(matches!(s.store_normalized(id, &n), Err(StagingRefusal::WindowQuota)));
        assert!(s.raw_path(id).exists());
        assert!(!s.output_path(id).exists());
        n.bytes.truncate(raw.len() - 1);
        n.sha256 = Sha256::digest(&n.bytes).into();
        s.store_normalized(id, &n).unwrap();
        assert_eq!(s.reserved, WINDOW_QUOTA_BYTES - 1);
        assert!(s.begin(1, 1, 1).is_ok());
    }

    #[test]
    fn a_replaced_raw_is_checked_again_by_the_worker_reader() {
        let scratch = ScratchDir::new("attach", "raw-integrity");
        let mut s = Staging::open(&scratch).unwrap();
        let (id, ready) = upload(&mut s, b"abc");
        agent::private_fs::write_private(&s.raw_path(id), b"xyz").unwrap();
        assert!(matches!(ready.read(), Err(ImageRefusal::Undecodable)));
        agent::private_fs::write_private(&s.raw_path(id), b"a").unwrap();
        assert!(matches!(ready.read(), Err(ImageRefusal::Undecodable)));
        agent::private_fs::write_private(&s.raw_path(id), b"abcd").unwrap();
        assert!(matches!(ready.read(), Err(ImageRefusal::Undecodable)));
    }

    #[test]
    fn staging_and_the_sweep_do_not_follow_directory_links() {
        let scratch = ScratchDir::new("attach", "linked-root");
        let outside = scratch.join("outside");
        std::fs::create_dir(&outside).unwrap();
        let sentinel = outside.join("2147483647-window");
        std::fs::create_dir(&sentinel).unwrap();
        symlink(&outside, scratch.join("eitri")).unwrap();
        assert!(Staging::open(&scratch).is_err());
        sweep_stale_staging(&scratch);
        assert!(sentinel.exists());
        assert!(!outside.join("attachments").exists());
    }

    #[test]
    fn staging_refusals_have_sentences() {
        for error in [
            StagingRefusal::Empty,
            StagingRefusal::TooLargeRaw {
                bytes: RAW_MAX_BYTES + 1,
            },
            StagingRefusal::WindowQuota,
            StagingRefusal::UnknownId,
            StagingRefusal::WrongOwner,
            StagingRefusal::WrongOffset,
            StagingRefusal::ChunkTooLarge,
            StagingRefusal::Overrun,
            StagingRefusal::Incomplete,
            StagingRefusal::WrongDigest,
            StagingRefusal::WrongState,
            StagingRefusal::Io(io::Error::other("no space")),
        ] {
            assert!(!error.sentence().is_empty());
        }
    }
}
