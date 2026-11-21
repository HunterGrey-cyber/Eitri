//! GTK-free identities and shared contracts for remote project sync.

pub mod actor;
pub mod conflicts;
pub mod daemon;
pub mod engine;
pub mod flush;
pub mod git_replica;
pub mod git_shim;
pub mod ignores;
pub mod local_git;
pub mod local_path;
pub mod mirror_identity;
pub mod mutagen;
pub mod mutagen_json;
pub mod other_sync;
pub mod quiet;
pub mod recreate;
pub mod sessions;
pub mod shim_dir;
pub mod shown_once;
pub mod status_file;
pub mod status_schema;
pub mod transport;
pub mod tripwire;

use sha2::{Digest, Sha256};
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// What the window tells the language-server wrap of `eitri.nvim` (`remote_dev.lsp_grace`, `remote_dev.lsp_local`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LspOptions {
    /// Seconds the helper keeps a server whose bridge is lost.
    pub grace: u32,
    /// Names of the servers kept on this machine.
    pub local_servers: Vec<String>,
}

impl Default for LspOptions {
    fn default() -> Self {
        Self {
            grace: crate::lua::config::REMOTE_DEV_LSP_GRACE_DEFAULT,
            local_servers: Vec::new(),
        }
    }
}

impl LspOptions {
    /// The Lua table `setup` takes as `lsp`.
    fn lua_table(&self) -> String {
        use crate::onboarding::lua_text::quote_bytes;
        let names: Vec<String> = self
            .local_servers
            .iter()
            .map(|name| quote_bytes(name.as_bytes()))
            .collect();
        format!("{{grace={},local_servers={{{}}}}}", self.grace, names.join(","))
    }
}

/// The embedded editor's early startup command, including every module for a plugin-free runtimepath, with the
/// language-server options at their defaults.
pub fn nvim_loader(root: &Path, program: &str) -> String {
    nvim_loader_with(root, program, &LspOptions::default())
}

/// [`nvim_loader`] passing the language-server wrap `lsp` as setup's `lsp` option.
pub fn nvim_loader_with(root: &Path, program: &str, lsp: &LspOptions) -> String {
    use crate::onboarding::lua_text::quote_bytes;
    format!(
        "lua package.preload['eitri.remote']=function()\n{}\nend; package.preload['eitri.remote.sync']=function()\n{}\nend; \
         package.preload['eitri.remote.exec']=function()\n{}\nend; package.preload['eitri.remote.buffers']=function()\n{}\nend; \
         package.preload['eitri.remote.lsp']=function()\n{}\nend; package.preload['eitri.remote.text']=function()\n{}\nend; \
         require('eitri.remote').setup({{embedded=true,root={},cmd={},lsp={}}})",
        include_str!("../../../nvim/eitri.nvim/lua/eitri/remote/init.lua"),
        include_str!("../../../nvim/eitri.nvim/lua/eitri/remote/sync.lua"),
        include_str!("../../../nvim/eitri.nvim/lua/eitri/remote/exec.lua"),
        include_str!("../../../nvim/eitri.nvim/lua/eitri/remote/buffers.lua"),
        include_str!("../../../nvim/eitri.nvim/lua/eitri/remote/lsp.lua"),
        include_str!("../../../nvim/eitri.nvim/lua/eitri/remote/text.lua"),
        quote_bytes(root.as_os_str().as_bytes()),
        quote_bytes(program.as_bytes()),
        lsp.lua_table(),
    )
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SyncKey(String);

impl SyncKey {
    pub fn new(host_key_sha256: &str, root: &Path) -> Self {
        let mut hash = Sha256::new();
        hash.update(b"eitri-remote-sync\0");
        hash.update(host_key_sha256.as_bytes());
        hash.update(b"\0");
        hash.update(root.as_os_str().as_bytes());
        Self(format!("{:x}", hash.finalize())[..16].to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SyncKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncError {
    Unsupported(String),
    Engine(String),
    Helper(String),
    Io(String),
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(text) | Self::Engine(text) | Self::Helper(text) | Self::Io(text) => f.write_str(text),
        }
    }
}

impl std::error::Error for SyncError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_key_uses_its_own_domain() {
        let root = Path::new("/srv/project");
        let key = SyncKey::new("SHA256:example", root);
        let expected = Sha256::digest(b"eitri-remote-sync\0SHA256:example\0/srv/project");
        assert_eq!(key.as_str(), &format!("{expected:x}")[..16]);
        assert_eq!(key.to_string(), key.as_str());
        assert_ne!(key.as_str(), crate::remote_project::project_key(root));
        let trust = crate::project_trust::RemoteRoot {
            host_key_sha256: "SHA256:example".into(),
            root: root.into(),
        };
        assert_ne!(key.as_str(), trust.record_id());
        assert_ne!(key, SyncKey::new("SHA256:other", root));
        assert_ne!(key, SyncKey::new("SHA256:example", Path::new("/srv/other")));
        use std::os::unix::ffi::OsStrExt;
        let raw = Path::new(std::ffi::OsStr::from_bytes(b"/srv/\xff"));
        let expected = Sha256::digest(b"eitri-remote-sync\0SHA256:example\0/srv/\xff");
        assert_eq!(
            SyncKey::new("SHA256:example", raw).as_str(),
            &format!("{expected:x}")[..16]
        );
    }

    #[test]
    fn the_loader_preloads_every_module_of_the_remote_directory_and_passes_the_language_server_options() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../nvim/eitri.nvim/lua/eitri/remote");
        let loader = nvim_loader(Path::new("/srv/project"), "eitri");
        for entry in std::fs::read_dir(&dir).unwrap() {
            let file = entry.unwrap().file_name().into_string().unwrap();
            let module = match file.strip_suffix(".lua").unwrap() {
                "init" => "eitri.remote".to_owned(),
                name => format!("eitri.remote.{name}"),
            };
            assert!(
                loader.contains(&format!("package.preload['{module}']=function()")),
                "{module} is not preloaded: a window without the plugin on its runtimepath could not load it"
            );
        }
        // One argument of nvim's command line: Linux takes at most 128 KiB.
        assert!(loader.len() < 100 * 1024, "{} bytes", loader.len());
        assert!(
            loader.ends_with("lsp={grace=1800,local_servers={}}})"),
            "{}",
            &loader[loader.len() - 120..]
        );
        let options = LspOptions {
            grace: 90,
            local_servers: vec!["lua_ls".into(), "we\"ird".into()],
        };
        let loader = nvim_loader_with(Path::new("/srv/project"), "eitri", &options);
        assert!(
            loader.ends_with("lsp={grace=90,local_servers={\"lua_ls\",\"we\\\"ird\"}}})"),
            "{}",
            &loader[loader.len() - 120..]
        );
    }
}

