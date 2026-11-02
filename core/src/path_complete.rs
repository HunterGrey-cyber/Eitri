//! Project-relative names for the composer's path picker. No file contents are opened.
use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Stdio};

fn usable(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('/')
        && !name.split('/').any(|part| part == ".git" || part == "..")
        && !name.chars().any(|c| c.is_control() || c == '"')
}

fn add(paths: &mut BTreeSet<String>, name: &str) {
    if !usable(name) {
        return;
    }
    paths.insert(name.to_owned());
    for (at, _) in name.match_indices('/') {
        paths.insert(name[..=at].to_owned());
    }
}

/// Lists names using Git's ignore rules, falling back to a bounded walk without following symlinks.
/// This may take seconds and belongs on a worker thread.
pub fn list_paths(root: &Path) -> Vec<String> {
    let mut paths = BTreeSet::new();
    if let Ok(output) = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-co", "--exclude-standard", "-z"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    {
        if output.status.success() {
            for bytes in output.stdout.split(|b| *b == 0) {
                if let Ok(name) = std::str::from_utf8(bytes) {
                    add(&mut paths, name);
                }
            }
            return paths.into_iter().collect();
        }
    }
    walk(root, &mut paths, 50_000, 32);
    paths.into_iter().collect()
}

fn walk(root: &Path, paths: &mut BTreeSet<String>, max_entries: usize, max_depth: usize) {
    let mut pending = vec![(root.to_path_buf(), String::new(), 0)];
    let mut entries = 0;
    while let Some((dir, prefix, depth)) = pending.pop() {
        let Ok(read) = std::fs::read_dir(dir) else { continue };
        for entry in read.flatten() {
            entries += 1;
            if entries > max_entries {
                return;
            }
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if !usable(&name) {
                continue;
            }
            let Ok(kind) = entry.file_type() else { continue };
            let relative = format!("{prefix}{name}");
            if kind.is_dir() {
                add(paths, &format!("{relative}/"));
                if depth + 1 < max_depth {
                    pending.push((entry.path(), format!("{relative}/"), depth + 1));
                }
            } else {
                add(paths, &relative);
            }
        }
    }
}

