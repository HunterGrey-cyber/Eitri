//! Blocking path facts and folder completion for a chooser's worker.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{PathAnswer, PathKind};

pub const DEBOUNCE: Duration = Duration::from_millis(100);
pub const ANSWER_WITHIN: Duration = Duration::from_millis(150);
pub const MAX_ENTRIES: usize = 2000;

/// Expand against explicit roots without consulting the process's current directory.
pub fn expand(text: &str, base: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let path = if text == "~" {
        home?.to_path_buf()
    } else if let Some(rest) = text.strip_prefix("~/") {
        // Repeated separators after the tilde must not replace the home root.
        home?.join(rest.trim_start_matches('/'))
    } else if text.starts_with('~') {
        return None;
    } else {
        base.join(text)
    };
    path.is_absolute().then_some(path)
}

/// Read at most one directory. The caller supplies debounce and deadline handling off the UI thread.
pub fn answer(text: &str, base: &Path, home: Option<&Path>) -> PathAnswer {
    let Some(path) = expand(text, base, home) else {
        return PathAnswer::Missing;
    };
    let path = tidy(&path);
    let exact = match kind(&path) {
        Ok(kind) => kind,
        Err(_) => return PathAnswer::Stuck,
    };
    let (directory, segment) = split(text, &path, base, home);
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) => {
            return if exact.is_some() {
                found(path, exact, vec![])
            } else {
                failed(error)
            }
        }
    };
    let mut folders = Vec::new();
    for entry in entries.take(MAX_ENTRIES) {
        let entry = match entry {
            Ok(entry) => entry,
            // A directory known to exist still answers with what was read.
            Err(_) if exact.is_some() => break,
            Err(_) => return PathAnswer::Stuck,
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(segment.as_str()) || (name.starts_with('.') && !segment.starts_with('.')) {
            continue;
        }
        let candidate = directory.join(name);
        // A child that cannot be examined (a looping link, an unreadable target) is not a folder to offer.
        if let Ok(Some(PathKind::Dir)) = kind(&candidate) {
            folders.push(candidate);
        }
    }
    folders.sort();
    if exact.is_none() && folders.is_empty() {
        PathAnswer::Missing
    } else {
        found(path, exact, folders)
    }
}

/// The directory to read and the name to match, taken from the typed text at its last separator so that a
/// trailing `.` stays a segment and a trailing `..` or `~` names the directory itself.
fn split(text: &str, path: &Path, base: &Path, home: Option<&Path>) -> (PathBuf, String) {
    let (head, segment) = match text.rfind('/') {
        Some(index) => (&text[..=index], &text[index + 1..]),
        // A bare `~` names the home folder itself among its siblings.
        None if text.starts_with('~') => {
            return match (path.parent(), path.file_name().and_then(|name| name.to_str())) {
                (Some(parent), Some(name)) => (parent.to_path_buf(), name.to_owned()),
                _ => (path.to_path_buf(), String::new()),
            };
        }
        None => ("", text),
    };
    if segment == ".." || (head.is_empty() && segment.is_empty()) {
        return (path.to_path_buf(), String::new());
    }
    let directory = if head.is_empty() {
        Some(base.to_path_buf())
    } else {
        expand(head, base, home)
    };
    match directory {
        Some(directory) => (tidy(&directory), segment.to_owned()),
        None => (path.to_path_buf(), String::new()),
    }
}

/// Drop `.` components and a trailing separator; `..` is kept because a link before it makes it mean something else.
fn tidy(path: &Path) -> PathBuf {
    path.components().collect()
}

