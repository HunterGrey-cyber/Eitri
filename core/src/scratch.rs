//! The shell -> nvim half of R3 and C5 (keymap/tabs spec §4.2, §4.3: "The existing RPC in
//! `LiveHarness` should be used, not a new socket"). One `nvim_input` of a `<Cmd>lua` call whose only
//! argument is hex (phase 3 ruling 18); bodies travel as files in a per-window 0700 directory; an
//! edit's result comes back as a marker file the agent panel's 33 ms tick polls.

use std::io;
use std::path::{Path, PathBuf};

const DIR_PREFIX: &str = "nv-sc-";
/// No socket lives here; the sweep's socket probe finds nothing and the pid alone decides. The
/// namespace caveat `instance_dir::sweep_stale_instance_dirs` documents applies: a live window in
/// another pid namespace sharing `TMPDIR` could lose its scratch directory, and costs a failed
/// `Ctrl+g` with a notice, not a crash.
const NO_SOCKET: &str = "none";
const LUA_NAME: &str = "nvim_scratch.lua";
const NVIM_SCRATCH_LUA: &str = include_str!("nvim_scratch.lua");

pub const LOADER_CMD: &str = "lua local p = vim.env.NEOVIBE_SCRATCH_LUA; if p and p ~= '' then pcall(dofile, p) end";

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
        format!("<Cmd>lua NeovibeScratch.call('{}')<CR>", self.hex)
    }
}

/// `gf` (N2): `:edit <path>` and the line.
pub fn open_request(path: &Path, line: Option<u32>) -> ScratchRequest {
    ScratchRequest::of(serde_json::json!({ "op": "open", "path": path.to_string_lossy(), "line": line }))
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

pub struct ScratchDir {
    dir: PathBuf,
    lua_path: PathBuf,
    next: u64,
}

impl ScratchDir {
    /// `None`, logged, when the directory cannot be made: `Ctrl+g` then answers with a notice.
    pub fn new() -> Option<Self> {
        let tmp = std::env::temp_dir();
        crate::instance_dir::sweep_stale_instance_dirs(&tmp, DIR_PREFIX, NO_SOCKET, "scratch");
        Self::in_dir(&tmp)
    }

    pub fn in_dir(tmp: &Path) -> Option<Self> {
        use std::os::unix::fs::DirBuilderExt;
        let dir = crate::instance_dir::instance_dir_path(tmp, DIR_PREFIX);
        if let Err(e) = std::fs::DirBuilder::new().mode(0o700).create(&dir) {
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

    pub fn child_env(&self) -> Vec<(String, String)> {
        vec![("NEOVIBE_SCRATCH_LUA".to_string(), self.lua_path.display().to_string())]
    }

    pub fn nvim_args(&self) -> Vec<String> {
        vec!["--cmd".to_string(), LOADER_CMD.to_string()]
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next;
        self.next += 1;
        id
    }

    /// R3: a read-only buffer holding `text`. The title names the file, so `:ls` says what it is.
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

    /// C5: the draft in an editable buffer whose wipe writes the marker.
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

    /// Ruling 18: only `[0-9a-f]` travels through `nvim_input`, so no path can close the `<Cmd>` or
    /// open a key notation of its own.
    #[test]
    fn a_request_crosses_nvim_input_as_hex_only() {
        let hostile = Path::new("/tmp/a<CR>:!rm -rf ~<CR>|b'.md");
        let request = open_request(hostile, Some(42));
        let keys = request.input_keys();
        assert!(
            keys.starts_with("<Cmd>lua NeovibeScratch.call('") && keys.ends_with("')<CR>"),
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

    #[test]
    fn nvim_gets_one_guarded_loader_and_one_variable() {
        let dir = ScratchDir::in_dir(&tmp()).unwrap();
        let env = dir.child_env();
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].0, "NEOVIBE_SCRATCH_LUA");
        assert!(Path::new(&env[0].1).exists(), "the snippet is on disk");
        assert_eq!(dir.nvim_args(), vec!["--cmd".to_string(), LOADER_CMD.to_string()]);
        assert!(
            LOADER_CMD.contains("pcall(dofile, p)") && LOADER_CMD.contains("p ~= ''"),
            "never dofile(nil): stdin is the RPC pipe"
        );
        dir.cleanup();
    }
}
