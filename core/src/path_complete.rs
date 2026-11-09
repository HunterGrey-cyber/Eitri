//! Project-relative names for path completion and suffix lookup. No file contents are opened.
use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

/// How long git may take to list a project. Past it the walk's bounded answer is better than none.
const GIT_LIMIT: Duration = Duration::from_secs(10);

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
    // The person's own git, for their global ignores, with the one setting a repository could name a program
    // through for this command turned off: a folder someone sent must not run anything when `@` is typed.
    let mut git = crate::turn_review::git::user_read_only(root);
    git.args([
        "-c",
        "core.fsmonitor=false",
        "ls-files",
        "-co",
        "--exclude-standard",
        "-z",
    ]);
    if let Ok(output) = crate::turn_review::git::run(git, GIT_LIMIT) {
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

/// Bounds a suffix listing on its worker thread.
#[derive(Clone, Copy)]
pub struct SuffixLimits {
    pub max_files: usize,
    pub deadline: Duration,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Stopped {
    Files(usize),
    Time,
}

#[derive(Debug, Default)]
pub struct SuffixLookup {
    pub matches: Vec<String>,
    pub stopped: Option<Stopped>,
}

/// Only relative, control-free names without parent traversal may start a suffix search.
pub fn suffix_wanted(wanted: &str) -> Option<&str> {
    let wanted = wanted.strip_prefix("./").unwrap_or(wanted);
    (!wanted.is_empty()
        && !Path::new(wanted).is_absolute()
        && !wanted.split('/').any(|part| part == "..")
        && !crate::agent_bridge::path_holds_control(wanted))
    .then_some(wanted)
}

/// Finds every file with this exact name or path suffix, using Git's ignores when available.
/// Disk access and child-process waiting belong on a worker thread.
pub fn find_by_suffix(root: &Path, wanted: &str, limits: SuffixLimits) -> SuffixLookup {
    let mut result = SuffixLookup::default();
    let Some(wanted) = suffix_wanted(wanted) else {
        return result;
    };
    let started = std::time::Instant::now();
    let mut listed = 0;
    if started.elapsed() >= limits.deadline {
        result.stopped = Some(Stopped::Time);
        return result;
    }
    if limits.max_files == 0 {
        result.stopped = Some(Stopped::Files(0));
        return result;
    }
    let mut git = crate::turn_review::git::user_read_only(root);
    git.args([
        "-c",
        "core.fsmonitor=false",
        "ls-files",
        "--cached",
        "--others",
        "--exclude-standard",
        "-z",
    ]);
    let listing = crate::turn_review::git::run(git, limits.deadline.saturating_sub(started.elapsed()));
    // Keep the time budget shared by Git and the fallback walk.
    if started.elapsed() >= limits.deadline {
        result.stopped = Some(Stopped::Time);
        return result;
    }
    let consider = |name: &str, result: &mut SuffixLookup| {
        if name == wanted || name.strip_suffix(wanted).is_some_and(|prefix| prefix.ends_with('/')) {
            if suffix_wanted(name).is_some() && root.join(name).is_file() {
                result.matches.push(name.to_owned());
            }
        }
    };
    if let Some(output) = listing
        .as_ref()
        .ok()
        .filter(|output| output.status.success() && output.stdout.iter().any(|byte| *byte != 0))
    {
        for bytes in output.stdout.split(|b| *b == 0).filter(|bytes| !bytes.is_empty()) {
            if started.elapsed() >= limits.deadline {
                result.stopped = Some(Stopped::Time);
                break;
            }
            if listed >= limits.max_files {
                result.stopped = Some(Stopped::Files(listed));
                break;
            }
            listed += 1;
            if let Ok(name) = std::str::from_utf8(bytes) {
                consider(name, &mut result);
            }
        }
    } else {
        let mut pending = vec![root.to_path_buf()];
        'walk: while let Some(dir) = pending.pop() {
            if started.elapsed() >= limits.deadline {
                result.stopped = Some(Stopped::Time);
                break;
            }
            let Ok(entries) = std::fs::read_dir(dir) else { continue };
            for entry in entries.flatten() {
                if started.elapsed() >= limits.deadline {
                    result.stopped = Some(Stopped::Time);
                    break 'walk;
                }
                if listed >= limits.max_files {
                    result.stopped = Some(Stopped::Files(listed));
                    break 'walk;
                }
                // Bound visited entries too, so a tree of empty directories cannot evade the limit.
                listed += 1;
                let Ok(kind) = entry.file_type() else { continue };
                if kind.is_dir() {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if !name.starts_with('.') && name != "target" && name != "node_modules" {
                        pending.push(entry.path());
                    }
                } else if kind.is_file() || kind.is_symlink() {
                    if let Some(name) = entry.path().strip_prefix(root).ok().and_then(Path::to_str) {
                        consider(name, &mut result);
                    }
                }
            }
        }
    }
    if result.stopped.is_none() && started.elapsed() >= limits.deadline {
        result.stopped = Some(Stopped::Time);
    }
    result
        .matches
        .sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
    result.matches.dedup();
    result
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
    use std::process::Command;

    /// A repository's own configuration names programs git runs. Opening a downloaded folder and typing `@`
    /// must not start one.
    #[test]
    fn listing_a_repository_runs_nothing_its_configuration_names() {
        let dir = TempDir::new();
        let git = |args: &[&str]| {
            assert!(Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .status()
                .unwrap()
                .success());
        };
        git(&["init", "-q"]);
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        git(&["add", "a.txt"]);
        let marker = dir.path().join("ran");
        let program = dir.path().join("monitor.sh");
        std::fs::write(&program, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        git(&["config", "core.fsmonitor", program.to_str().unwrap()]);
        assert!(list_paths(dir.path()).contains(&"a.txt".to_string()));
        assert!(!marker.exists(), "the repository's fsmonitor ran");
    }

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
    fn suffix(root: &Path, wanted: &str) -> SuffixLookup {
        find_by_suffix(
            root,
            wanted,
            SuffixLimits {
                max_files: 200_000,
                deadline: Duration::from_secs(1),
            },
        )
    }
    fn write_names(root: &Path, names: &[&str]) {
        for name in names {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
    }
    #[test]
    fn suffix_exact_single_multiple_and_none() {
        for git in [false, true] {
            let dir = TempDir::new();
            if git {
                assert!(Command::new("git")
                    .args(["init", "-q"])
                    .arg(dir.path())
                    .status()
                    .unwrap()
                    .success());
            }
            write_names(
                dir.path(),
                &["a.rs", "internal/session/forward_test.go", "a/mod.rs", "longer/mod.rs"],
            );
            assert_eq!(suffix(dir.path(), "./a.rs").matches, paths(&["a.rs"]));
            assert_eq!(
                suffix(dir.path(), "session/forward_test.go").matches,
                paths(&["internal/session/forward_test.go"])
            );
            assert_eq!(
                suffix(dir.path(), "mod.rs").matches,
                paths(&["a/mod.rs", "longer/mod.rs"])
            );
            assert!(suffix(dir.path(), "absent.rs").matches.is_empty());
            assert!(suffix(dir.path(), "od.rs").matches.is_empty());
        }
    }
    #[test]
    fn suffix_invalid_names_start_no_search() {
        for name in ["", "../x.rs", "a/../b.rs", "/a.rs", "a\nb.rs", "a\u{85}.rs"] {
            let result = find_by_suffix(
                Path::new("/not/a/project"),
                name,
                SuffixLimits {
                    max_files: 0,
                    deadline: Duration::ZERO,
                },
            );
            assert!(result.matches.is_empty(), "{name:?}");
            assert_eq!(result.stopped, None, "{name:?}");
        }
    }
    #[test]
    fn suffix_walk_skips_hidden_build_and_linked_directories() {
        let dir = TempDir::new();
        let outside = TempDir::new();
        write_names(
            dir.path(),
            &[".hidden/x.rs", "target/x.rs", "node_modules/x.rs", "src/x.rs"],
        );
        write_names(outside.path(), &["x.rs"]);
        std::os::unix::fs::symlink(outside.path(), dir.path().join("linked")).unwrap();
        assert_eq!(suffix(dir.path(), "x.rs").matches, paths(&["src/x.rs"]));
    }
    #[test]
    fn suffix_listing_obeys_file_and_time_bounds() {
        for git in [false, true] {
            let dir = TempDir::new();
            if git {
                assert!(Command::new("git")
                    .args(["init", "-q"])
                    .arg(dir.path())
                    .status()
                    .unwrap()
                    .success());
            }
            for i in 0..10 {
                write_names(dir.path(), &[&format!("file{i}.rs")]);
            }
            let result = find_by_suffix(
                dir.path(),
                "missing.rs",
                SuffixLimits {
                    max_files: 3,
                    deadline: Duration::from_secs(1),
                },
            );
            assert_eq!(result.stopped, Some(Stopped::Files(3)));
            let result = find_by_suffix(
                dir.path(),
                "missing.rs",
                SuffixLimits {
                    max_files: 10,
                    deadline: Duration::ZERO,
                },
            );
            assert_eq!(result.stopped, Some(Stopped::Time));
        }
    }
    #[test]
    fn suffix_git_ignores_and_never_runs_the_repositorys_monitor() {
        let dir = TempDir::new();
        let root = dir.path();
        assert!(Command::new("git")
            .args(["init", "-q"])
            .arg(root)
            .status()
            .unwrap()
            .success());
        write_names(root, &["src/a.rs", "ignored/a.rs"]);
        std::fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
        let marker = root.join("ran");
        let program = root.join("monitor.sh");
        std::fs::write(&program, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["config", "core.fsmonitor", program.to_str().unwrap()])
            .status()
            .unwrap()
            .success());
        assert_eq!(suffix(root, "a.rs").matches, paths(&["src/a.rs"]));
        assert!(!marker.exists());
    }
    #[test]
    fn suffix_empty_git_listing_falls_back_to_walk() {
        let dir = TempDir::new();
        assert!(Command::new("git")
            .args(["init", "-q"])
            .arg(dir.path())
            .status()
            .unwrap()
            .success());
        std::fs::write(dir.path().join(".gitignore"), "*\n").unwrap();
        write_names(dir.path(), &["src/a.rs"]);
        assert_eq!(suffix(dir.path(), "a.rs").matches, paths(&["src/a.rs"]));
    }

    #[test]
    fn suffix_refuses_bidi_in_wanted_and_drops_bidi_matches() {
        let dir = TempDir::new();
        write_names(dir.path(), &["bad\u{202e}/a.rs", "good/a.rs", "bad\u{202e}.rs"]);
        assert!(suffix_wanted("bad\u{202e}.rs").is_none());
        assert!(suffix(dir.path(), "bad\u{202e}.rs").matches.is_empty());
        assert_eq!(suffix(dir.path(), "a.rs").matches, paths(&["good/a.rs"]));
    }

    #[test]
    fn suffix_drops_directories_fifos_and_links_to_them() {
        use std::os::unix::fs::symlink;
        for git in [false, true] {
            let dir = TempDir::new();
            if git {
                assert!(Command::new("git")
                    .args(["init", "-q"])
                    .arg(dir.path())
                    .status()
                    .unwrap()
                    .success());
            }
            std::fs::create_dir(dir.path().join("dir.rs")).unwrap();
            assert!(Command::new("mkfifo")
                .arg(dir.path().join("pipe.rs"))
                .status()
                .unwrap()
                .success());
            symlink(dir.path().join("dir.rs"), dir.path().join("dir-link.rs")).unwrap();
            symlink(dir.path().join("pipe.rs"), dir.path().join("pipe-link.rs")).unwrap();
            for name in ["dir.rs", "pipe.rs", "dir-link.rs", "pipe-link.rs"] {
                assert!(suffix(dir.path(), name).matches.is_empty(), "{name}");
            }
        }
    }

    #[test]
    fn suffix_slow_git_is_killed_and_reaped_at_deadline() {
        // A subprocess isolates PATH from other tests; the stand-in records the process and group
        // that the real read-only command builder and runner must terminate.
        const CHILD: &str = "EITRI_SUFFIX_SLOW_GIT_CHILD";
        if let Some(root) = std::env::var_os(CHILD) {
            let root = Path::new(&root);
            let result = find_by_suffix(
                root,
                "a.rs",
                SuffixLimits {
                    max_files: 100,
                    deadline: Duration::from_secs(1),
                },
            );
            assert_eq!(result.stopped, Some(Stopped::Time));
            assert!(result.matches.is_empty(), "a timed-out git must not start the walk");
            let pid: libc::pid_t = std::fs::read_to_string(root.join("pid"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            // SAFETY: These calls take no pointers; the pid belongs to the stand-in just run.
            unsafe {
                assert_eq!(libc::kill(pid, 0), -1, "git still lives");
                assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
                assert_eq!(libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG), -1);
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::ECHILD),
                    "git was not reaped"
                );
                assert_eq!(libc::kill(-pid, 0), -1, "git's process group still lives");
            }
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        write_names(dir.path(), &["src/a.rs"]);
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let fake = bin.join("git");
        std::fs::write(&fake, "#!/bin/sh\necho $$ > pid\nexec /bin/sleep 60\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "path_complete::tests::suffix_slow_git_is_killed_and_reaped_at_deadline",
                "--nocapture",
            ])
            .env(CHILD, dir.path())
            .env("PATH", bin)
            .status()
            .unwrap();
        assert!(status.success());
    }
}
