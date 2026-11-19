//! Reading the workspace's own sources as text, for tests that hold an invariant by scanning code.
//!
//! A scanner that reads a path which no longer holds the code it guards passes on whatever is left. Every
//! reader here fails instead: a listed directory that is missing, unreadable or holds no file of the kind
//! asked for, a named file that cannot be read, and an anchor -- a definition the scan exists to cover --
//! that none of the files read contains. Paths are relative to the workspace root, so a test in any crate
//! names the same directories, and moving code between crates means editing one list, not a path per test.
//!
//! The guards of the crates share two scans: a crate manifest read as TOML ([`manifest_dependency_offenders`], with a
//! per-crate predicate and [`is_package_in_family`]; [`manifest_table`] for a guard that reads more of it) and the
//! socket names built outside `socket_path::in_dir` ([`unchecked_socket_offsets`], [`unrouted_socket_sites`] and
//! [`stale_never_bound`] for an allowlist).

use std::path::{Path, PathBuf};

/// The `src` directory of every product crate: what a scan that guards "the whole product" reads.
///
/// Every workspace member except `poc/` has its entry here, and a test fails when one does not
/// ([`product_crates_missing_from`]), so code that moves into a new crate stays under every whole-tree scan.
/// A scan that deliberately skips a crate names the skipped entry in [`product_dirs_except`] rather than
/// listing the directories it keeps.
pub const PRODUCT_SOURCES: &[&str] = &[
    "agent/src",
    "core/src",
    "desk/src",
    "mac/src",
    "neovide-editor/src",
    "panel/src",
    "remote/src",
    "server-proto/src",
    "server/src",
    "shell/src",
    "terminal/engine/src",
    "terminal/frame/src",
    "terminal/input/src",
    "terminal/render/src",
    "terminal/sync/src",
];

const NON_HOST_SOURCES: &[&str] = &[
    "agent/src",
    "neovide-editor/src",
    "remote/src",
    "server-proto/src",
    "server/src",
    "terminal/engine/src",
    "terminal/frame/src",
    "terminal/input/src",
    "terminal/render/src",
    "terminal/sync/src",
];

/// Product sources other than known non-host crates and the entries in `excluded`.
/// New product crates are scanned unless explicitly excluded.
///
/// # Panics
/// When an excluded entry is not on the list.
pub fn host_dirs_except(excluded: &[&str]) -> Vec<&'static str> {
    host_dirs_from(PRODUCT_SOURCES, excluded)
}

fn host_dirs_from<'a>(product: &[&'a str], excluded: &[&str]) -> Vec<&'a str> {
    for entry in NON_HOST_SOURCES {
        assert!(product.contains(entry), "{entry} is not a product source directory");
    }
    for entry in excluded {
        assert!(
            product.contains(entry) && !NON_HOST_SOURCES.contains(entry),
            "{entry} is not a host source directory"
        );
    }
    product
        .iter()
        .copied()
        .filter(|dir| !NON_HOST_SOURCES.contains(dir) && !excluded.contains(dir))
        .collect()
}

/// [`PRODUCT_SOURCES`], for a scan that reads all of the product.
pub fn product_dirs() -> Vec<&'static str> {
    PRODUCT_SOURCES.to_vec()
}

/// [`PRODUCT_SOURCES`] without the entries in `excluded`.
///
/// # Panics
/// When an excluded entry is not on the list (a typo, or a crate that was renamed).
pub fn product_dirs_except(excluded: &[&str]) -> Vec<&'static str> {
    for entry in excluded {
        assert!(
            PRODUCT_SOURCES.contains(entry),
            "{entry} is not a product source directory ({PRODUCT_SOURCES:?})"
        );
    }
    PRODUCT_SOURCES
        .iter()
        .copied()
        .filter(|dir| !excluded.contains(dir))
        .collect()
}

/// Every `.rs` file of the product, [`PRODUCT_SOURCES`] read through [`rust_sources`].
pub fn product_sources() -> Vec<Source> {
    rust_sources(PRODUCT_SOURCES)
}

/// The `members` of a workspace manifest's `[workspace]` table, in order.
///
/// # Panics
/// When the manifest has no `members = [ ... ]` list.
pub fn workspace_members(manifest: &str) -> Vec<String> {
    let manifest = manifest.parse::<toml::Table>().expect("valid workspace manifest TOML");
    manifest
        .get("workspace")
        .and_then(|workspace| workspace.get("members"))
        .and_then(toml::Value::as_array)
        .expect("the workspace manifest has a members array")
        .iter()
        .map(|member| member.as_str().expect("each workspace member is a path").to_owned())
        .collect()
}

fn dependency_tables(manifest: &toml::Table) -> impl Iterator<Item = (String, &toml::Table)> {
    // Cargo also accepts the underscore spellings in manifests before edition 2024.
    const KINDS: &[&str] = &[
        "dependencies",
        "dev-dependencies",
        "dev_dependencies",
        "build-dependencies",
        "build_dependencies",
    ];
    let targets = manifest
        .get("target")
        .map(|targets| targets.as_table().expect("targets are a table"))
        .into_iter()
        .flatten()
        .map(|(target, tables)| {
            (
                format!("target.{target:?}."),
                tables.as_table().expect("target settings are a table"),
            )
        });
    std::iter::once((String::new(), manifest))
        .chain(targets)
        .flat_map(|(scope, tables)| {
            KINDS.iter().filter_map(move |kind| {
                let deps = tables.get(*kind)?.as_table().expect("dependencies are a table");
                Some((format!("{scope}{kind}"), deps))
            })
        })
}

/// A crate manifest read as TOML, for a guard that checks more than which dependencies it names: whether one is
/// optional, which features turn it on.
///
/// # Panics
/// When the manifest is not valid TOML.
pub fn manifest_table(manifest: &str) -> toml::Table {
    manifest.parse::<toml::Table>().expect("valid crate manifest TOML")
}

