//! The account's transcripts and the CLI's settings files, read on the server for a remote tab. Everything here is
//! read-only: nothing under the account's config directory is written, renamed, locked or created, and the files are
//! the CLI's own. Parsing stays on the laptop; this module finds the files, bounds what it sends and says why it
//! refused.
//!
//! The config directory is the one the CLI of the tab uses. The laptop names it when it has the handshake's
//! `effective_claude_config_dir`; a call made before any tab names an account instead, resolved here on the login
//! environment's `$HOME` as the sidecar's own start resolves it, else the login environment's `CLAUDE_CONFIG_DIR`, else
//! `$HOME/.claude`. A call that names both must name the same directory. The project's root is resolved as a sidecar
//! start and the trust scan resolve it (`sidecars::open_root`, `sidecars::canonical_root`): the path the kernel gives
//! the folder, through every link, is the one the CLI records its transcripts under and the one whose parents hold the
//! settings that apply. A folder that is gone is named through the parents that are there. Every answer says which
//! config directory and which root it used.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use agent::read_deny::{files_to_read, read_settings, FileRead};
use agent::transcript::{ai_title_in, is_transcript_stable, list_session_files, transcript_path_in};
use eitri_server_proto::{
    ListTranscriptsResponse, SettingsRead, SettingsScope, TranscriptEntry, TranscriptScope, SETTINGS_FILES_MAX,
    SETTINGS_TOTAL_MAX, TRANSCRIPTS_LIST_MAX, TRANSCRIPT_CHUNK, TRANSCRIPT_PATH_MAX, TRANSCRIPT_REFUSAL_MAX,
    TRANSCRIPT_TAIL_MAX, TRANSCRIPT_TITLE_MAX,
};

use super::login_env::Environment;
use super::sidecars::{
    canonical_root, check_path_form, leads_to, open_owned_directory, open_prefix, open_root, refusal_text,
};

/// How long the stability probe waits between its two reads of a transcript's modification time.
pub const STABLE_PROBE: Duration = Duration::from_millis(250);

/// The longest path the helper takes from its caller or its login environment, as a sidecar start takes a root.
const INPUT_PATH_MAX: usize = 4096;
const ACCOUNT_MAX: usize = 128;

/// `text` cut to at most `max` bytes, on a character boundary.
pub fn cut_to(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut cut = max;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
    }
    text
}

/// A refusal's words, within the bound the laptop accepts.
pub fn bounded(text: impl Into<String>) -> String {
    cut_to(text.into(), TRANSCRIPT_REFUSAL_MAX)
}

/// A path text the helper sends: UTF-8 and within [`TRANSCRIPT_PATH_MAX`].
pub fn checked_path(path: &Path) -> Result<String, String> {
    let text = path.to_str().ok_or_else(|| "a path is not UTF-8".to_owned())?;
    if text.len() > TRANSCRIPT_PATH_MAX {
        return Err(format!("a path exceeds {TRANSCRIPT_PATH_MAX} bytes"));
    }
    Ok(text.to_owned())
}

fn check_text(text: &str, what: &str) -> Result<(), String> {
    if text.chars().any(char::is_control) {
        return Err(format!("the {what} has a control character"));
    }
    Ok(())
}

/// The project's root as a call resolved it, and the directory it is anchored to.
#[derive(Debug)]
pub struct Root {
    /// The path the kernel gives the folder, through every link. A folder that is gone is that path of the deepest
    /// parent that is there, then the names that are not.
    pub text: String,
    /// The folder does not exist (a project whose folder was deleted still has its transcripts).
    pub gone: bool,
    /// The folder, or when it is gone the deepest parent that is there, held open from the check on.
    held: File,
    /// The path `held` was named by.
    held_path: String,
}

impl Root {
    /// Whether the folder is still the one that was checked: a directory renamed into its place after the check would
    /// have the settings of another tree read for this project.
    pub fn is_held(&self) -> bool {
        leads_to(&self.held, &self.held_path)
    }
}

