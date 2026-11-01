//! The shell -> nvim half of the read-only row view (`Ctrl+g` in BROWSE) and the draft editor
//! (`Ctrl+g` in INPUT), over the existing RPC in `LiveHarness` rather than a new socket. One
//! `nvim_input` of a `<Cmd>lua` call whose only argument is hex, so nothing typed is ever read as
//! Lua; bodies travel as files in a per-window 0700 directory; an
//! edit's result comes back as a marker file the agent panel's 33 ms tick polls.

use std::io;
use std::path::{Path, PathBuf};

use crate::agent_bridge::{ProposedChange, ProposedEdit};

const DIR_PREFIX: &str = "nv-sc-";
/// No socket lives here; the sweep's socket probe finds nothing and the pid alone decides. The
/// namespace caveat `instance_dir::sweep_stale_instance_dirs` documents applies: a live window in
/// another pid namespace sharing `TMPDIR` could lose its scratch directory, and costs a failed
/// `Ctrl+g` with a notice, not a crash.
const NO_SOCKET: &str = "none";
const LUA_NAME: &str = "nvim_scratch.lua";
pub(crate) const NVIM_SCRATCH_LUA: &str = include_str!("nvim_scratch.lua");

pub const LOADER_CMD: &str = "lua local p = vim.env.EITRI_SCRATCH_LUA; if p and p ~= '' then pcall(dofile, p) end";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScratchRequest {
    hex: String,
}

impl ScratchRequest {
    fn of(value: serde_json::Value) -> Self {
        ScratchRequest {
            hex: hex(value.to_string().as_bytes()),
        }
    }
    pub fn hex(&self) -> &str {
        &self.hex
    }
    /// What `NeovideEditorPane::send_keys` is given.
    pub fn input_keys(&self) -> String {
        format!("<Cmd>lua EitriScratch.call('{}')<CR>", self.hex)
    }
}

/// `gf`: `:edit <path>` and the line.
pub fn open_request(path: &Path, line: Option<u32>) -> ScratchRequest {
    ScratchRequest::of(serde_json::json!({ "op": "open", "path": path.to_string_lossy(), "line": line }))
}

/// The largest file a proposal is shown against. A source file an agent edits is far smaller; a larger one
/// would be held twice in memory and written twice to the scratch directory for a view nobody reads whole.
pub const PROPOSAL_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Why a pending edit's proposal cannot be shown against its file. Each says so in one line, naming the
/// file, for the panel's band.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProposalRefused {
    /// An old text is no longer in the file, or is there more than once for a single replacement: the
    /// file changed since the card was raised, and a diff against it would show a change nobody proposed.
    Stale {
        path: String,
    },
    /// Replacements proposed for a file that is not there.
    Missing {
        path: String,
    },
    /// A directory, a FIFO, a device or a socket: nothing a card edits, and a FIFO could hold a read forever.
    NotAFile {
        path: String,
    },
    TooLarge {
        path: String,
        bytes: u64,
    },
    NotText {
        path: String,
    },
    Io(String),
}

impl std::fmt::Display for ProposalRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProposalRefused::Stale { path } => {
                write!(f, "the proposal no longer applies to {path} as it is on disk")
            }
            ProposalRefused::Missing { path } => write!(f, "no such file: {path}"),
            ProposalRefused::NotAFile { path } => write!(f, "not a regular file: {path}"),
            ProposalRefused::TooLarge { path, bytes } => write!(
                f,
                "{path} is too large to show beside the proposal ({bytes} bytes; the most is {PROPOSAL_MAX_BYTES})"
            ),
            ProposalRefused::NotText { path } => write!(f, "{path} is not UTF-8 text"),
            ProposalRefused::Io(why) => f.write_str(why),
        }
    }
}

/// The file as it is on disk and as the proposal would leave it: `(base, proposed)`. Never writes `file`.
///
/// Called on a worker thread, never the GTK one: the path is the agent's, and it can name anything. The
/// file is opened non-blocking, so a FIFO with no writer cannot hold the open, and the open handle is
/// checked to be a regular file before a byte is read, so what is checked is what is read. A file that
/// is not there is an empty base for whole content (a new file) and refused for replacements.
pub fn proposal_texts(file: &Path, change: &ProposedChange) -> Result<(String, String), ProposalRefused> {
    let path = file.display().to_string();
    let base = match read_regular_file(file, &path)? {
        Some(text) => text,
        None => match change {
            ProposedChange::Content { .. } => String::new(),
            ProposedChange::Edits { .. } => return Err(ProposalRefused::Missing { path }),
        },
    };
    let proposed = match change {
        ProposedChange::Content { content } => {
            refuse_over_limit(content.len() as u64, &path)?;
            content.clone()
        }
        // A tool that always reads CR-LF as LF writes what that reading gives even when the old texts also
        // match the raw bytes: a new text's bare LFs come out as CR-LF, so the raw result is not its result.
        ProposedChange::Edits {
            edits,
            crlf_as_lf: true,
        } if base.contains("\r\n") => apply_edits_to_lf(&base, edits, &path)?,
        ProposedChange::Edits { edits, .. } => match apply_edits(&base, edits, &path) {
            Err(ProposalRefused::Stale { .. }) if base.contains("\r\n") => apply_edits_to_lf(&base, edits, &path)?,
            other => other?,
        },
    };
    Ok((base, proposed))
}