/// The table and key of each forbidden dependency name or renamed package in a crate manifest.
pub fn manifest_dependency_offenders(manifest: &str, forbidden: impl Fn(&str) -> bool) -> Vec<String> {
    let manifest = manifest_table(manifest);
    let mut offenders = Vec::new();
    for (table, deps) in dependency_tables(&manifest) {
        for (name, spec) in deps {
            let package = spec.get("package").and_then(toml::Value::as_str);
            if forbidden(name) || package.is_some_and(&forbidden) {
                let package = package.map_or_else(String::new, |name| format!(" (package {name})"));
                offenders.push(format!("[{table}] {name}{package}"));
            }
        }
    }
    offenders
}

/// The local dependency directories of a workspace member, based at the workspace root.
pub fn manifest_path_dependencies(manifest: &str, workspace: &str, member: &Path) -> Vec<PathBuf> {
    let manifest = manifest_table(manifest);
    let workspace = workspace.parse::<toml::Table>().expect("valid workspace manifest TOML");
    let inherited = workspace.get("workspace").and_then(|table| table.get("dependencies"));
    dependency_tables(&manifest)
        .flat_map(|(_, deps)| deps.iter())
        .filter_map(|(name, spec)| {
            if spec.get("workspace").and_then(toml::Value::as_bool) == Some(true) {
                let spec = inherited?.get(name)?;
                Some(PathBuf::from(spec.get("path")?.as_str()?))
            } else {
                Some(member.join(spec.get("path")?.as_str()?))
            }
        })
        .collect()
}

/// The `src` of each product crate (a workspace member outside `poc/`) that `list` does not hold.
pub fn product_crates_missing_from(manifest: &str, list: &[&str]) -> Vec<String> {
    workspace_members(manifest)
        .into_iter()
        .filter(|member| !member.starts_with("poc/"))
        .map(|member| format!("{member}/src"))
        .filter(|dir| !list.contains(&dir.as_str()))
        .collect()
}

/// One file a scan read: its path relative to the workspace root (`/`-separated) and its text.
#[derive(Debug, Clone)]
pub struct Source {
    pub path: String,
    pub text: String,
}

/// The directory holding the workspace's `Cargo.toml`.
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("eitri-core sits one level below the workspace root")
        .to_path_buf()
}

/// Every file under each of `dirs` (relative to the workspace root), recursively, whose extension is one
/// of `extensions`, sorted by path.
///
/// # Panics
/// When `dirs` is empty, a directory is missing or unreadable, one of them holds no such file, or a file
/// cannot be read as UTF-8.
pub fn sources(dirs: &[&str], extensions: &[&str]) -> Vec<Source> {
    assert!(!dirs.is_empty(), "a scan with no directories reads nothing");
    let root = workspace_root();
    let mut out = Vec::new();
    for dir in dirs {
        let mut found = Vec::new();
        collect(&root.join(dir), extensions, &mut found);
        assert!(
            !found.is_empty(),
            "{dir} holds no {extensions:?} file: the scan's directory list is out of date"
        );
        for path in found {
            out.push(read_at(&root, &path));
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// [`sources`] over `.rs` files.
pub fn rust_sources(dirs: &[&str]) -> Vec<Source> {
    sources(dirs, &["rs"])
}

/// One file, relative to the workspace root.
///
/// # Panics
/// When it cannot be read.
pub fn read_required(path: impl AsRef<Path>) -> Source {
    let root = workspace_root();
    read_at(&root, &root.join(path))
}

/// The one file of `sources` whose code (comments and literals blanked, [`code_only`]) contains `needle`.
///
/// # Panics
/// When no file does, or more than one does.
pub fn file_with<'a>(sources: &'a [Source], needle: &str) -> &'a Source {
    let hits: Vec<&Source> = sources.iter().filter(|s| code_only(&s.text).contains(needle)).collect();
    match hits.as_slice() {
        [one] => one,
        [] => panic!("no scanned file contains `{needle}` (read: {})", read_dirs(sources)),
        many => panic!(
            "`{needle}` is in more than one file: {}",
            many.iter().map(|s| s.path.as_str()).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// The unique function named `name`, with comments, literals and test modules blanked.
///
/// # Panics
/// When no function or more than one function matches, naming the files searched or matched.
pub fn function_in<'a>(sources: &'a [Source], name: &str) -> (&'a Source, String) {
    function_body(sources, name, false)
}

/// The unique function named `name`, retaining its original comments and literals.
///
/// # Panics
/// When no function or more than one function matches, naming the files searched or matched.
pub fn function_in_raw<'a>(sources: &'a [Source], name: &str) -> (&'a Source, String) {
    function_body(sources, name, true)
}

fn function_body<'a>(sources: &'a [Source], name: &str, raw: bool) -> (&'a Source, String) {
    let mut hits = Vec::new();
    for source in sources {
        let code = code_only(&source.text);
        let code = without(&code, &modules(&code, "tests"));
        for body in functions(&code, name) {
            let start = body.as_ptr() as usize - code.as_ptr() as usize;
            let text = if raw {
                &source.text[start..start + body.len()]
            } else {
                body
            };
            hits.push((source, text.to_owned()));
        }
    }
    assert!(
        hits.len() == 1,
        "expected one function `{name}`, found {} (read: {}; matched: {})",
        hits.len(),
        sources.iter().map(|s| s.path.as_str()).collect::<Vec<_>>().join(", "),
        hits.iter().map(|(s, _)| s.path.as_str()).collect::<Vec<_>>().join(", "),
    );
    hits.pop().unwrap()
}

/// # Panics
/// Unless each of `anchors` appears in the code ([`code_only`]) of at least one of `sources`.
pub fn require_anchors(sources: &[Source], anchors: &[&str]) {
    let code: Vec<String> = sources.iter().map(|s| code_only(&s.text)).collect();
    let missing: Vec<&str> = anchors
        .iter()
        .copied()
        .filter(|anchor| !code.iter().any(|c| c.contains(anchor)))
        .collect();
    assert!(
        missing.is_empty(),
        "the scan never saw {missing:?}: the code it guards is not in the directories it reads ({})",
        read_dirs(sources)
    );
}

fn collect(dir: &Path, extensions: &[&str], into: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
    for entry in entries {
        let path = entry
            .unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()))
            .path();
        let metadata = std::fs::metadata(&path).unwrap_or_else(|e| panic!("cannot inspect {}: {e}", path.display()));
        if metadata.is_dir() {
            collect(&path, extensions, into);
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| extensions.contains(&e))
        {
            into.push(path);
        }
    }
}

