//! Which of a language server's `file://` URIs Neovim can open as files, and the rewrite of the ones it cannot.
//!
//! A remote project's mirror holds what sync carries: the files under the project root that the project's effective
//! excludes do not match. A server also names files sync does not carry (a dependency under `~/.cargo`, the standard
//! library, a generated file in `target/`), and those URIs would open a local file of the same path, or nothing. The
//! bridge rewrites them to `eitri-remote://<host>/<path>`, which `eitri.nvim` opens as a read-only buffer read from the
//! server (`eitri host read`), and rewrites Neovim's `eitri-remote://` URIs back before they reach the server.
//!
//! The decision is made from the path alone, never from whether the file is on this machine yet: a file sync has not
//! brought is still the mirror's, and turning it read-only would be wrong. Only the members the protocol types as a
//! URI are rewritten ([`URI_MEMBERS`], and the keys of the maps keyed by one, [`URI_KEYED`]), and only when the value
//! *begins with* the scheme: a document's text, an edit's new text, a completion's label or a message is data, and a
//! file whose text starts like a URI is sent and shown as it is. A bare string in a command's `arguments` that begins
//! with the scheme is treated as a URI too: a client's handler for the command opens it.
//!
//! The `eitri-remote://` names are the bridge's to write. A language server never has one of its own, and one it sends
//! could name another host's file (`eitri-remote://<other host>/...`), which Neovim would read from that host and show
//! to this server: [`names_the_scheme`] finds such a string anywhere in a message, and the bridge refuses the message.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use super::documents::percent_decode;
use crate::remote_sync::ignores::Matcher;

/// The scheme of the buffers `eitri.nvim` opens for a path the mirror does not hold.
pub const SCHEME: &str = "eitri-remote://";

/// The members whose value is a URI (`DocumentUri` or `URI` in the protocol): a `Location`, `LocationLink`,
/// `TextDocumentIdentifier`, `WorkspaceFolder`, file operation, `ConfigurationItem`, `RelativePattern`, `DocumentLink`
/// and `CodeDescription`, wherever they sit (a command's arguments, a completion's `data`).
pub const URI_MEMBERS: &[&str] = &[
    "uri",
    "targetUri",
    "oldUri",
    "newUri",
    "rootUri",
    "scopeUri",
    "baseUri",
    "target",
    "href",
];

/// The members whose string value is text a person reads or an edit inserts, never a name Neovim opens: a document's
/// or an edit's text, a completion's label and what it inserts, a message, a hover's markup value, a symbol's name.
/// [`names_the_scheme`] does not look inside them (a diagnostic for a line that begins `eitri-remote://` is still a
/// diagnostic); objects and arrays under them are searched as anywhere else.
pub const TEXT_MEMBERS: &[&str] = &[
    "text",
    "newText",
    "insertText",
    "label",
    "filterText",
    "sortText",
    "value",
    "message",
    "detail",
    "documentation",
    "contents",
    "name",
    "description",
    "title",
];

/// The member that holds a command's arguments: a bare string in it that begins with `file:///` is a URI too (a client
/// handler opens it), so it is rewritten like a URI member.
const ARGUMENTS: &str = "arguments";

/// The members whose value is a map keyed by URI: `WorkspaceEdit.changes` and a diagnostic report's
/// `relatedDocuments`.
pub const URI_KEYED: &[&str] = &["changes", "relatedDocuments"];

const FILE_PREFIX: &str = "file:///";

/// What sync carries for one project: its root and the effective excludes.
#[derive(Clone, Debug)]
pub struct Mirror {
    root: PathBuf,
    excludes: Matcher,
}

impl Mirror {
    pub fn new(root: PathBuf, excludes: Matcher) -> Self {
        Self { root, excludes }
    }

    /// `path` relative to the root, when it is under it (or is it, as the empty string) in normal form.
    fn relative<'a>(&self, path: &'a Path) -> Option<&'a str> {
        let rel = path.strip_prefix(&self.root).ok()?;
        rel.components()
            .all(|part| matches!(part, Component::Normal(_)))
            .then(|| rel.to_str())
            .flatten()
    }

    /// Whether sync carries the file at `path`: inside the root and not excluded. The root itself is no file.
    pub fn carried(&self, path: &Path) -> bool {
        self.relative(path)
            .is_some_and(|rel| !rel.is_empty() && !self.excludes.ignored(rel, false))
    }

    /// Whether the mirror is where Neovim should look for `path`: the root, a directory or a file under it that the
    /// excludes do not match.
    pub fn holds(&self, path: &Path) -> bool {
        self.relative(path)
            .is_some_and(|rel| rel.is_empty() || !self.excludes.ignored(rel, false))
    }
}