/// Ways to proceed when a tree entry cannot be synchronized, by the side that reports it (`None` when the sync tool
/// counted entries without naming them). Fixing the entry or excluding its path settles either side. Git in a server
/// terminal helps only with an entry on the server: an entry this laptop could not send never reached the server, so
/// git there would act without it.
pub fn problem_actions(side: Option<engine::Side>, host: &str) -> String {
    match side {
        Some(engine::Side::Server) => format!(
            "fix the entry on the server, use eitri host sync exclude <path>, or run git in a terminal on {host}"
        ),
        Some(engine::Side::Laptop) => "fix the entry on this laptop or use eitri host sync exclude <path>".to_owned(),
        None => format!(
            "fix each entry on the side that reports it or use eitri host sync exclude <path>; for an entry on the \
             server, git in a terminal on {host} also works"
        ),
    }
}

/// [`problem_actions`] for a side as the status file spells it (`laptop` or `server`).
pub fn problem_actions_for(side: &str, host: &str) -> String {
    problem_actions(
        match side {
            "laptop" => Some(engine::Side::Laptop),
            "server" => Some(engine::Side::Server),
            _ => None,
        },
        host,
    )
}

/// `text` as it may go to a terminal: every control character (C0, DEL, C1), bidirectional
/// control and line or paragraph separator written as `\u{..}`. Lines about a remote project carry server file names
/// and server-side messages, which anyone able to write files on the server chooses; raw, an escape sequence among
/// them would be read by the person's terminal (a clipboard write, a window title, cursor moves). A newline is escaped
/// too, so a server name can never start a line of its own; Eitri's own messages that need several lines say each line
/// separately.
pub fn terminal_text(text: &str) -> String {
    let mut shown = String::with_capacity(text.len());
    for c in text.chars() {
        if crate::agent_bridge::is_control_like(c) {
            shown.extend(c.escape_unicode());
        } else {
            shown.push(c);
        }
    }
    shown
}

/// A JSON document as it may go to a terminal: every control-like character (see [`terminal_text`]) written as a JSON
/// `\uXXXX` escape. serde_json escapes only C0 controls, `"` and `\`, so a C1 control (CSI, OSC), DEL, a bidirectional
/// control or a line separator in a server file name would otherwise reach the terminal raw when `--json` output is
/// read by a person. Compact JSON holds such characters only inside strings, and each is in the Basic Multilingual
/// Plane, so the document stays valid and decodes to the same values.
pub fn terminal_json(document: &str) -> String {
    let mut shown = String::with_capacity(document.len());
    for c in document.chars() {
        if crate::agent_bridge::is_control_like(c) {
            shown.push_str(&format!("\\u{:04x}", u32::from(c)));
        } else {
            shown.push(c);
        }
    }
    shown
}

#[cfg(test)]
mod terminal_text_tests {
    #[test]
    fn terminal_json_escapes_controls_inside_strings_and_round_trips() {
        let value = serde_json::json!({"path": "a\u{9b}2Jb\u{9d}0;t\u{7}c\u{202e}d\u{2028}e\u{7f}f\ng\u{61c}"});
        let shown = super::terminal_json(&serde_json::to_string(&value).unwrap());
        assert!(!shown.chars().any(crate::agent_bridge::is_control_like), "{shown:?}");
        assert!(shown.contains("\\u009b2J") && shown.contains("\\u202e"), "{shown}");
        assert_eq!(serde_json::from_str::<serde_json::Value>(&shown).unwrap(), value);
    }

    #[test]
    fn terminal_text_keeps_an_untrusted_message_on_one_line() {
        let shown = super::terminal_text("a\u{1b}]52;c;eA==\u{7}b\u{9b}2J\u{202e}c\u{7f}\r\n\td");
        assert_eq!(
            shown,
            "a\\u{1b}]52;c;eA==\\u{7}b\\u{9b}2J\\u{202e}c\\u{7f}\\u{d}\\u{a}\\u{9}d"
        );
        assert_eq!(super::terminal_text("plain line: ok"), "plain line: ok");
    }
}