fn read_at(root: &Path, path: &Path) -> Source {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let rel = path.strip_prefix(root).unwrap_or(path);
    Source {
        path: rel.to_string_lossy().replace('\\', "/"),
        text,
    }
}

fn read_dirs(sources: &[Source]) -> String {
    let mut dirs: Vec<&str> = sources
        .iter()
        .map(|s| s.path.rsplit_once('/').map_or(s.path.as_str(), |(dir, _)| dir))
        .collect();
    dirs.sort_unstable();
    dirs.dedup();
    dirs.join(", ")
}

pub use crate::rust_mask::code_only;
pub use crate::rust_mask::without_line_comments;

/// The byte positions just after every whole-word occurrence of `word`.
pub fn word_ends(code: &str, word: &str) -> Vec<usize> {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    code.match_indices(word)
        .filter(|(at, _)| !code[..*at].chars().next_back().is_some_and(ident))
        .map(|(at, w)| at + w.len())
        .filter(|end| !code[*end..].chars().next().is_some_and(ident))
        .collect()
}

/// The text from the `{` at `open` to its matching `}`, inclusive.
pub fn braced(code: &str, open: usize) -> &str {
    let mut depth = 0usize;
    for (at, c) in code[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &code[open..=open + at];
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces from byte {open}");
}

/// The body of every `fn NAME`.
pub fn functions<'a>(code: &'a str, name: &str) -> Vec<&'a str> {
    word_ends(code, "fn")
        .into_iter()
        .filter_map(|after_fn| {
            let rest = &code[after_fn..];
            let trimmed = rest.trim_start();
            if !trimmed.starts_with(name)
                || trimmed[name.len()..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_')
            {
                return None;
            }
            let start = after_fn + (rest.len() - trimmed.len());
            let open = start + code[start..].find('{')?;
            Some(braced(code, open))
        })
        .collect()
}

/// The bodies of inherent and trait `impl` blocks whose self type is named `type_name`.
/// Pass [`code_only`] text so comments and literals cannot contribute braces or names. Generic arguments,
/// bounds and trait names do not count as the self type; qualified self types use their final path segment.
pub fn impl_blocks<'a>(code: &'a str, type_name: &str) -> Vec<&'a str> {
    word_ends(code, "impl")
        .into_iter()
        .filter_map(|after| {
            let rest = &code[after..];
            let mut angles = 0usize;
            let mut groups = 0usize;
            let mut header_end = None;
            for (at, c) in rest.char_indices() {
                match c {
                    '<' => angles += 1,
                    '>' if angles > 0 && !rest[..at].ends_with('-') => angles -= 1,
                    '(' | '[' => groups += 1,
                    ')' | ']' if groups > 0 => groups -= 1,
                    '{' if angles == 0 && groups == 0 => {
                        header_end = Some(at);
                        break;
                    }
                    _ => {}
                }
            }
            let open = after + header_end?;
            let mut header = code[after..open].trim_start();
            if header.starts_with('<') {
                let mut depth = 0;
                let end = header.char_indices().find_map(|(at, c)| {
                    match c {
                        '<' => depth += 1,
                        '>' if !header[..at].ends_with('-') => depth -= 1,
                        _ => {}
                    }
                    (depth == 0).then_some(at + c.len_utf8())
                })?;
                header = header[end..].trim_start();
            }
            // Only top-level `for` and `where` delimit the self type: a bound may mention either word.
            let mut depth = 0usize;
            let mut self_start = 0;
            let mut self_end = header.len();
            let wheres = word_ends(header, "where");
            let fors = word_ends(header, "for");
            for (at, c) in header.char_indices() {
                match c {
                    '<' | '(' | '[' => depth += 1,
                    '>' if depth > 0 && !header[..at].ends_with('-') => depth -= 1,
                    ')' | ']' if depth > 0 => depth -= 1,
                    _ => {}
                }
                if depth == 0 && wheres.contains(&(at + 5)) {
                    self_end = at;
                    break;
                }
                if depth == 0 && fors.contains(&(at + 3)) {
                    self_start = at + 3;
                }
            }
            let self_type = header[self_start..self_end].trim();
            let path_end = self_type.find('<').unwrap_or(self_type.len());
            let name = self_type[..path_end].rsplit("::").next()?.trim();
            (name == type_name).then(|| braced(code, open))
        })
        .collect()
}

/// The body of every `mod NAME`.
pub fn modules<'a>(code: &'a str, name: &str) -> Vec<&'a str> {
    word_ends(code, "mod")
        .into_iter()
        .filter_map(|after| {
            let rest = &code[after..];
            let trimmed = rest.trim_start();
            let is_name = trimmed.starts_with(name)
                && !trimmed[name.len()..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_');
            if !is_name {
                return None;
            }
            let start = after + (rest.len() - trimmed.len()) + name.len();
            let next = code[start..].trim_start();
            next.starts_with('{')
                .then(|| braced(code, start + (code[start..].len() - next.len())))
        })
        .collect()
}