/// `host` percent-encoded for the authority of a URI: everything but letters, digits and `-._~`.
pub fn encode_host(host: &str) -> String {
    let mut out = String::with_capacity(host.len());
    for byte in host.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The `eitri-remote://` URI of `path` (absolute) on `host`, for a path the mirror does not hold: the one name of that
/// file, whether a person, the panel or a server's location (through [`Rewriter::to_nvim`]) names it. Everything
/// outside the unreserved characters, the sub-delimiters, `:`, `@` and `/` is percent-encoded, as Neovim encodes the
/// path of a `file://` URI.
pub fn remote_uri(host: &str, path: &str) -> String {
    format!("{SCHEME}{}{}", encode_host(host), encode_path(path))
}

/// `path` percent-encoded for an `eitri-remote://` name: everything outside the unreserved characters, the
/// sub-delimiters, `:`, `@` and `/`.
fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@/".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The rewrite of a bridge's messages for one project on one host.
#[derive(Clone, Debug)]
pub struct Rewriter {
    /// `eitri-remote://<host as typed, percent-encoded>/`.
    prefix: String,
    mirror: Mirror,
}

impl Rewriter {
    pub fn new(host: &str, root: &Path, excludes: Matcher) -> Self {
        Self::of_mirror(host, Mirror::new(root.to_path_buf(), excludes))
    }

    pub fn of_mirror(host: &str, mirror: Mirror) -> Self {
        Self {
            prefix: format!("{SCHEME}{}/", encode_host(host)),
            mirror,
        }
    }

    /// Every URI member of `message` (and key of a map keyed by URI) that begins with `file:///` and names a path
    /// outside the mirror becomes `eitri-remote://<host>/<path>`; a query or fragment stays where it was.
    pub fn to_nvim(&self, message: &mut Value) {
        walk(message, &|text| self.remote_of(text));
    }

    /// Every URI member (and key of a map keyed by URI) that begins with this host's `eitri-remote://<host>/` becomes
    /// `file:///<path>`.
    pub fn to_server(&self, message: &mut Value) {
        walk(message, &|text| {
            text.strip_prefix(&self.prefix)
                .map(|path| format!("{FILE_PREFIX}{path}"))
        });
    }

    /// Whether a URI member of a message from Neovim (or a key of a map keyed by URI) names an `eitri-remote://`
    /// buffer of another host: that file is not this server's, and its text must not reach it.
    pub fn names_another_host(&self, message: &Value) -> bool {
        let mut found = false;
        visit_uris(message, &mut |text| {
            found |= begins_with_scheme(text) && !text.starts_with(&self.prefix);
        });
        found
    }

    /// The `eitri-remote://` name of a `file:///` URI outside the mirror. A path that decodes is encoded again as
    /// [`remote_uri`] encodes it, so a server's location names the same buffer that `:EitriRemote open` and the panel
    /// name for that file (a server's own escapes, `%2B` for `+` or lowercase hex, would give the file a second
    /// buffer); servers decode the path, so the `file:///` URI [`Self::to_server`] makes of it names the same file. A
    /// path that does not decode keeps the server's bytes.
    fn remote_of(&self, text: &str) -> Option<String> {
        let path = text.strip_prefix(FILE_PREFIX)?;
        let end = path.find(['?', '#']).unwrap_or(path.len());
        // A path that does not decode, or holds a NUL, is not one the mirror can hold: it is read from the server.
        let Some(decoded) = percent_decode(&path[..end]).filter(|decoded| !decoded.contains('\0')) else {
            return Some(format!("{}{path}", self.prefix));
        };
        if self.mirror.holds(Path::new(&format!("/{decoded}"))) {
            return None;
        }
        Some(format!("{}{}{}", self.prefix, encode_path(&decoded), &path[end..]))
    }
}

/// Whether `text` begins with the `eitri-remote:` scheme, in any case (a scheme is case-insensitive, and Neovim's
/// patterns are on a file system that ignores case).
fn begins_with_scheme(text: &str) -> bool {
    let scheme = &SCHEME.as_bytes()[..SCHEME.len() - 2];
    text.as_bytes()
        .get(..scheme.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(scheme))
}

/// Whether any string of `message`, in any member, or any object key, begins with the `eitri-remote:` scheme. Every
/// string counts, not only the URI members: Neovim, or a plugin, may open a URI from wherever it sits (a command's
/// arguments, a link in a hover). The exception is a string held by one of the [`TEXT_MEMBERS`]: it is text, and text
/// that begins like a name (a formatting edit for such a line, a diagnostic about it) is not one.
pub fn names_the_scheme(message: &Value) -> bool {
    match message {
        Value::String(text) => begins_with_scheme(text),
        Value::Array(items) => items.iter().any(names_the_scheme),
        Value::Object(map) => map.iter().any(|(key, item)| {
            begins_with_scheme(key)
                || if TEXT_MEMBERS.contains(&key.as_str()) {
                    names_the_scheme_beyond_text(item)
                } else {
                    names_the_scheme(item)
                }
        }),
        _ => false,
    }
}

/// The value of a text member: its own strings (and those of an array it holds) are text; what is nested in it is
/// searched as anywhere else.
fn names_the_scheme_beyond_text(value: &Value) -> bool {
    match value {
        Value::Array(items) => items.iter().any(names_the_scheme_beyond_text),
        Value::String(_) => false,
        other => names_the_scheme(other),
    }
}

/// Calls `see` with each URI member's string and each key of a map keyed by URI. Messages are parsed with a depth
/// limit, so the recursion is as deep as the parser let the message be.
fn visit_uris(value: &Value, see: &mut dyn FnMut(&str)) {
    match value {
        Value::Array(items) => items.iter().for_each(|item| visit_uris(item, see)),
        Value::Object(map) => {
            for (key, item) in map {
                match item {
                    Value::String(text) if URI_MEMBERS.contains(&key.as_str()) => see(text),
                    Value::Object(keyed) if URI_KEYED.contains(&key.as_str()) => {
                        keyed.keys().for_each(|uri| see(uri));
                        visit_uris(item, see);
                    }
                    Value::Array(arguments) if key == ARGUMENTS => {
                        for argument in arguments {
                            match argument {
                                Value::String(text) => see(text),
                                other => visit_uris(other, see),
                            }
                        }
                    }
                    _ => visit_uris(item, see),
                }
            }
        }
        _ => {}
    }
}

/// Replaces each URI member's string, and each key of a map keyed by URI, that `rewrite` gives an answer for.
/// Messages are parsed with a depth limit, so the recursion is as deep as the parser let the message be.
fn walk(value: &mut Value, rewrite: &dyn Fn(&str) -> Option<String>) {
    match value {
        Value::Array(items) => items.iter_mut().for_each(|item| walk(item, rewrite)),
        Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                match item {
                    Value::String(text) if URI_MEMBERS.contains(&key.as_str()) => {
                        if let Some(new) = rewrite(text) {
                            *text = new;
                        }
                    }
                    Value::Object(keyed) if URI_KEYED.contains(&key.as_str()) => {
                        rewrite_keys(keyed, rewrite);
                        walk(item, rewrite);
                    }
                    Value::Array(arguments) if key == ARGUMENTS => {
                        for argument in arguments.iter_mut() {
                            match argument {
                                Value::String(text) => {
                                    if let Some(new) = rewrite(text) {
                                        *text = new;
                                    }
                                }
                                other => walk(other, rewrite),
                            }
                        }
                    }
                    _ => walk(item, rewrite),
                }
            }
        }
        _ => {}
    }
}

/// The keys of a map keyed by URI, rewritten in place; the order of the others is kept.
fn rewrite_keys(map: &mut serde_json::Map<String, Value>, rewrite: &dyn Fn(&str) -> Option<String>) {
    if map.keys().any(|key| rewrite(key).is_some()) {
        for (key, item) in std::mem::take(map) {
            map.insert(rewrite(&key).unwrap_or(key), item);
        }
    }
}