/// `TooLarge` when a text of `bytes` would pass [`PROPOSAL_MAX_BYTES`]. The proposed text is held to the
/// same limit as the file: it is built in memory on the worker and written to the scratch directory on the
/// GTK thread, and replacing every match of a short old text with a long new one multiplies its size.
fn refuse_over_limit(bytes: u64, path: &str) -> Result<(), ProposalRefused> {
    if bytes > PROPOSAL_MAX_BYTES {
        return Err(ProposalRefused::TooLarge {
            path: path.to_string(),
            bytes,
        });
    }
    Ok(())
}

/// `edits` applied in order to `base`, each to the text the earlier ones left. The size each replacement
/// would leave is checked before it is built, so an oversized result is refused without being allocated.
fn apply_edits(base: &str, edits: &[ProposedEdit], path: &str) -> Result<String, ProposalRefused> {
    let mut text = base.to_string();
    for edit in edits {
        // An empty old text is found everywhere and so names no place; it says nothing about the file.
        let found = if edit.old_text.is_empty() {
            0
        } else {
            text.matches(edit.old_text.as_str()).count()
        };
        if found == 0 || (found > 1 && !edit.replace_all) {
            return Err(ProposalRefused::Stale { path: path.to_string() });
        }
        let replaced = found as u64;
        let after = (text.len() as u64)
            .saturating_sub(replaced.saturating_mul(edit.old_text.len() as u64))
            .saturating_add(replaced.saturating_mul(edit.new_text.len() as u64));
        refuse_over_limit(after, path)?;
        text = if edit.replace_all {
            text.replace(&edit.old_text, &edit.new_text)
        } else {
            text.replacen(&edit.old_text, &edit.new_text, 1)
        };
    }
    Ok(text)
}

/// The edits applied to `base` with its CR-LF line ends read as LF, for an agent whose edit tool matches
/// old texts that way (Claude Code's does: it reads a file with CR-LF made LF, matches and replaces there,
/// and writes LF back as CR-LF when CR-LF ends most of the file's lines). Used first for a change that says
/// its tool reads files that way; for any other, only after the edits failed to match the file's bytes as
/// they are, so an agent that matches raw bytes is shown exactly that.
fn apply_edits_to_lf(base: &str, edits: &[ProposedEdit], path: &str) -> Result<String, ProposalRefused> {
    let lf = |text: &str| text.replace("\r\n", "\n");
    let edits: Vec<ProposedEdit> = edits
        .iter()
        .map(|edit| ProposedEdit {
            old_text: lf(&edit.old_text),
            new_text: lf(&edit.new_text),
            replace_all: edit.replace_all,
        })
        .collect();
    let text = apply_edits(&lf(base), &edits, path)?;
    let crlf = base.matches("\r\n").count();
    let bare_lf = base.matches('\n').count() - crlf;
    if crlf <= bare_lf {
        return Ok(text);
    }
    refuse_over_limit((text.len() + text.matches('\n').count()) as u64, path)?;
    Ok(text.replace('\n', "\r\n"))
}

/// `Ok(None)` when nothing is at `file`; its UTF-8 text when it is a regular file of at most
/// [`PROPOSAL_MAX_BYTES`]; refused otherwise.
fn read_regular_file(file: &Path, path: &str) -> Result<Option<String>, ProposalRefused> {
    read_file_with(file, path, 0, |_| true)
}

/// As [`read_regular_file`], for a caller that judged `file` and has to know that what it reads is what it judged. A link
/// at its last component is refused, and `check` looks at the open handle before a byte is read; `false` refuses it.
pub fn read_file_checked(
    file: &Path,
    check: impl FnOnce(&std::fs::File) -> bool,
) -> Result<Option<String>, ProposalRefused> {
    read_file_with(file, &file.display().to_string(), libc::O_NOFOLLOW, check)
}