/// The paths of the files in `sources` that exist only under `cfg(test)`: a file its parent declares with
/// `#[cfg(test)] mod name;`, and every file such a file declares in turn. A scan that skips an inline `mod tests
/// { .. }` skips these too, since a module moved into a file of its own is no longer inside the text it cut.
/// A child is found as `dir/name.rs` or `dir/name/mod.rs` for `mod name;` in `dir.rs`, `dir/mod.rs`, `lib.rs`
/// or `main.rs`, or where `#[path = ".."]` points (relative to the declaring file's directory). Only a plain
/// `#[cfg(test)]` counts; `#[cfg(all(test, ..))]` and `#[cfg(any(test, ..))]` are read as product code.
///
/// # Panics
/// On a `#[cfg(test)] mod name;` inside an inline module, whose file the name alone does not give: a scan
/// that cannot tell which file is test-only must not guess. A declaration inside an inline module without
/// that attribute is not followed.
pub fn test_only_files(sources: &[Source]) -> std::collections::BTreeSet<String> {
    // The files `source` declares as `mod name;`: where each may be (first existing wins), and whether
    // `#[cfg(test)]` is on it.
    let declared = |source: &Source| -> Vec<(Vec<String>, bool)> {
        let code = code_only(&source.text);
        let (dir, own_dir) = match source.path.rsplit_once('/') {
            Some((dir, file)) if matches!(file, "mod.rs" | "lib.rs" | "main.rs") => (dir.to_owned(), dir.to_owned()),
            Some((dir, file)) => (format!("{dir}/{}", file.trim_end_matches(".rs")), dir.to_owned()),
            None => (source.path.trim_end_matches(".rs").to_owned(), String::new()),
        };
        let mut found = Vec::new();
        let (mut depth, mut seen) = (0_i32, 0);
        for after in word_ends(&code, "mod") {
            depth += code[seen..after].matches('{').count() as i32 - code[seen..after].matches('}').count() as i32;
            seen = after;
            let rest = code[after..].trim_start();
            let name_len = rest
                .find(|c: char| !c.is_alphanumeric() && c != '_')
                .unwrap_or(rest.len());
            let (name, tail) = rest.split_at(name_len);
            if name.is_empty() || !tail.trim_start().starts_with(';') {
                continue;
            }
            let head = &code[..after - 3];
            let from = head.rfind([';', '{', '}']).map_or(0, |at| at + 1);
            let squeezed: String = head[from..].chars().filter(|c| !c.is_whitespace()).collect();
            let gated = squeezed.contains("#[cfg(test)]");
            if depth > 0 {
                assert!(
                    !gated,
                    "{}: `#[cfg(test)] mod {name};` inside an inline module: test_only_files does not follow it",
                    source.path
                );
                continue;
            }
            // The attribute's path is a literal, blanked in `code`; the text has it at the same offsets.
            let attributes = &source.text[from..after - 3];
            let path_attr = attributes.find("#[path").and_then(|at| {
                let quoted = &attributes[at..];
                let open = quoted.find('"')? + 1;
                Some(&quoted[open..open + quoted[open..].find('"')?])
            });
            let candidates = match (squeezed.contains("#[path="), path_attr) {
                (true, Some(path)) => vec![normalized(&format!("{own_dir}/{path}"))],
                (true, None) => continue,
                _ => vec![format!("{dir}/{name}.rs"), format!("{dir}/{name}/mod.rs")],
            };
            found.push((candidates, gated));
        }
        found
    };
    let by_path: std::collections::BTreeMap<&str, Vec<(Vec<String>, bool)>> = sources
        .iter()
        .map(|source| (source.path.as_str(), declared(source)))
        .collect();
    let resolve = |candidates: &[String]| {
        candidates
            .iter()
            .find(|path| by_path.contains_key(path.as_str()))
            .cloned()
    };
    let mut only = std::collections::BTreeSet::new();
    let mut work = Vec::new();
    for (candidates, gated) in by_path.values().flatten() {
        if let Some(path) = resolve(candidates).filter(|_| *gated) {
            if only.insert(path.clone()) {
                work.push(path);
            }
        }
    }
    // What a test-only file declares is test-only whatever its own attributes say.
    while let Some(path) = work.pop() {
        for (candidates, _) in &by_path[path.as_str()] {
            if let Some(child) = resolve(candidates) {
                if only.insert(child.clone()) {
                    work.push(child);
                }
            }
        }
    }
    only
}

/// `path` without its `.` and `x/..` segments.
fn normalized(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            part => out.push(part),
        }
    }
    out.join("/")
}

/// `code` with every one of `bodies` (slices of `code`) blanked.
pub fn without(code: &str, bodies: &[&str]) -> String {
    let mut out = code.to_owned();
    for body in bodies {
        let start = body.as_ptr() as usize - code.as_ptr() as usize;
        out.replace_range(start..start + body.len(), &" ".repeat(body.len()));
    }
    out
}

/// Every call of `name` in `code`: the word followed (after blanks) by `(`, and not a definition
/// (`fn name`) or a method of something else (`.name(`).
pub fn calls(code: &str, name: &str) -> usize {
    word_ends(code, name)
        .into_iter()
        .filter(|end| code[*end..].trim_start().starts_with('('))
        .filter(|end| {
            let before = code[..end - name.len()].trim_end();
            !before.ends_with('.') && !before.ends_with("fn")
        })
        .count()
}