/// Matches names case-insensitively, with stronger matches before shorter and alphabetical names.
pub fn rank(paths: &[String], query: &str, limit: usize) -> Vec<String> {
    if query.starts_with(['/', '~']) || query.starts_with("..") || query.contains("/../") || query.ends_with("/..") {
        return Vec::new();
    }
    if query.is_empty() {
        let mut top: Vec<_> = paths
            .iter()
            .filter(|p| !p.trim_end_matches('/').contains('/'))
            .cloned()
            .collect();
        top.sort();
        top.dedup();
        top.truncate(limit);
        return top;
    }
    let query = query.to_lowercase();
    let mut matches = Vec::new();
    for path in paths {
        let lower = path.to_lowercase();
        let basename = lower.trim_end_matches('/').rsplit('/').next().unwrap_or("");
        let class = if !query.contains('/') && basename.starts_with(&query) {
            0
        } else if lower.starts_with(&query) {
            1
        } else if !query.contains('/') && basename.contains(&query) {
            2
        } else if lower.contains(&query) {
            3
        } else {
            let mut letters = query.chars();
            let mut next = letters.next();
            for c in lower.chars() {
                if next == Some(c) {
                    next = letters.next();
                }
            }
            if next.is_some() {
                continue;
            }
            4
        };
        matches.push((class, path.chars().count(), lower, path));
    }
    matches.sort();
    matches.into_iter().take(limit).map(|(_, _, _, p)| p.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("path-complete-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn paths(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }
    #[test]
    fn ranking_and_relative_queries() {
        let names = paths(&[
            "blob.bin",
            "long.txt",
            "sub/",
            "sub/deep/",
            "sub/inner.txt",
            "sub/deep/leaf.txt",
            ".env",
        ]);
        assert_eq!(rank(&names, "lo", 50), paths(&["long.txt", "blob.bin"]));
        assert_eq!(rank(&names, "LONG", 50), paths(&["long.txt"]));
        assert_eq!(
            rank(&names, "sub/", 50),
            paths(&["sub/", "sub/deep/", "sub/inner.txt", "sub/deep/leaf.txt"])
        );
        assert_eq!(rank(&names, "", 50), paths(&[".env", "blob.bin", "long.txt", "sub/"]));
        assert!(rank(&names, "zzz", 50).is_empty());
        assert_eq!(rank(&names, "lo", 1), paths(&["long.txt"]));
        for query in ["/abs", "~", "../", "sub/../x", "sub/.."] {
            assert!(rank(&names, query, 50).is_empty());
        }
        let names = paths(&["a/zlo", "alox", "lo/z", "abc/lob", "l_o", "xlo"]);
        assert_eq!(
            rank(&names, "lo", 50),
            paths(&["abc/lob", "lo/z", "xlo", "alox", "a/zlo", "l_o"])
        );
    }
    #[test]
    fn git_ignores_and_keeps_tracked_names_without_statting() {
        let dir = TempDir::new();
        let root = dir.path();
        assert!(Command::new("git")
            .args(["init", "-q"])
            .arg(root)
            .status()
            .unwrap()
            .success());
        std::fs::write(root.join(".gitignore"), "ignored\n").unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/file"), "").unwrap();
        std::fs::write(root.join("ignored"), "").unwrap();
        std::fs::write(root.join(".hidden"), "").unwrap();
        assert!(Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["add", "sub/file"])
            .status()
            .unwrap()
            .success());
        std::fs::remove_file(root.join("sub/file")).unwrap();
        assert_eq!(list_paths(root), paths(&[".gitignore", ".hidden", "sub/", "sub/file"]));
    }
    #[test]
    fn walk_includes_dotfiles_and_ignored_names_without_following_links() {
        use std::os::unix::{ffi::OsStrExt, fs::symlink};
        let dir = TempDir::new();
        let root = dir.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/hidden"), "").unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        for name in [
            ".hidden",
            ".gitignore",
            "ignored",
            "with space.txt",
            "bad\nname",
            "bad\"name",
        ] {
            std::fs::write(root.join(name), "").unwrap();
        }
        std::fs::write(root.join(std::ffi::OsStr::from_bytes(b"bad\xff")), "").unwrap();
        let outside = TempDir::new();
        std::fs::write(outside.path().join("secret"), "").unwrap();
        symlink(outside.path(), root.join("link")).unwrap();
        assert_eq!(
            list_paths(root),
            paths(&[".gitignore", ".hidden", "ignored", "link", "sub/", "with space.txt"])
        );
    }
    #[test]
    fn walk_stops_at_depth_32() {
        let dir = TempDir::new();
        let mut path = dir.path().to_path_buf();
        for _ in 0..34 {
            path.push("d");
            std::fs::create_dir(&path).unwrap();
        }
        let names = list_paths(dir.path());
        assert_eq!(names.len(), 32);
        assert_eq!(names.last().unwrap().matches('/').count(), 32);
    }
    #[test]
    fn walk_caps_the_entries_it_visits() {
        let dir = TempDir::new();
        for name in ["a", "b", "c", "d", "e"] {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        let mut paths = BTreeSet::new();
        walk(dir.path(), &mut paths, 3, 32);
        assert_eq!(paths.len(), 3);
    }
    #[test]
    fn names_are_filtered_sorted_and_deduplicated_including_every_parent() {
        let mut names = BTreeSet::new();
        for name in [
            "z/deep/file",
            "z/deep/other",
            "z/deep/file",
            ".env",
            "space name",
            "bad\nname",
            "bad\"name",
            ".git/HEAD",
            "dir/.git/config",
        ] {
            add(&mut names, name);
        }
        assert_eq!(
            names.into_iter().collect::<Vec<_>>(),
            paths(&[".env", "space name", "z/", "z/deep/", "z/deep/file", "z/deep/other"])
        );
    }
    #[test]
    fn bridge_message_is_routed_and_its_reply_is_bounded() {
        use crate::agent_bridge::{serialize_path_completions_for_js, InboundMessage, TabRef};
        let message: InboundMessage =
            serde_json::from_str(r#"{"type":"path_complete","request_id":"p","tab":7,"query":"sub/","seq":3}"#)
                .unwrap();
        assert_eq!(message.tab_ref(), TabRef::Named(crate::tabs::TabId(7)));
        assert_eq!(message.request_id(), "p");
        let items: Vec<_> = (0..100).map(|n| format!("path{n}")).collect();
        let payload: serde_json::Value =
            serde_json::from_str(&serialize_path_completions_for_js(crate::tabs::TabId(7), 3, &items)).unwrap();
        assert_eq!(payload["kind"], "path_completions");
        assert_eq!(payload["tab"], 7);
        assert_eq!(payload["seq"], 3);
        assert_eq!(payload["items"].as_array().unwrap().len(), 50);
    }
}