fn read_file_with(
    file: &Path,
    path: &str,
    flags: libc::c_int,
    check: impl FnOnce(&std::fs::File) -> bool,
) -> Result<Option<String>, ProposalRefused> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    // A path that names no regular file is refused before it is opened: opening a device can act on it
    // (a serial port raises its lines). The open handle is checked again below, since the path can change
    // between this look and the open.
    match std::fs::metadata(file) {
        Ok(meta) if !meta.file_type().is_file() => return Err(ProposalRefused::NotAFile { path: path.to_string() }),
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(ProposalRefused::Io(format!("could not read {path}: {e}"))),
    }
    let opened = std::fs::OpenOptions::new()
        .read(true)
        // No controlling terminal is taken from a tty the path might name.
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | flags)
        .open(file);
    let handle = match opened {
        Ok(handle) => handle,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(ProposalRefused::Io(format!("could not read {path}: {e}"))),
    };
    let meta = handle
        .metadata()
        .map_err(|e| ProposalRefused::Io(format!("could not read {path}: {e}")))?;
    if !meta.file_type().is_file() || !check(&handle) {
        return Err(ProposalRefused::NotAFile { path: path.to_string() });
    }
    if meta.len() > PROPOSAL_MAX_BYTES {
        return Err(ProposalRefused::TooLarge {
            path: path.to_string(),
            bytes: meta.len(),
        });
    }
    // The size can grow between the check and the read; one byte past the limit tells that apart.
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    handle
        .take(PROPOSAL_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| ProposalRefused::Io(format!("could not read {path}: {e}")))?;
    if bytes.len() as u64 > PROPOSAL_MAX_BYTES {
        return Err(ProposalRefused::TooLarge {
            path: path.to_string(),
            bytes: bytes.len() as u64,
        });
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| ProposalRefused::NotText { path: path.to_string() })
}