fn kind(path: &Path) -> io::Result<Option<PathKind>> {
    let metadata = match fs::symlink_metadata(path).and_then(|metadata| {
        if metadata.is_symlink() {
            fs::metadata(path)
        } else {
            Ok(metadata)
        }
    }) {
        Ok(metadata) => metadata,
        Err(error) if absent(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    Ok(if metadata.is_dir() {
        Some(PathKind::Dir)
    } else if metadata.is_file() {
        Some(PathKind::File)
    } else {
        None
    })
}

/// A name that cannot exist (absent, under a file, too long, a link loop) is not a folder, not a failure.
fn absent(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory)
        || matches!(error.raw_os_error(), Some(libc::ENAMETOOLONG | libc::ELOOP))
}

fn failed(error: io::Error) -> PathAnswer {
    if absent(&error) {
        PathAnswer::Missing
    } else {
        PathAnswer::Stuck
    }
}

fn found(path: PathBuf, exact: Option<PathKind>, folders: Vec<PathBuf>) -> PathAnswer {
    let mut common = folders.first().and_then(|path| path.to_str()).unwrap_or("").to_owned();
    for folder in folders.iter().skip(1) {
        let next = folder.to_str().unwrap_or("");
        let length = common
            .chars()
            .zip(next.chars())
            .take_while(|(a, b)| a == b)
            .map(|(c, _)| c.len_utf8())
            .sum();
        common.truncate(length);
    }
    PathAnswer::Found {
        path,
        exact,
        folders,
        common,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_chooser::PathKind;
    use std::fs;
    use std::path::{Path, PathBuf};

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("eitri-chooser-path-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn dir(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::create_dir_all(&path).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn found(text: &str, base: &Path) -> (PathBuf, Option<PathKind>, Vec<PathBuf>, String) {
        match answer(text, base, None) {
            PathAnswer::Found {
                path,
                exact,
                folders,
                common,
            } => (path, exact, folders, common),
            other => panic!("expected a readable path answer, got {other:?}"),
        }
    }

    #[test]
    fn tilde_expands_against_home_and_tilde_user_does_not() {
        let base = Path::new("/project");
        let home = Some(Path::new("/home/person"));
        assert_eq!(expand("~", base, home), Some(PathBuf::from("/home/person")));
        assert_eq!(expand("~/", base, home), Some(PathBuf::from("/home/person/")));
        assert_eq!(
            expand("~/src/app", base, home),
            Some(PathBuf::from("/home/person/src/app"))
        );
        assert_eq!(expand("~person/src", base, home), None);
    }

    #[test]
    fn relative_forms_are_against_base_never_the_cwd() {
        let base = Path::new("/elsewhere/project");
        assert_eq!(expand("./src", base, None), Some(base.join("./src")));
        assert_eq!(expand("../sibling", base, None), Some(base.join("../sibling")));
        assert_eq!(expand("/absolute", base, None), Some(PathBuf::from("/absolute")));
    }

    #[test]
    fn no_home_leaves_tilde_unexpanded() {
        assert_eq!(expand("~", Path::new("/base"), None), None);
        assert_eq!(expand("~/src", Path::new("/base"), None), None);
        assert_eq!(answer("~/src", Path::new("/base"), None), PathAnswer::Missing);
    }

    #[test]
    fn the_text_itself_a_directory_is_exact_dir() {
        let scratch = Scratch::new();
        let dir = scratch.dir("app");
        let (path, exact, _, _) = found("./app", &scratch.0);
        assert_eq!(path, scratch.0.join("./app"));
        assert_eq!(exact, Some(PathKind::Dir));
        assert!(path.is_absolute());
        assert_eq!(found(dir.to_str().unwrap(), &scratch.0).1, Some(PathKind::Dir));
    }

    #[test]
    fn a_file_is_exact_file() {
        let scratch = Scratch::new();
        fs::write(scratch.0.join("file"), "text").unwrap();
        let (path, exact, folders, _) = found("./file", &scratch.0);
        assert_eq!(path, scratch.0.join("./file"));
        assert_eq!(exact, Some(PathKind::File));
        assert!(folders.is_empty());
    }

    #[test]
    fn neither_is_missing() {
        let scratch = Scratch::new();
        assert_eq!(answer("./absent", &scratch.0, None), PathAnswer::Missing);
        std::os::unix::fs::symlink("absent", scratch.0.join("dangling")).unwrap();
        assert_eq!(answer("./dangling", &scratch.0, None), PathAnswer::Missing);
        assert_eq!(answer("./absent/child", &scratch.0, None), PathAnswer::Missing);
    }

    #[test]
    fn answers_carry_absolute_paths() {
        let scratch = Scratch::new();
        scratch.dir("app");
        let (path, _, folders, common) = found("./a", &scratch.0);
        assert!(path.is_absolute());
        assert!(!folders.is_empty());
        assert!(folders.iter().all(|folder| folder.is_absolute()));
        assert!(Path::new(&common).is_absolute());
    }

    #[test]
    fn folders_match_the_last_segment_and_files_are_left_out() {
        let scratch = Scratch::new();
        let one = scratch.dir("nested/apple");
        let two = scratch.dir("nested/apricot");
        scratch.dir("nested/banana");
        fs::write(scratch.0.join("nested/apfile"), "text").unwrap();
        std::os::unix::fs::symlink("apple", scratch.0.join("nested/aplink")).unwrap();
        std::os::unix::fs::symlink("absent", scratch.0.join("nested/apgone")).unwrap();
        let (_, exact, folders, _) = found("./nested/ap", &scratch.0);
        assert_eq!(exact, None);
        assert_eq!(folders, vec![scratch.0.join("./nested/aplink"), one, two]);
        let (_, exact, folders, _) = found("./nested/", &scratch.0);
        assert_eq!(exact, Some(PathKind::Dir));
        assert_eq!(folders.len(), 4);
    }

    #[test]
    fn hidden_folders_only_when_the_segment_starts_with_a_dot() {
        let scratch = Scratch::new();
        scratch.dir(".hidden");
        let visible = scratch.dir("visible");
        assert_eq!(found("./", &scratch.0).2, vec![visible]);
        assert_eq!(found("./.h", &scratch.0).2, vec![scratch.0.join(".hidden")]);
        assert_eq!(found("./.", &scratch.0).2, vec![scratch.0.join(".hidden")]);
        assert_eq!(found(".", &scratch.0).2, vec![scratch.0.join(".hidden")]);
        let home = scratch.dir("person");
        fs::create_dir(home.join(".config")).unwrap();
        fs::create_dir(home.join("src")).unwrap();
        match answer("~/.", &scratch.0, Some(&home)) {
            PathAnswer::Found { folders, common, .. } => {
                assert_eq!(folders, vec![home.join(".config")]);
                assert_eq!(common, home.join(".config").to_str().unwrap());
            }
            other => panic!("expected a path answer, got {other:?}"),
        }
    }

    #[test]
    fn a_child_that_cannot_be_examined_is_left_out_not_fatal() {
        let scratch = Scratch::new();
        let good = scratch.dir("a/good");
        std::os::unix::fs::symlink("loop", scratch.0.join("a/loop")).unwrap();
        let shut = scratch.dir("shut");
        std::os::unix::fs::symlink("../shut/inner", scratch.0.join("a/shut")).unwrap();
        fs::create_dir(shut.join("inner")).unwrap();
        // Permission bits do not bind root, so the unreadable target only exists for other users.
        let unreadable = unsafe { libc::geteuid() } != 0;
        use std::os::unix::fs::PermissionsExt;
        if unreadable {
            fs::set_permissions(&shut, fs::Permissions::from_mode(0o000)).unwrap();
        }
        let (_, exact, folders, _) = found("./a/", &scratch.0);
        fs::set_permissions(&shut, fs::Permissions::from_mode(0o755)).unwrap();
        let folders: Vec<_> = folders
            .into_iter()
            .filter(|f| unreadable || !f.ends_with("shut"))
            .collect();
        assert_eq!(exact, Some(PathKind::Dir));
        assert_eq!(folders, vec![good]);
    }

    #[test]
    fn a_name_that_cannot_exist_is_missing() {
        let scratch = Scratch::new();
        let long = "x".repeat(300);
        assert_eq!(answer(&format!("./{long}"), &scratch.0, None), PathAnswer::Missing);
        std::os::unix::fs::symlink("loop", scratch.0.join("loop")).unwrap();
        assert_eq!(answer("./loop", &scratch.0, None), PathAnswer::Missing);
    }

    #[test]
    fn answers_do_not_carry_dot_components() {
        let scratch = Scratch::new();
        scratch.dir("nested/apple");
        let (path, _, folders, common) = found("./nested/ap", &scratch.0);
        assert!(!path.to_str().unwrap().contains("/./"));
        assert_eq!(folders, vec![scratch.0.join("nested/apple")]);
        assert_eq!(common, scratch.0.join("nested/apple").to_str().unwrap());
        assert_eq!(found("./nested/", &scratch.0).0, scratch.0.join("nested"));
    }

    #[test]
    fn common_is_the_longest_shared_prefix() {
        let scratch = Scratch::new();
        scratch.dir("应用甲");
        scratch.dir("应用乙");
        let (_, _, folders, common) = found(scratch.0.join("应").to_str().unwrap(), &scratch.0);
        assert_eq!(folders.len(), 2);
        assert_eq!(common, scratch.0.join("应用").to_str().unwrap());
        assert_eq!(
            found(scratch.0.join("应用甲").to_str().unwrap(), &scratch.0).3,
            scratch.0.join("应用甲").to_str().unwrap()
        );
    }

    #[test]
    fn a_directory_past_the_cap_answers_what_it_read() {
        let scratch = Scratch::new();
        for i in 0..MAX_ENTRIES + 7 {
            scratch.dir(&format!("folder-{i:04}"));
        }
        let (_, exact, folders, common) = found("./", &scratch.0);
        assert_eq!(exact, Some(PathKind::Dir));
        assert_eq!(folders.len(), MAX_ENTRIES);
        assert!(folders.iter().all(|path| path.is_dir()));
        assert!(folders.iter().all(|path| path.to_str().unwrap().starts_with(&common)));
        assert_eq!(DEBOUNCE, std::time::Duration::from_millis(100));
        assert_eq!(ANSWER_WITHIN, std::time::Duration::from_millis(150));
        assert_eq!(MAX_ENTRIES, 2000);
    }
}