/// The project's root as the tab names it, resolved the way a sidecar start records it: absolute, normal form, at most
/// 4096 bytes, an existing directory the helper's account owns, named through every link. A folder that is not there
/// (also a link that leads nowhere) is named through the parents that are: the deepest one that exists, as a start
/// would record it, then the missing names as spelled.
pub fn check_root(root: &str) -> Result<Root, String> {
    check_text(root, "root")?;
    match open_root(root) {
        Ok(held) => {
            let text = canonical_root(&held).map_err(|status| refusal_text(&status))?;
            Ok(Root {
                held_path: text.clone(),
                text,
                gone: false,
                held,
            })
        }
        Err(status) => gone_root(root).ok_or_else(|| refusal_text(&status)),
    }
}

/// `root` as a folder that is not there, or `None` when it is something else: the walk must stop at a name that is
/// absent (`try_exists` is `false` only then), not at a file, a folder that cannot be entered or one another account
/// owns.
fn gone_root(root: &str) -> Option<Root> {
    check_path_form(root, "root").ok()?;
    let prefix = open_prefix(root, "root").ok()?;
    if prefix.rest >= root.len() {
        return None;
    }
    let parent = canonical_root(&prefix.dir).ok()?;
    let missing = &root[prefix.rest..];
    let first = missing.split('/').next().unwrap_or(missing);
    if !matches!(Path::new(&parent).join(first).try_exists(), Ok(false)) {
        return None;
    }
    let text = if parent == "/" {
        format!("/{missing}")
    } else {
        format!("{parent}/{missing}")
    };
    Some(Root {
        text,
        gone: true,
        held: prefix.dir,
        held_path: parent,
    })
}

/// A session id as the CLI names its transcripts: a lowercase, hyphenated UUID.
pub fn is_session_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(byte),
        })
}

fn text_of<'a>(env: &'a Environment, key: &str) -> Option<&'a str> {
    env.get(OsStr::new(key)).and_then(|value| value.to_str())
}

/// A directory named by text: absolute, a doubled, trailing or `.` component dropped (the CLI reads the same
/// directory), and no `..`, which would need the folders on the way to be resolved.
fn lexical_directory(text: &str, what: &str) -> Result<PathBuf, String> {
    if text.len() > INPUT_PATH_MAX {
        return Err(format!("the {what} exceeds {INPUT_PATH_MAX} bytes"));
    }
    check_text(text, what)?;
    let path = Path::new(text);
    if !path.is_absolute() {
        return Err(format!("the {what} is not an absolute path"));
    }
    let mut rebuilt = PathBuf::new();
    for component in path.components() {
        if component == Component::ParentDir {
            return Err(format!("the {what} has a `..` component"));
        }
        // An absolute path on a unix host has a root, then names; `.` only leads a relative path.
        rebuilt.push(component);
    }
    Ok(rebuilt)
}

/// The login environment's `$HOME`, absolute.
pub fn home_of(env: &Environment) -> Result<PathBuf, String> {
    match text_of(env, "HOME").filter(|home| !home.is_empty()) {
        Some(home) => lexical_directory(home, "login environment's HOME"),
        None => Err("the login environment has no HOME".to_owned()),
    }
}

/// The directory the account `name` names on the login environment's `$HOME`, the way a sidecar start resolves it.
fn account_directory(name: &str, env: &Environment) -> Result<PathBuf, String> {
    if name.len() > ACCOUNT_MAX {
        return Err("the account name exceeds 128 bytes".to_owned());
    }
    let home = home_of(env).ok();
    let account = agent::ClaudeAccount::resolve_from(name, |key| match key {
        "HOME" => home.as_ref().and_then(|home| home.to_str()).map(str::to_owned),
        _ => text_of(env, key).map(str::to_owned),
    })
    .map_err(|error| error.to_string())?;
    if account.name() != name {
        return Err("the account name is not in normal form".to_owned());
    }
    account.check_config_dir().map_err(|error| error.to_string())?;
    let dir = account
        .config_dir()
        .to_str()
        .ok_or_else(|| "the account's directory is not UTF-8".to_owned())?;
    lexical_directory(dir, "account's directory")
}