/// The part of a file name that may stand in a scratch file's name: its last component, with anything
/// but ASCII letters, digits, `.`, `_` and `-` made `-`, and no leading dot, so the name can neither
/// leave the scratch directory nor hide in it.
fn scratch_slug(name: &str) -> String {
    let last = name.rsplit('/').next().unwrap_or(name);
    let slug: String = last
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let slug: String = slug.trim_start_matches('.').chars().rev().take(48).collect();
    let slug: String = slug.chars().rev().collect();
    if slug.is_empty() {
        "file".to_string()
    } else {
        slug
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditDone {
    Written(String),
    Discarded,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingEdit {
    pub id: u64,
    pub body: PathBuf,
    pub done: PathBuf,
}

impl PendingEdit {
    /// `None` until nvim wrote the marker. An empty marker is still being written, not a failure:
    /// the snippet renames a finished file into place, but reading one mid-write as `Failed("")`
    /// would delete the text the user just `:wq`'d, so this side does not rely on that alone.
    pub fn poll(&self) -> Option<EditDone> {
        let marker = std::fs::read_to_string(&self.done).ok()?;
        let marker = marker.trim_end();
        if marker.is_empty() {
            return None;
        }
        Some(if marker == "written" {
            match std::fs::read_to_string(&self.body) {
                Ok(text) => EditDone::Written(text.strip_suffix('\n').unwrap_or(&text).to_string()),
                Err(e) => EditDone::Failed(format!("could not read the edited draft back: {e}")),
            }
        } else if marker == "discarded" {
            EditDone::Discarded
        } else {
            EditDone::Failed(marker.strip_prefix("error: ").unwrap_or(marker).to_string())
        })
    }
    pub fn cleanup(&self) {
        let _ = std::fs::remove_file(&self.body);
        let _ = std::fs::remove_file(&self.done);
    }
}

/// Whether nvim's buffer name `file` lies inside the scratch directory `dir`: a `Ctrl+g` draft or a
/// read-only row view, Eitri's own buffer and never a file the user is working on. The editor-context
/// feed drops such a report, so the composer's editor-context line and the next turn keep naming the
/// file the user is working in. Compared by path component, so `<dir>x/…` is not inside.
pub fn holds(dir: &Path, file: &str) -> bool {
    !file.is_empty() && Path::new(file).starts_with(dir)
}

pub struct ScratchDir {
    dir: PathBuf,
    lua_path: PathBuf,
    next: u64,
}

impl ScratchDir {
    /// `None`, logged, when the directory cannot be made: `Ctrl+g` then answers with a notice.
    pub fn new() -> Option<Self> {
        // The snippet accepts only absolute paths, and a relative `TMPDIR` would hand it relative ones.
        let tmp = match std::path::absolute(std::env::temp_dir()) {
            Ok(tmp) => tmp,
            Err(e) => {
                // An empty `TMPDIR` has no absolute form; say so rather than leaving `Ctrl+g` unexplained.
                eprintln!(
                    "[scratch] could not make the temporary directory {} absolute: {e} -- Ctrl+g is unavailable",
                    std::env::temp_dir().display()
                );
                return None;
            }
        };
        crate::instance_dir::sweep_stale_instance_dirs(&tmp, DIR_PREFIX, NO_SOCKET, "scratch");
        Self::in_dir(&tmp)
    }

    pub fn in_dir(tmp: &Path) -> Option<Self> {
        let dir = crate::instance_dir::instance_dir_path(tmp, DIR_PREFIX);
        // Exactly 0700 whatever the umask, like the other per-instance directories: one that takes the
        // owner's own bits away would leave a directory the snippet cannot be written into.
        if let Err(e) = crate::instance_dir::create_private_dir(&dir) {
            eprintln!(
                "[scratch] could not create {}: {e} -- Ctrl+g is unavailable",
                dir.display()
            );
            return None;
        }
        let lua_path = dir.join(LUA_NAME);
        if let Err(e) = std::fs::write(&lua_path, NVIM_SCRATCH_LUA) {
            eprintln!("[scratch] could not write the snippet: {e} -- Ctrl+g is unavailable");
            let _ = std::fs::remove_dir_all(&dir);
            return None;
        }
        Some(ScratchDir { dir, lua_path, next: 1 })
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Whether nvim's buffer name `file` is one of this directory's scratch buffers (`holds`).
    pub fn holds_file(&self, file: &str) -> bool {
        holds(&self.dir, file)
    }

    pub fn child_env(&self) -> Vec<(String, String)> {
        vec![("EITRI_SCRATCH_LUA".to_string(), self.lua_path.display().to_string())]
    }

    pub fn nvim_args(&self) -> Vec<String> {
        vec!["--cmd".to_string(), LOADER_CMD.to_string()]
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next;
        self.next += 1;
        id
    }

    /// A read-only buffer holding `text`. The title names the file, so `:ls` says what it is.
    pub fn prepare_view(&mut self, title: &str, text: &str) -> io::Result<ScratchRequest> {
        let id = self.next_id();
        let slug: String = title
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .take(32)
            .collect();
        let body = self.dir.join(format!("{id}-{}.md", slug.trim_matches('-')));
        std::fs::write(&body, text)?;
        Ok(ScratchRequest::of(
            serde_json::json!({ "op": "view", "path": body.to_string_lossy() }),
        ))
    }

    /// A pending edit's proposal: the file's text as it is (`base`) and as the edit would leave it
    /// (`proposed`), each in a scratch file of its own, for nvim's read-only diff tab. `name` is the real
    /// file's name; nvim takes the filetype from it, and only a slug of its last component reaches the
    /// scratch files' names. Writes nothing outside the scratch directory.
    pub fn prepare_proposal(&mut self, name: &str, base: &str, proposed: &str) -> io::Result<ScratchRequest> {
        let id = self.next_id();
        let slug = scratch_slug(name);
        let base_file = self.dir.join(format!("{id}-on-disk-{slug}"));
        let proposed_file = self.dir.join(format!("{id}-proposed-{slug}"));
        std::fs::write(&base_file, base)?;
        std::fs::write(&proposed_file, proposed)?;
        Ok(ScratchRequest::of(serde_json::json!({
            "op": "diff",
            "base": base_file.to_string_lossy(),
            "proposed": proposed_file.to_string_lossy(),
            "name": name,
        })))
    }

    /// The draft in an editable buffer whose wipe writes the marker.
    pub fn prepare_edit(&mut self, text: &str) -> io::Result<(ScratchRequest, PendingEdit)> {
        let id = self.next_id();
        let edit = PendingEdit {
            id,
            body: self.dir.join(format!("{id}-draft.md")),
            done: self.dir.join(format!("{id}-draft.done")),
        };
        std::fs::write(&edit.body, text)?;
        let request = ScratchRequest::of(serde_json::json!({
            "op": "edit",
            "path": edit.body.to_string_lossy(),
            "done": edit.done.to_string_lossy(),
        }));
        Ok((request, edit))
    }

    /// Called from the window's close handler (GTK does not reliably run `Drop` before exit).
    pub fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scratch directory itself is the unique per-test directory (`nv-sc-<pid>-<uuid>`), so
    /// `cleanup` leaves nothing behind in the shared `TMPDIR`.
    fn tmp() -> PathBuf {
        std::env::temp_dir()
    }

    fn decode(hex: &str) -> serde_json::Value {
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Only `[0-9a-f]` travels through `nvim_input`, so a path cannot close `<Cmd>` or introduce
    /// its own key notation.
    #[test]
    fn a_request_crosses_nvim_input_as_hex_only() {
        let hostile = Path::new("/tmp/a<CR>:!rm -rf ~<CR>|b'.md");
        let request = open_request(hostile, Some(42));
        let keys = request.input_keys();
        assert!(
            keys.starts_with("<Cmd>lua EitriScratch.call('") && keys.ends_with("')<CR>"),
            "{keys}"
        );
        assert!(request
            .hex()
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_eq!(keys.matches('<').count(), 2, "only <Cmd> and <CR>: {keys}");
        let value = decode(request.hex());
        assert_eq!(value["op"], "open");
        assert_eq!(value["path"], hostile.to_string_lossy().as_ref());
        assert_eq!(value["line"], 42);
    }

    /// A focused `Ctrl+g` draft must not replace the user's file in editor context:
    /// a buffer in this directory belongs to Eitri.
    #[test]
    fn only_files_inside_the_scratch_directory_are_scratch_buffers() {
        let mut dir = ScratchDir::in_dir(&tmp()).unwrap();
        let (_, edit) = dir.prepare_edit("draft").unwrap();
        let view = decode(dir.prepare_view("Bash: ls", "out").unwrap().hex());
        assert!(dir.holds_file(&edit.body.to_string_lossy()));
        assert!(dir.holds_file(view["path"].as_str().unwrap()));
        assert!(!dir.holds_file("/home/user/project/src/main.rs"));
        assert!(!dir.holds_file(""), "a buffer with no file");
        // A sibling whose name merely starts with the directory's: a component match, not a string one.
        let sibling = format!("{}x/1-draft.md", dir.path().display());
        assert!(!dir.holds_file(&sibling));
        dir.cleanup();
    }

    #[test]
    fn a_view_writes_its_body_and_names_no_done_marker() {
        let mut dir = ScratchDir::in_dir(&tmp()).unwrap();
        let request = dir.prepare_view("Bash: cargo test", "line 1\nline 2").unwrap();
        let value = decode(request.hex());
        assert_eq!(value["op"], "view");
        let body = PathBuf::from(value["path"].as_str().unwrap());
        assert!(body.starts_with(dir.path()));
        assert_eq!(std::fs::read_to_string(&body).unwrap(), "line 1\nline 2");
        assert!(value.get("done").is_none() || value["done"].is_null());
        dir.cleanup();
        assert!(!dir.path().exists());
    }

    #[test]
    fn an_edit_is_pending_until_its_marker_says_written_discarded_or_failed() {
        let mut dir = ScratchDir::in_dir(&tmp()).unwrap();
        let (request, edit) = dir.prepare_edit("draft text").unwrap();
        let value = decode(request.hex());
        assert_eq!(value["op"], "edit");
        assert_eq!(value["done"], edit.done.to_string_lossy().as_ref());
        assert_eq!(edit.poll(), None, "no marker yet");
        std::fs::write(&edit.done, "").unwrap();
        assert_eq!(edit.poll(), None, "an empty marker is one being written, not a failure");

        std::fs::write(&edit.body, "edited in nvim\n").unwrap();
        std::fs::write(&edit.done, "written").unwrap();
        assert_eq!(
            edit.poll(),
            Some(EditDone::Written("edited in nvim".into())),
            "nvim's final EOL is not the user's"
        );

        std::fs::write(&edit.done, "discarded").unwrap();
        assert_eq!(edit.poll(), Some(EditDone::Discarded));
        std::fs::write(&edit.done, "error: E212: Can't open file").unwrap();
        assert_eq!(edit.poll(), Some(EditDone::Failed("E212: Can't open file".into())));
        let (_, second) = dir.prepare_edit("again").unwrap();
        assert_ne!(second.id, edit.id, "every request has its own files");
        edit.cleanup();
        assert!(!edit.body.exists() && !edit.done.exists());
        dir.cleanup();
    }

    fn edit(old: &str, new: &str, replace_all: bool) -> ProposedEdit {
        ProposedEdit {
            old_text: old.into(),
            new_text: new.into(),
            replace_all,
        }
    }

    fn edits(list: Vec<ProposedEdit>) -> ProposedChange {
        ProposedChange::Edits {
            edits: list,
            crlf_as_lf: false,
        }
    }

    /// A per-test directory for the files a proposal is read from (the scratch directory serves: it is
    /// private and unique, and `cleanup` removes it).
    fn project() -> ScratchDir {
        ScratchDir::in_dir(&tmp()).unwrap()
    }

    #[test]
    fn a_proposal_applies_its_edits_to_the_file_on_disk() {
        let dir = project();
        let file = dir.path().join("lib.rs");
        std::fs::write(&file, "fn a() {}\nfn b() {}\nlet x = 1; let x = 1;\n").unwrap();
        let change = edits(vec![
            edit("fn a() {}", "fn a() { 1 }", false),
            // Applied to the text the first replacement left, as the agent's own tool applies them.
            edit("fn a() { 1 }\nfn b", "fn a() { 2 }\nfn b", false),
            edit("let x = 1;", "let y = 2;", true),
        ]);
        let (base, proposed) = proposal_texts(&file, &change).unwrap();
        assert_eq!(base, "fn a() {}\nfn b() {}\nlet x = 1; let x = 1;\n");
        assert_eq!(proposed, "fn a() { 2 }\nfn b() {}\nlet y = 2; let y = 2;\n");
        let (base, proposed) = proposal_texts(
            &file,
            &ProposedChange::Content {
                content: "new\n".into(),
            },
        )
        .unwrap();
        assert_eq!(base, "fn a() {}\nfn b() {}\nlet x = 1; let x = 1;\n");
        assert_eq!(proposed, "new\n");
        dir.cleanup();
    }

    #[test]
    fn a_proposal_whose_old_text_is_gone_is_refused_as_stale() {
        let dir = project();
        let file = dir.path().join("lib.rs");
        std::fs::write(&file, "fn a() { changed since }\n").unwrap();
        let refused = proposal_texts(&file, &edits(vec![edit("fn a() {}", "fn a() { 1 }", false)])).unwrap_err();
        assert_eq!(
            refused,
            ProposalRefused::Stale {
                path: file.display().to_string()
            }
        );
        assert_eq!(
            refused.to_string(),
            format!("the proposal no longer applies to {} as it is on disk", file.display())
        );
        // A later replacement whose text the earlier ones removed is just as stale.
        let refused = proposal_texts(
            &file,
            &edits(vec![edit("changed", "kept", false), edit("changed", "again", false)]),
        )
        .unwrap_err();
        assert!(matches!(refused, ProposalRefused::Stale { .. }), "{refused:?}");
        dir.cleanup();
    }

    #[test]
    fn an_old_text_found_twice_without_replace_all_is_stale() {
        let dir = project();
        let file = dir.path().join("lib.rs");
        std::fs::write(&file, "x = 1\nx = 1\n").unwrap();
        let once = proposal_texts(&file, &edits(vec![edit("x = 1", "x = 2", false)])).unwrap_err();
        assert!(matches!(once, ProposalRefused::Stale { .. }), "{once:?}");
        let all = proposal_texts(&file, &edits(vec![edit("x = 1", "x = 2", true)])).unwrap();
        assert_eq!(all.1, "x = 2\nx = 2\n");
        let empty = proposal_texts(&file, &edits(vec![edit("", "y", true)])).unwrap_err();
        assert!(
            matches!(empty, ProposalRefused::Stale { .. }),
            "an empty old text names no place"
        );
        dir.cleanup();
    }

    #[test]
    fn a_proposal_for_a_new_file_diffs_against_nothing() {
        let dir = project();
        let file = dir.path().join("src").join("new.rs");
        let (base, proposed) = proposal_texts(
            &file,
            &ProposedChange::Content {
                content: "fn main() {}\n".into(),
            },
        )
        .unwrap();
        assert_eq!(base, "");
        assert_eq!(proposed, "fn main() {}\n");
        assert!(!file.exists(), "looking never makes the file");
        dir.cleanup();
    }

    #[test]
    fn a_missing_file_passes_only_for_whole_content() {
        let dir = project();
        let file = dir.path().join("gone.rs");
        assert!(proposal_texts(&file, &ProposedChange::Content { content: "x".into() }).is_ok());
        let refused = proposal_texts(&file, &edits(vec![edit("a", "b", false)])).unwrap_err();
        assert_eq!(
            refused,
            ProposalRefused::Missing {
                path: file.display().to_string()
            }
        );
        assert_eq!(refused.to_string(), format!("no such file: {}", file.display()));
        dir.cleanup();
    }

    #[test]
    fn a_file_over_4_mib_is_refused() {
        let dir = project();
        let file = dir.path().join("big.txt");
        std::fs::write(&file, vec![b'a'; PROPOSAL_MAX_BYTES as usize + 1]).unwrap();
        let refused = proposal_texts(&file, &ProposedChange::Content { content: String::new() }).unwrap_err();
        assert_eq!(
            refused,
            ProposalRefused::TooLarge {
                path: file.display().to_string(),
                bytes: PROPOSAL_MAX_BYTES + 1,
            }
        );
        std::fs::write(&file, vec![b'a'; PROPOSAL_MAX_BYTES as usize]).unwrap();
        assert!(
            proposal_texts(&file, &ProposedChange::Content { content: String::new() }).is_ok(),
            "the limit itself passes"
        );
        std::fs::write(&file, [0xff, 0xfe, 0x00]).unwrap();
        let refused = proposal_texts(&file, &ProposedChange::Content { content: String::new() }).unwrap_err();
        assert!(matches!(refused, ProposalRefused::NotText { .. }), "{refused:?}");
        dir.cleanup();
    }

    /// An agent can name a FIFO; opening one for reading with no writer waits for a writer forever unless
    /// the open is non-blocking. Run on a thread the test gives a deadline, so a regression fails here
    /// instead of hanging the suite.
    #[test]
    fn a_fifo_with_no_writer_is_refused_without_waiting() {
        let dir = project();
        let fifo = dir.path().join("pipe");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `c_path` is a valid NUL-terminated path that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0, "mkfifo");
        let (tx, rx) = std::sync::mpsc::channel();
        let probe = fifo.clone();
        std::thread::spawn(move || {
            let _ = tx.send(proposal_texts(
                &probe,
                &ProposedChange::Content { content: String::new() },
            ));
        });
        let answer = rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("reading a FIFO with no writer waited");
        assert_eq!(
            answer.unwrap_err(),
            ProposalRefused::NotAFile {
                path: fifo.display().to_string()
            }
        );
        dir.cleanup();
    }

    /// The proposed text is built on the worker and written from the GTK thread; one short old text replaced
    /// everywhere by a long new one would otherwise turn a file under the limit into gigabytes.
    #[test]
    fn a_proposal_whose_text_would_pass_the_limit_is_refused_before_it_is_built() {
        let dir = project();
        let file = dir.path().join("big.txt");
        let limit = PROPOSAL_MAX_BYTES as usize;
        std::fs::write(&file, vec![b'a'; limit]).unwrap();
        let long = "b".repeat(1024);
        let refused = proposal_texts(&file, &edits(vec![edit("a", &long, true)])).unwrap_err();
        assert_eq!(
            refused,
            ProposalRefused::TooLarge {
                path: file.display().to_string(),
                bytes: PROPOSAL_MAX_BYTES * 1024,
            }
        );
        // One replacement that grows the text one byte past the limit is refused as well.
        let mut one_mark = vec![b'a'; limit];
        one_mark[0] = b'x';
        std::fs::write(&file, one_mark).unwrap();
        let refused = proposal_texts(&file, &edits(vec![edit("x", "xy", false)])).unwrap_err();
        assert!(
            matches!(refused, ProposalRefused::TooLarge { bytes, .. } if bytes == PROPOSAL_MAX_BYTES + 1),
            "{refused:?}"
        );
        // A replacement that keeps the size passes.
        assert!(proposal_texts(&file, &edits(vec![edit("x", "y", false)])).is_ok());
        // Whole content past the limit is refused too, whether or not a file is there.
        for target in [file.clone(), dir.path().join("new.txt")] {
            let refused = proposal_texts(
                &target,
                &ProposedChange::Content {
                    content: "c".repeat(limit + 1),
                },
            )
            .unwrap_err();
            assert!(matches!(refused, ProposalRefused::TooLarge { .. }), "{refused:?}");
        }
        dir.cleanup();
    }

    #[test]
    fn a_crlf_file_takes_edits_matched_against_its_lines_with_lf_ends() {
        let dir = project();
        let file = dir.path().join("win.txt");
        std::fs::write(&file, "one\r\ntwo\r\nthree\r\n").unwrap();
        // Old and new texts written with LF: the result keeps the file's CR-LF ends.
        let (base, proposed) = proposal_texts(&file, &edits(vec![edit("one\ntwo", "one\nTWO\nmore", false)])).unwrap();
        assert_eq!(base, "one\r\ntwo\r\nthree\r\n");
        assert_eq!(proposed, "one\r\nTWO\r\nmore\r\nthree\r\n");
        // Texts that match the bytes as they are are applied as they are.
        let (_, proposed) = proposal_texts(&file, &edits(vec![edit("one\r\ntwo", "x", false)])).unwrap();
        assert_eq!(proposed, "x\r\nthree\r\n");
        // A file with LF ends on most lines is written back with LF throughout.
        std::fs::write(&file, "one\r\ntwo\nthree\n").unwrap();
        let (_, proposed) = proposal_texts(&file, &edits(vec![edit("one\ntwo", "x", false)])).unwrap();
        assert_eq!(proposed, "x\nthree\n");
        // Text in neither form is still stale.
        let refused = proposal_texts(&file, &edits(vec![edit("one\nthree", "x", false)])).unwrap_err();
        assert!(matches!(refused, ProposalRefused::Stale { .. }), "{refused:?}");
        dir.cleanup();
    }

    #[test]
    fn a_tool_that_reads_crlf_as_lf_is_shown_its_own_result_even_when_raw_bytes_match() {
        let dir = project();
        let file = dir.path().join("win.txt");
        std::fs::write(&file, "one\r\ntwo\r\n").unwrap();
        let lf_reader = |list| ProposedChange::Edits {
            edits: list,
            crlf_as_lf: true,
        };
        // "two" matches the bytes as they are, but the tool's write puts CR-LF after every line it made.
        let (_, proposed) = proposal_texts(&file, &lf_reader(vec![edit("two", "TWO\nmore", false)])).unwrap();
        assert_eq!(proposed, "one\r\nTWO\r\nmore\r\n");
        // An agent that matches raw bytes is shown the raw result.
        let (_, proposed) = proposal_texts(&file, &edits(vec![edit("two", "TWO\nmore", false)])).unwrap();
        assert_eq!(proposed, "one\r\nTWO\nmore\r\n");
        // A file with LF ends on most lines: the tool writes LF throughout, its CR-LF line included.
        std::fs::write(&file, "one\r\ntwo\nthree\n").unwrap();
        let (_, proposed) = proposal_texts(&file, &lf_reader(vec![edit("three", "3", false)])).unwrap();
        assert_eq!(proposed, "one\ntwo\n3\n");
        // A file without CR-LF is the same in either reading.
        std::fs::write(&file, "one\ntwo\n").unwrap();
        let (_, proposed) = proposal_texts(&file, &lf_reader(vec![edit("two", "TWO\nmore", false)])).unwrap();
        assert_eq!(proposed, "one\nTWO\nmore\n");
        dir.cleanup();
    }

    #[test]
    fn a_directory_is_refused() {
        let dir = project();
        let sub = dir.path().join("src");
        std::fs::create_dir(&sub).unwrap();
        for change in [
            ProposedChange::Content { content: String::new() },
            edits(vec![edit("a", "b", false)]),
        ] {
            let refused = proposal_texts(&sub, &change).unwrap_err();
            assert_eq!(
                refused,
                ProposalRefused::NotAFile {
                    path: sub.display().to_string()
                }
            );
            assert_eq!(refused.to_string(), format!("not a regular file: {}", sub.display()));
        }
        dir.cleanup();
    }

    #[test]
    fn the_proposal_never_writes_the_real_file() {
        let mut dir = project();
        let file = dir.path().join("keep.rs");
        std::fs::write(&file, "old\n").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let before = std::fs::metadata(&file).unwrap().modified().unwrap();
        let (base, proposed) = proposal_texts(&file, &edits(vec![edit("old", "new", false)])).unwrap();
        let request = dir
            .prepare_proposal(&file.display().to_string(), &base, &proposed)
            .unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "old\n");
        assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), before);
        let value = decode(request.hex());
        assert_eq!(value["op"], "diff");
        assert_eq!(value["name"], file.display().to_string());
        let base_file = PathBuf::from(value["base"].as_str().unwrap());
        let proposed_file = PathBuf::from(value["proposed"].as_str().unwrap());
        assert_ne!(base_file, file);
        assert_ne!(proposed_file, file);
        assert_eq!(std::fs::read_to_string(&base_file).unwrap(), "old\n");
        assert_eq!(std::fs::read_to_string(&proposed_file).unwrap(), "new\n");
        dir.cleanup();
    }

    #[test]
    fn scratch_names_carry_no_path_separator() {
        let mut dir = project();
        for name in [
            "../../etc/passwd",
            "/abs/dir/main.rs",
            "a\\b/..",
            "..",
            "",
            "dir/",
            "naïve name\n.rs",
        ] {
            let value = decode(dir.prepare_proposal(name, "a", "b").unwrap().hex());
            assert_eq!(value["name"], name, "the real name goes to nvim as data");
            for key in ["base", "proposed"] {
                let made = PathBuf::from(value[key].as_str().unwrap());
                assert_eq!(made.parent(), Some(dir.path()), "{name:?}: {}", made.display());
                let file_name = made.file_name().unwrap().to_string_lossy().into_owned();
                assert!(
                    file_name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
                    "{file_name:?}"
                );
            }
        }
        dir.cleanup();
    }

    #[test]
    fn nvim_gets_one_guarded_loader_and_one_variable() {
        let dir = ScratchDir::in_dir(&tmp()).unwrap();
        let env = dir.child_env();
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].0, "EITRI_SCRATCH_LUA");
        assert!(Path::new(&env[0].1).exists(), "the snippet is on disk");
        assert_eq!(dir.nvim_args(), vec!["--cmd".to_string(), LOADER_CMD.to_string()]);
        assert!(
            LOADER_CMD.contains("pcall(dofile, p)") && LOADER_CMD.contains("p ~= ''"),
            "never dofile(nil): stdin is the RPC pipe"
        );
        dir.cleanup();
    }
}