/// Spellings of one dependency on `gtk4` a manifest reader must each report once: a comment after a header or an
/// entry, single quotes, a rename in a sub-table, after other keys (one of them holding the word `package`) or in a
/// dotted key, and a quoted key. Every manifest guard runs them through its own list.
pub const AWKWARD_DEPENDENCY_SPELLINGS: &[&str] = &[
    "[dependencies] # desktop tests\ngtk4 = '0.11'\n",
    "[dependencies]\ngtk4 = '0.11' # toolkit\n",
    "[dependencies.ui] # renamed\npackage = 'gtk4'\n",
    "[dependencies.ui]\npackage = \"gtk4\" # renamed\n",
    "[dependencies]\nui = { package = 'gtk4' } # renamed\n",
    "[dependencies]\nui = { features = [\"package\"], package = \"gtk4\" }\n",
    "[dependencies]\nui = { git = \"https://example.org/package\", package = \"gtk4\" }\n",
    "[dependencies]\nui = { version = \"1\", package = 'gtk4' }\n",
    "[dependencies]\nui.package = \"gtk4\"\n",
    "[dependencies]\n\"gtk4\" = \"0.11\"\n",
];

/// A manifest whose names hold a gtk-rs family name inside another word, and a rename to one: none of them is that
/// dependency.
pub const NAMES_INSIDE_OTHER_WORDS: &str = "[dependencies]\nglibc-version = '1'\ngtkish = '1'\ngiobuf = '1'\n\
                                                gdkx = '1'\nsupergtk4 = '1'\n[dependencies.ui]\npackage = 'glibc'\n";

/// Whether `name` is `family` or a crate of it (`family-...`: `gio` holds `gio`, `gio-sys`, never `giobuf`).
pub fn is_package_in_family(name: &str, families: &[&str]) -> bool {
    families
        .iter()
        .any(|family| name == *family || name.strip_prefix(family).is_some_and(|rest| rest.starts_with('-')))
}

/// The byte ranges of the argument lists of every `socket_path::in_dir(...)` call in `code`.
fn in_dir_arguments(code: &str) -> Vec<std::ops::Range<usize>> {
    let mut arguments = Vec::new();
    for end in word_ends(code, "socket_path") {
        let call = code[end..]
            .trim_start()
            .strip_prefix("::")
            .and_then(|tail| tail.trim_start().strip_prefix("in_dir"))
            .and_then(|tail| tail.trim_start().strip_prefix('('));
        let Some(body) = call else {
            continue;
        };
        let start = code.len() - body.len();
        let mut depth = 1;
        for (offset, byte) in body.bytes().enumerate() {
            match byte {
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => continue,
            }
            if depth == 0 {
                arguments.push(start..start + offset);
                break;
            }
        }
    }
    arguments
}