/// Whether two config directory spellings lead to the same directory.
fn same_config_directory(a: &Path, b: &Path) -> bool {
    a == b
        || match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(a), Ok(b)) => (a.dev(), a.ino()) == (b.dev(), b.ino()),
            _ => false,
        }
}

/// The account's config directory for a call's scope, in the order the module doc gives.
pub fn config_dir(scope: &TranscriptScope, env: &Environment) -> Result<PathBuf, String> {
    let given = if scope.config_dir.is_empty() {
        None
    } else {
        // A doubled, trailing or `.` component names the directory the CLI reads, and the answer says it in normal form.
        let dir = lexical_directory(&scope.config_dir, "config directory")?;
        let text = dir
            .to_str()
            .ok_or_else(|| "the config directory is not UTF-8".to_owned())?;
        open_owned_directory(text, "config directory").map_err(|status| refusal_text(&status))?;
        Some(dir)
    };
    let account = if scope.account.is_empty() {
        None
    } else {
        Some(account_directory(&scope.account, env)?)
    };
    match (given, account) {
        (Some(given), Some(account)) if !same_config_directory(&given, &account) => Err(format!(
            "the config directory {} is not the directory {} of the account {}",
            given.display(),
            account.display(),
            scope.account
        )),
        (Some(given), _) => Ok(given),
        (None, Some(account)) => Ok(account),
        (None, None) => match text_of(env, "CLAUDE_CONFIG_DIR").filter(|dir| !dir.is_empty()) {
            Some(dir) => lexical_directory(dir, "login environment's CLAUDE_CONFIG_DIR"),
            None => Ok(home_of(env)?.join(".claude")),
        },
    }
}

/// What a call resolved: the config directory and the root it used, each as the answer names it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Used {
    pub config_dir: String,
    pub root: String,
    pub root_gone: bool,
}

/// The root and the config directory of a call's scope, with the root as it was held.
fn resolve_held(scope: &TranscriptScope, env: &Environment) -> Result<(Used, Root), String> {
    let root = check_root(&scope.root)?;
    let dir = config_dir(scope, env)?;
    checked_path(Path::new(&root.text))?;
    let used = Used {
        config_dir: checked_path(&dir)?,
        root: root.text.clone(),
        root_gone: root.gone,
    };
    Ok((used, root))
}

/// The root and the config directory of a call's scope.
pub fn resolve(scope: &TranscriptScope, env: &Environment) -> Result<Used, String> {
    resolve_held(scope, env).map(|(used, _)| used)
}

/// The title the CLI recorded in a transcript, within [`TRANSCRIPT_TITLE_MAX`], empty when there is none. The file is
/// opened as a tail is (never through a link, never blocking), so a transcript swapped for a pipe after it was listed
/// cannot hold the thread.
pub fn title_of(path: &Path) -> String {
    let title = open_regular(path)
        .ok()
        .and_then(|(file, _)| ai_title_in(file))
        .unwrap_or_default();
    cut_to(title, TRANSCRIPT_TITLE_MAX)
}

/// The server's session transcripts for the tab's account and root, newest first, at most
/// [`TRANSCRIPTS_LIST_MAX`], each with the title the CLI recorded.
pub fn list(scope: &TranscriptScope, env: &Environment) -> ListTranscriptsResponse {
    let used = match resolve(scope, env) {
        Ok(used) => used,
        Err(refusal) => {
            return ListTranscriptsResponse {
                refusal: bounded(refusal),
                ..Default::default()
            }
        }
    };
    let answer = ListTranscriptsResponse {
        config_dir: used.config_dir.clone(),
        root: used.root.clone(),
        root_gone: used.root_gone,
        ..Default::default()
    };
    let projects = Path::new(&used.config_dir).join("projects");
    // `list_session_files` keeps only names the laptop takes for a session before it cuts, so one more than the bound
    // being there says exactly that there were more.
    let mut found = match list_session_files(&projects, &used.root, TRANSCRIPTS_LIST_MAX + 1) {
        Ok(found) => found,
        Err(error) => {
            return ListTranscriptsResponse {
                refusal: bounded(format!("the account's transcripts could not be listed: {error}")),
                ..answer
            }
        }
    };
    let truncated = found.len() > TRANSCRIPTS_LIST_MAX;
    found.truncate(TRANSCRIPTS_LIST_MAX);
    let entries = found
        .into_iter()
        .map(|file| TranscriptEntry {
            ai_title: title_of(&transcript_path_in(&projects, &used.root, &file.provider_session_id)),
            provider_session_id: file.provider_session_id,
            len: file.len,
            modified_unix_ms: file.modified_ms,
        })
        .collect();
    ListTranscriptsResponse {
        entries,
        truncated,
        ..answer
    }
}

/// Where a session's transcript is, for a request's scope: what the call used and the file's path.
pub fn session_file(
    scope: &TranscriptScope,
    provider_session_id: &str,
    env: &Environment,
) -> Result<(Used, PathBuf), String> {
    if !is_session_id(provider_session_id) {
        return Err("the session id is not a lowercase hyphenated UUID".to_owned());
    }
    let used = resolve(scope, env)?;
    let path = transcript_path_in(
        &Path::new(&used.config_dir).join("projects"),
        &used.root,
        provider_session_id,
    );
    Ok((used, path))
}

/// A transcript opened for its tail: never through a link, never anything but a regular file.
pub struct Tail {
    file: File,
    /// Where the bytes sent start; past 0 the tail begins inside the file.
    pub start: u64,
    remaining: u64,
}

impl Tail {
    pub fn started_mid_file(&self) -> bool {
        self.start > 0
    }

    /// The tail's bytes in chunks of at most [`TRANSCRIPT_CHUNK`], each handed to `send` with whether it is the last;
    /// `send` answers `false` when nobody takes any more. A file that shrank under the read ends it early, as the
    /// last chunk.
    pub fn chunks(mut self, send: &mut dyn FnMut(Vec<u8>, bool) -> bool) -> std::io::Result<()> {
        self.file.seek(SeekFrom::Start(self.start))?;
        loop {
            let want = self.remaining.min(TRANSCRIPT_CHUNK as u64) as usize;
            let mut chunk = Vec::with_capacity(want);
            (&self.file).take(want as u64).read_to_end(&mut chunk)?;
            self.remaining -= chunk.len() as u64;
            let last = self.remaining == 0 || chunk.len() < want;
            if !send(chunk, last) || last {
                return Ok(());
            }
        }
    }
}

/// Opens a transcript. The final component is opened with `O_NOFOLLOW`, so a link planted in a transcript's place is
/// refused, and `O_NONBLOCK`, so a pipe there cannot hold the open; what is open must be a regular file.
fn open_regular(path: &Path) -> Result<(File, std::fs::Metadata), String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| match error.raw_os_error() {
            Some(libc::ELOOP) => "the transcript is a link, not a file".to_owned(),
            _ if error.kind() == std::io::ErrorKind::NotFound => "there is no such transcript".to_owned(),
            _ => format!("the transcript could not be opened: {error}"),
        })?;
    let meta = file
        .metadata()
        .map_err(|error| format!("the transcript could not be read: {error}"))?;
    if !meta.is_file() {
        return Err("the transcript is not a regular file".to_owned());
    }
    Ok((file, meta))
}