/// The offsets of every socket name (`.sock` that does not go on as an identifier) in the code of `text` that sits
/// outside the argument list of a `socket_path::in_dir(...)` call: a path built any other way escapes the length cap
/// `in_dir` owns. A line comment names nothing (a block comment is read as code, so a name in one is reported); a call
/// is read from its parentheses, so a name on the same line as a call but outside its arguments is found, and a name
/// below a call that has closed is too.
pub fn unchecked_socket_offsets(text: &str) -> Vec<usize> {
    let text = without_line_comments(text);
    let code = code_only(&text);
    let arguments = in_dir_arguments(&code);
    text.match_indices(".sock")
        .filter(|(offset, suffix)| {
            !text[offset + suffix.len()..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
                && !arguments.iter().any(|range| range.contains(offset))
        })
        .map(|(offset, _)| offset)
        .collect()
}

/// The socket names of `sources` that [`unchecked_socket_offsets`] finds and no `never_bound` entry excuses, as
/// `path:line: text`. An entry is `(file, a substring of the line)`: a `.sock` mention that is never bound or
/// connected as a path, such as a file name handed to a function or a fixture that is only compared.
pub fn unrouted_socket_sites(sources: &[Source], never_bound: &[(&str, &str)]) -> Vec<String> {
    let mut sites = Vec::new();
    for source in sources {
        for offset in unchecked_socket_offsets(&source.text) {
            let start = source.text[..offset].rfind('\n').map_or(0, |at| at + 1);
            let code = source.text[start..].lines().next().unwrap_or_default().trim_start();
            if never_bound
                .iter()
                .any(|(file, needle)| *file == source.path && code.contains(needle))
            {
                continue;
            }
            let line = source.text[..start].bytes().filter(|byte| *byte == b'\n').count() + 1;
            sites.push(format!("{}:{line}: {code}", source.path));
        }
    }
    sites
}

/// The `never_bound` entries that no file of `sources` still contains, as `file: needle`: an allowlist entry whose
/// text is gone would let a construction site hide behind it.
pub fn stale_never_bound(sources: &[Source], never_bound: &[(&str, &str)]) -> Vec<String> {
    never_bound
        .iter()
        .filter(|(file, needle)| !sources.iter().any(|s| s.path == *file && s.text.contains(needle)))
        .map(|(file, needle)| format!("{file}: {needle:?}"))
        .collect()
}

#[cfg(test)]
mod walker_tests {
    use super::*;

    #[test]
    fn test_only_files_follow_cfg_test_declarations_down_the_tree() {
        let file = |path: &str, text: &str| Source {
            path: path.into(),
            text: text.into(),
        };
        let sources = [
            file(
                "core/src/lib.rs",
                "mod set;\n#[cfg(test)]\nmod doubles;\n// #[cfg(test)] mod ghost;\nconst T: &str = \"#[cfg(test)] mod text;\";",
            ),
            file(
                "core/src/set.rs",
                "mod moving;\n#[cfg(test)] mod tests;\n#[cfg(test)]\n#[allow(dead_code)]\npub(crate) mod frozen;\nmod next { }",
            ),
            file("core/src/set/moving.rs", "fn shipped() {}"),
            file("core/src/set/tests.rs", "mod nested;\nfn a() {}"),
            file("core/src/set/tests/nested.rs", "fn b() {}"),
            file("core/src/set/frozen/mod.rs", "mod leaf;"),
            file("core/src/set/frozen/leaf.rs", "fn c() {}"),
            file("core/src/doubles.rs", ""),
            file("core/src/ghost.rs", ""),
            file("core/src/text.rs", ""),
        ];
        let test_only = |sources: &[Source]| test_only_files(sources).into_iter().collect::<Vec<_>>();
        assert_eq!(
            test_only(&sources),
            [
                "core/src/doubles.rs",
                "core/src/set/frozen/leaf.rs",
                "core/src/set/frozen/mod.rs",
                "core/src/set/tests.rs",
                "core/src/set/tests/nested.rs",
            ]
        );

        // A `#[path]` points where it says; the production file of the same name stays product code.
        let with_path = [
            file(
                "core/src/lib.rs",
                "#[cfg(not(test))] mod live;\n#[cfg(test)]\n#[path = \"../tests/support/live.rs\"]\nmod live;\n#[path = \"other.rs\"] mod shipped;",
            ),
            file("core/src/live.rs", ""),
            file("core/src/other.rs", ""),
            file("core/tests/support/live.rs", ""),
        ];
        assert_eq!(test_only(&with_path), ["core/tests/support/live.rs"]);

        // Inside an inline module the name alone does not say which file: an ungated declaration is not
        // followed, a gated one is refused rather than guessed.
        let nested = |text: &str| {
            [
                file("core/src/lib.rs", text),
                file("core/src/live.rs", ""),
                file("core/src/inner/live.rs", ""),
            ]
        };
        assert!(test_only(&nested("mod live;\nmod inner { mod live; }")).is_empty());
        let refused =
            std::panic::catch_unwind(|| test_only_files(&nested("mod live;\nmod inner { #[cfg(test)] mod live; }")));
        assert!(refused.is_err());
    }

    #[test]
    fn impl_blocks_finds_only_the_named_self_type_across_headers_and_bodies() {
        let code = code_only(
            r#"
            impl Driver { fn first() { if true { nested(); } } }
            impl<T: Decide> Driver<T> where T: Other { fn second() {} }
            impl<T> HostSink for crate::wm::Driver<T> { fn third() {} }
            impl Controller { fn fourth() {} }
            impl DriverExtra { fn wrong() {} }
            impl Driver for Other { fn wrong() {} }
            impl Wrapper<Driver> { fn wrong() {} }
            impl<T: Trait<Driver>> Other<T> { fn wrong() {} }
            impl<T: Fn() -> Driver> Other<T> { fn wrong() {} }
            // impl Driver { fn wrong() {} }
            const TEXT: &str = "impl Driver { fn wrong() {} }";
            "#,
        );
        let blocks = impl_blocks(&code, "Driver");
        assert_eq!(blocks.len(), 3);
        for (body, name) in blocks.iter().zip(["first", "second", "third"]) {
            assert_eq!(functions(body, name).len(), 1);
            assert!(functions(body, "wrong").is_empty());
        }
        assert_eq!(impl_blocks(&code, "Controller").len(), 1);
        assert!(impl_blocks(&code, "Missing").is_empty());
    }

    #[test]
    fn impl_blocks_uses_braces_instead_of_line_boundaries() {
        for code in [
            "impl Driver{fn kept(){}}impl Driver{fn gone(){}}",
            "impl\nDriver\n{\nfn\nkept()\n{}\n}\nimpl\nDriver\n{fn gone(){}}",
            "impl<T> SomeTrait<T> for Driver<T> where T: Bound {fn kept(){}} impl Driver{fn gone(){}}",
            "impl<const N: usize> Driver<{ N + 1 }> {fn kept(){}} impl Driver{fn gone(){}}",
            "impl<T: Fn() -> ()> Driver<T> {fn kept(){}} impl Driver{fn gone(){}}",
        ] {
            let code = code_only(code);
            let blocks = impl_blocks(&code, "Driver");
            assert_eq!(blocks.len(), 2, "{code}");
            assert_eq!(functions(blocks[0], "kept").len(), 1);
            assert_eq!(functions(blocks[1], "gone").len(), 1);
        }
    }

    #[test]
    fn function_in_finds_one_function_across_files_and_refuses_two() {
        let mut sources = vec![
            Source {
                path: "a.rs".into(),
                text: "// é\nfn other() {}\n#[cfg(test)] mod tests { fn wanted() {} }".into(),
            },
            Source {
                path: "b.rs".into(),
                text: "// é\nfn wanted() { let text = \"héllo\"; }".into(),
            },
        ];
        let (file, body) = function_in(&sources, "wanted");
        assert_eq!(file.path, "b.rs");
        assert!(body.contains("let text ="));
        assert!(!body.contains("héllo"));
        let (raw_file, raw) = function_in_raw(&sources, "wanted");
        assert_eq!(raw_file.path, file.path);
        assert_eq!(raw, "{ let text = \"héllo\"; }");
        assert_eq!(body.len(), raw.len());
        assert!(std::panic::catch_unwind(|| function_in(&sources, "missing")).is_err());
        sources[0].text.push_str("\nfn wanted() {}");
        let failure = std::panic::catch_unwind(|| function_in(&sources, "wanted")).unwrap_err();
        let message = failure.downcast_ref::<String>().unwrap();
        assert!(message.contains("a.rs") && message.contains("b.rs"));
        sources[0].text.push_str("\nfn other() {}");
        assert!(std::panic::catch_unwind(|| function_in(&sources, "other")).is_err());
    }

    #[test]
    #[should_panic(expected = "cannot read")]
    fn a_missing_directory_fails() {
        rust_sources(&["core/no-such-directory"]);
    }

    #[test]
    #[should_panic(expected = "holds no")]
    fn a_directory_without_such_files_fails() {
        rust_sources(&["docs/keymap"]);
    }

    #[test]
    #[should_panic(expected = "never saw")]
    fn a_missing_anchor_fails() {
        let read = rust_sources(&["core/src/theme"]);
        require_anchors(&read, &["fn no_such_function_anywhere("]);
    }

    #[test]
    #[should_panic(expected = "no scanned file contains")]
    fn file_with_fails_on_no_file() {
        let read = rust_sources(&["core/src/theme"]);
        file_with(&read, "fn no_such_function_anywhere(");
    }

    #[test]
    #[should_panic(expected = "more than one file")]
    fn file_with_fails_on_two_files() {
        let read = rust_sources(&["core/src"]);
        file_with(&read, "use ");
    }

    #[test]
    #[should_panic(expected = "cannot read")]
    fn a_missing_required_file_fails() {
        read_required("core/src/no_such_file.rs");
    }

    #[test]
    fn the_paths_are_relative_to_the_workspace_root() {
        let read = rust_sources(&["core/src"]);
        assert!(
            read.iter().any(|s| s.path == "core/src/lib.rs"),
            "{:?}",
            read.first().map(|s| &s.path)
        );
        let lib = file_with(&read, "pub mod source_scan");
        assert_eq!(lib.path, "core/src/lib.rs");
        require_anchors(&read, &["pub mod source_scan"]);
        assert!(read_required("Cargo.toml").text.contains("[workspace]"));
    }

    #[test]
    fn an_anchor_inside_a_comment_or_a_string_does_not_count() {
        let read = vec![Source {
            path: "x.rs".into(),
            text: "// fn hidden(\nlet s = \"fn hidden(\";".into(),
        }];
        let result = std::panic::catch_unwind(|| require_anchors(&read, &["fn hidden("]));
        assert!(result.is_err());
    }

    #[test]
    fn every_product_crate_is_on_the_product_list() {
        let manifest = read_required("Cargo.toml").text;
        let missing = product_crates_missing_from(&manifest, PRODUCT_SOURCES);
        assert!(missing.is_empty(), "add to PRODUCT_SOURCES: {missing:?}");
        let members = workspace_members(&manifest);
        for dir in PRODUCT_SOURCES {
            let krate = dir.strip_suffix("/src").unwrap();
            assert!(
                members.iter().any(|m| m == krate),
                "{dir} is on the list but {krate} is not a member"
            );
        }
        assert!(
            members.iter().any(|m| m.starts_with("poc/")) && !PRODUCT_SOURCES.iter().any(|d| d.starts_with("poc/")),
            "poc/ stays off the list"
        );
    }

    #[test]
    fn an_unlisted_product_crate_is_reported_and_poc_is_not() {
        let manifest = "[workspace]\nmembers = [\n    \"poc/spike\",\n    \"shell\",\n    \"desk\",  # new\n    \"terminal/engine\",\n]\n";
        assert_eq!(
            workspace_members(manifest),
            ["poc/spike", "shell", "desk", "terminal/engine"]
        );
        let list = ["shell/src", "terminal/engine/src"];
        assert_eq!(product_crates_missing_from(manifest, &list), ["desk/src"]);
        assert!(product_crates_missing_from(manifest, &["desk/src", "shell/src", "terminal/engine/src"]).is_empty());
    }

    #[test]
    fn adjacent_workspace_members_are_checked_individually() {
        let manifest = "[workspace] # crates\nmembers = [\"poc/spike\", 'new-product',] # members\n";
        assert_eq!(product_crates_missing_from(manifest, &[]), ["new-product/src"]);
    }

    #[test]
    fn product_dirs_except_drops_named_entries_and_refuses_unknown_ones() {
        assert_eq!(product_dirs().len(), PRODUCT_SOURCES.len());
        let kept = product_dirs_except(&["mac/src", "remote/src"]);
        assert_eq!(kept.len(), PRODUCT_SOURCES.len() - 2);
        assert!(!kept.contains(&"mac/src") && kept.contains(&"shell/src"));
        assert!(std::panic::catch_unwind(|| product_dirs_except(&["no/such/src"])).is_err());
    }

    #[test]
    fn host_scans_follow_new_product_directories() {
        let excluded = ["core/src", "mac/src"];
        let manifest = read_required("Cargo.toml").text;
        let mut expected: std::collections::BTreeSet<_> = workspace_members(&manifest)
            .into_iter()
            .map(|member| format!("{member}/src"))
            .collect();
        expected.retain(|dir| PRODUCT_SOURCES.contains(&dir.as_str()));
        for dir in NON_HOST_SOURCES.iter().chain(&excluded) {
            expected.remove(*dir);
        }
        let mut actual = host_dirs_except(&excluded);
        actual.sort_unstable();
        assert_eq!(actual, expected.into_iter().collect::<Vec<_>>());
        let mut product = product_dirs();
        product.push("new-window/src");
        assert!(host_dirs_from(&product, &["core/src"]).contains(&"new-window/src"));
        assert!(std::panic::catch_unwind(|| host_dirs_except(&["agent/src"])).is_err());
    }

    #[test]
    fn the_product_list_reads_every_listed_directory() {
        let read = product_sources();
        for dir in PRODUCT_SOURCES {
            assert!(
                read.iter().any(|s| s.path.starts_with(&format!("{dir}/"))),
                "{dir} contributed no file"
            );
        }
    }

    #[test]
    #[should_panic(expected = "holds no")]
    fn an_empty_product_directory_fails() {
        // `docs/keymap` exists and holds no `.rs` file: a listed directory that emptied out fails the scan.
        rust_sources(&["core/src/theme", "docs/keymap"]);
    }
}

#[cfg(test)]
mod socket_scan_tests {
    use super::*;

    fn found(text: &str) -> usize {
        unchecked_socket_offsets(text).len()
    }

    fn source(path: &str, text: &str) -> Source {
        Source {
            path: path.into(),
            text: text.into(),
        }
    }

    /// The matcher sees a socket name and does not see a bare identifier, and sees the call that routes one.
    #[test]
    fn the_matcher_reads_what_it_should() {
        assert_eq!(found("let p = dir.join(\"x.sock\");"), 1);
        assert_eq!(found("let p = self.socket_path;"), 0);
        assert_eq!(
            found("let p = agent::socket_path::in_dir(\n &dir,\n \"x.sock\",\n);"),
            0
        );
        assert_eq!(found("let a = 1;\nlet p = dir.join(\"x.sock\");"), 1);
        assert_eq!(found("// x.sock"), 0);
    }

    #[test]
    fn trailing_comments_do_not_hide_or_add_socket_paths() {
        let checked = r#"let p = socket_path::in_dir(dir, "ok.sock")?; // previously old.sock"#;
        assert_eq!(found(checked), 0);
        let hand = "let p = dir.join(name); // was dir.join(\"old.sock\")";
        assert_eq!(found(hand), 0);
        for line in [
            r#"let p = dir.join("é\"//real.sock"); // old.sock"#,
            r##"let p = dir.join(r#"prefix
//real.sock"#); // old.sock"##,
        ] {
            assert_eq!(unchecked_socket_offsets(line), [line.find(".sock").unwrap()]);
        }
    }

    #[test]
    fn a_checked_call_does_not_exempt_a_fallback_on_the_same_line() {
        let text =
            r#"let p = socket_path::in_dir(make_dir(), "a).sock").unwrap_or_else(|_| dir.join("fallback.sock"));"#;
        assert_eq!(unchecked_socket_offsets(text), [text.find(".sock\"));").unwrap()]);
        let split = "socket_path ::\n in_dir\n(dir, \"a.sock\")\n.or_else(|_| dir.join(\"fallback.sock\"))";
        assert_eq!(unchecked_socket_offsets(split), [split.rfind(".sock").unwrap()]);
        let both = "let both = (socket_path::in_dir(&d, \"a.sock\"), d.join(\"b.sock\"));";
        assert_eq!(unchecked_socket_offsets(both), [both.rfind(".sock").unwrap()]);
    }

    /// Only the argument list of a call routes a name: a call that has closed, however few lines above, and a
    /// comment that names `in_dir` do not.
    #[test]
    fn a_name_is_routed_by_the_call_it_sits_in_and_nothing_nearby() {
        let wrapped = "let p = agent::socket_path::in_dir(\n    &dir,\n    &format!(\"a-{i}.sock\"),\n)?;";
        assert_eq!(found(wrapped), 0);
        let nested = "let p = socket_path::in_dir(&dir, &join(&[a, (b)], \"x.sock\"))?;";
        assert_eq!(found(nested), 0);
        let closed = "let s = S {\n    a: socket_path::in_dir(&d, \"a.sock\")?,\n    b: 1,\n    c: 2,\n    d: d.join(\"b.sock\"),\n};";
        assert_eq!(unchecked_socket_offsets(closed), [closed.rfind(".sock").unwrap()]);
        let commented = "// built through socket_path::in_dir(\nlet p = dir.join(\"x.sock\")";
        assert_eq!(found(commented), 1);
        let imported = "use agent::socket_path::in_dir;\nlet p = in_dir(&d, \"x.sock\");";
        assert_eq!(found(imported), 1);
        let other_module = "let p = my_socket_path::in_dir(&d, \"x.sock\");";
        assert_eq!(found(other_module), 1);
    }

    #[test]
    fn a_dotted_word_that_only_starts_with_sock_is_not_a_socket_name() {
        assert_eq!(found("let a = self.socket_path(); let b = x.sockets;"), 0);
        assert_eq!(found("let a = \"x.sock\"; let b = \"y.sock_x\";"), 1);
    }

    #[test]
    fn the_allowlist_excuses_a_line_of_its_own_file_only() {
        let read = [
            source("a/src/x.rs", "let n = \"w.sock\";\nlet p = d.join(\"b.sock\");\n"),
            source("a/src/y.rs", "let n = \"w.sock\";\n"),
        ];
        let never = [("a/src/x.rs", "let n = \"w.sock\"")];
        assert_eq!(
            unrouted_socket_sites(&read, &never),
            [
                "a/src/x.rs:2: let p = d.join(\"b.sock\");",
                "a/src/y.rs:1: let n = \"w.sock\";"
            ]
        );
        assert!(stale_never_bound(&read, &never).is_empty());
        let gone = [("a/src/x.rs", "let n = \"v.sock\""), ("a/src/z.rs", "let n")];
        assert_eq!(
            stale_never_bound(&read, &gone),
            ["a/src/x.rs: \"let n = \\\"v.sock\\\"\"", "a/src/z.rs: \"let n\""]
        );
    }

    #[test]
    fn the_awkward_spellings_are_each_one_dependency() {
        for manifest in AWKWARD_DEPENDENCY_SPELLINGS {
            assert_eq!(
                manifest_dependency_offenders(manifest, |name| name == "gtk4").len(),
                1,
                "{manifest}"
            );
        }
        let inside = manifest_dependency_offenders(NAMES_INSIDE_OTHER_WORDS, |name| {
            is_package_in_family(name, &["gtk4", "gtk", "glib", "gio", "gdk"])
        });
        assert_eq!(inside, Vec::<String>::new());
    }

    #[test]
    fn a_family_holds_its_own_crates_and_no_longer_word() {
        let families = ["gio", "gtk4"];
        for name in ["gio", "gio-sys", "gtk4-macros"] {
            assert!(is_package_in_family(name, &families), "{name}");
        }
        for name in ["giobuf", "glibc-version", "supergtk4", "gtk", "shellexpand"] {
            assert!(!is_package_in_family(name, &families), "{name}");
        }
    }
}