/// Opens `path` for its last [`TRANSCRIPT_TAIL_MAX`] bytes, as [`open_regular`] opens it.
pub fn open_tail(path: &Path) -> Result<Tail, String> {
    let (file, meta) = open_regular(path)?;
    let start = meta.len().saturating_sub(TRANSCRIPT_TAIL_MAX);
    Ok(Tail {
        file,
        start,
        remaining: meta.len() - start,
    })
}

/// What the stability probe found.
#[derive(Debug, PartialEq, Eq)]
pub enum Stability {
    Stable,
    Moving,
    Missing,
}

/// [`is_transcript_stable`] over a transcript that must be a regular file (a link or anything else is refused), with
/// `delay` between its two reads. Blocking.
pub fn stable(path: &Path, delay: Duration) -> Result<Stability, String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_file() => {}
        Ok(_) => return Err("the transcript is not a regular file".to_owned()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Stability::Missing),
        Err(error) => return Err(format!("the transcript could not be examined: {error}")),
    }
    match is_transcript_stable(path, delay) {
        Ok(true) => Ok(Stability::Stable),
        Ok(false) => Ok(Stability::Moving),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Stability::Missing),
        Err(error) => Err(format!("the transcript could not be examined: {error}")),
    }
}

/// The settings files that bear on a project at `root` on this machine, each read as the local reader reads it, in
/// `files_to_read`'s order, after a scope that names the server's `$HOME`, the config directory and how many files
/// follow. A project with more files than [`SETTINGS_FILES_MAX`] is refused outright: a shorter list would read as a
/// complete one on the laptop and lose deny rules.
pub fn settings(scope: &TranscriptScope, env: &Environment) -> Result<(SettingsScope, Vec<SettingsRead>), String> {
    let (used, root) = resolve_held(scope, env)?;
    let home = checked_path(&home_of(env)?)?;
    let files = files_to_read(Path::new(&used.root), Some(Path::new(&used.config_dir)));
    if files.len() > SETTINGS_FILES_MAX {
        return Err(format!(
            "{} settings files bear on this project, more than the {SETTINGS_FILES_MAX} that can be sent",
            files.len()
        ));
    }
    let mut total = 0u64;
    let mut reads = Vec::with_capacity(files.len());
    for file in files {
        // A path past the bound is a refusal of the whole list: the file cannot be named, and a list without it
        // would read as a complete one.
        if file.path.as_os_str().len() > TRANSCRIPT_PATH_MAX {
            return Err(format!("a settings file's path exceeds {TRANSCRIPT_PATH_MAX} bytes"));
        }
        let mut read = SettingsRead {
            path: file.path.to_string_lossy().into_owned(),
            strict: file.strict,
            ..Default::default()
        };
        match read_settings(&file) {
            FileRead::Bytes(bytes) => {
                total += bytes.len() as u64;
                if total > SETTINGS_TOTAL_MAX {
                    read.failed = format!(
                        "the settings files together are larger than {} MiB",
                        SETTINGS_TOTAL_MAX >> 20
                    );
                } else if file.path.to_str().is_none() {
                    read.failed = "its name is not UTF-8".to_owned();
                } else {
                    read.bytes = bytes;
                }
            }
            FileRead::Missing => read.missing = true,
            FileRead::Failed(why) => {
                // A read that failed always says so: an empty reason would read as an empty file.
                read.failed = bounded(if why.is_empty() {
                    "it could not be read".to_owned()
                } else {
                    why
                });
            }
        }
        reads.push(read);
    }
    // The files were named from the root's path and read from it afresh: a folder renamed into its place meanwhile
    // would have its settings sent as this project's.
    if !root.is_held() {
        return Err("the project folder changed while its settings were read".to_owned());
    }
    let scope = SettingsScope {
        home,
        config_dir: used.config_dir,
        files: reads.len() as u32,
        root: used.root,
        root_gone: used.root_gone,
    };
    Ok((scope, reads))
}
