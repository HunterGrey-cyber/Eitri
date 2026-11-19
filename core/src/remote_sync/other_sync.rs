//! Sync folder detection shared by the terminal and the installed helper.

use agent::ssh::ProbeReport;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};

trait Reader {
    fn read(&self, path: &Path) -> io::Result<String>;
}

struct Disk;

impl Reader for Disk {
    fn read(&self, path: &Path) -> io::Result<String> {
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)?;
        const LIMIT: u64 = 1024 * 1024;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("ignore file is not a regular file"));
        }
        let mut text = String::new();
        file.take(LIMIT + 1).read_to_string(&mut text)?;
        if text.len() as u64 > LIMIT {
            return Err(io::Error::other("ignore file is too large"));
        }
        Ok(text)
    }
}

fn literal(path: &str) -> bool {
    !path.contains(['*', '?', '[', ']', '{', '}', '\\', '\n', '\r'])
}

/// Only an anchored literal can establish exclusion. An exception anywhere, including in an
/// included file, must be provably disjoint from the entire subtree.
fn excludes(reader: &dyn Reader, folder: &Path, wanted: &str) -> bool {
    fn evaluate(reader: &dyn Reader, file: &Path, wanted: &Path, include: bool, excluded: &mut bool) -> Option<()> {
        for line in reader.read(file).ok()?.lines() {
            if let Some(name) = line.strip_prefix("#include ") {
                let path = Path::new(name);
                if !include
                    || name.trim() != name
                    || name.is_empty()
                    || !path.components().all(|c| matches!(c, Component::Normal(_)))
                {
                    return None;
                }
                evaluate(reader, &file.parent()?.join(path), wanted, false, excluded)?;
            } else if line.trim().starts_with('#') && !line.trim().starts_with("//") {
                // `#escape=` and any other directive changes how later lines read: not understood, so not counted.
                return None;
            } else if let Some(rule) = line.trim().strip_prefix('!') {
                let path = Path::new(rule.trim_end_matches('/'));
                // Syncthing normalizes Unicode before matching, so a non-ASCII exception may name the root in
                // another form: counted as overlapping.
                if !path.is_absolute()
                    || !rule.is_ascii()
                    || !literal(rule)
                    || path.components().any(|part| part == Component::ParentDir)
                    || wanted.starts_with(path)
                    || path.starts_with(wanted)
                {
                    return None;
                }
            } else if line.trim().starts_with("(?") || line.trim().starts_with("#include") {
                return None;
            } else if line.starts_with('/') && literal(line) {
                let rule = line.strip_suffix('/').unwrap_or(line);
                if rule != "/" && wanted.ancestors().any(|p| p.to_str() == Some(rule)) {
                    *excluded = true;
                }
            }
        }
        Some(())
    }
    let mut excluded = false;
    wanted != "/"
        && literal(wanted)
        && evaluate(
            reader,
            &folder.join(".stignore"),
            Path::new(wanted),
            true,
            &mut excluded,
        )
        .is_some()
        && excluded
}

#[derive(Debug)]
pub struct Marker {
    pub path: PathBuf,
    pub excluded: bool,
    pub line: String,
}

fn ignore_line(root: &Path, folder: &Path) -> Option<String> {
    Some(format!("/{}", root.strip_prefix(folder).ok()?.to_str()?))
}

/// The nearest unexcluded marker, or the nearest excluded marker if all folders exclude the project.
pub fn marker(root: &Path) -> io::Result<Option<Marker>> {
    let mut nearest = None;
    for folder in root.ancestors() {
        for name in [".stfolder", ".dropbox", ".dropbox.cache"] {
            let path = folder.join(name);
            match fs::symlink_metadata(&path) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(io::Error::other(format!("cannot check {}: {e}", path.display()))),
            }
            let line = ignore_line(root, folder).ok_or_else(|| io::Error::other("sync folder path is not UTF-8"))?;
            let excluded = name == ".stfolder" && excludes(&Disk, folder, &line);
            let found = Marker { path, line, excluded };
            if !excluded {
                return Ok(Some(found));
            }
            nearest.get_or_insert(found);
        }
    }
    Ok(nearest)
}

/// A refusal to set up sync: `lines` go to the person first, each on a line of its own (the exact line to copy among
/// them), then `error` ends the attempt.
#[derive(Debug)]
pub struct Refusal {
    pub lines: Vec<String>,
    pub error: String,
}

impl From<String> for Refusal {
    fn from(error: String) -> Self {
        Self { lines: vec![], error }
    }
}

fn refuse(host: &str, marker: &Marker) -> Refusal {
    const CONSEQUENCE: &str = "The tool's other devices stop receiving updates for this path. Alternatively, use a \
                               path outside the synced folder on both machines.";
    if marker.path.ends_with(".stfolder") && marker.line != "/" {
        let ignore = marker.path.with_file_name(".stignore");
        return Refusal {
            lines: vec![
                format!("add this exact line to {}:", ignore.display()),
                marker.line.clone(),
            ],
            error: format!(
                "sync refused: {} on {host} is a sync marker; add the line above to {}. {CONSEQUENCE}",
                marker.path.display(),
                ignore.display()
            ),
        };
    }
    Refusal {
        lines: vec![],
        error: format!(
            "sync refused: {} on {host} is a sync marker; exclude {} from this synced folder. {CONSEQUENCE}",
            marker.path.display(),
            marker.line
        ),
    }
}

pub fn check(root: &Path, host: &str, report: &ProbeReport) -> Result<(), Refusal> {
    if let Some(found) = marker(root).map_err(|e| e.to_string())? {
        if !found.excluded {
            return Err(refuse("this machine", &found));
        }
    }
    if let Some(path) = &report.sync_marker {
        let line = report
            .sync_ignore_line
            .clone()
            .or_else(|| ignore_line(root, path.parent()?))
            .ok_or_else(|| {
                format!(
                    "sync refused: cannot determine the exclusion for {} on {host}",
                    path.display()
                )
            })?;
        if report.sync_excluded != Some(true) || report.sync_ignore_line.is_none() || !path.ends_with(".stfolder") {
            return Err(refuse(
                host,
                &Marker {
                    path: path.clone(),
                    excluded: false,
                    line,
                },
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    impl Refusal {
        /// What the person reads, one line after another.
        fn text(&self) -> String {
            let mut text = self.lines.join("\n");
            text.push('\n');
            text.push_str(&self.error);
            text
        }
    }

    use crate::test_scratch_dir::ScratchDir;
    fn fixture(name: &str) -> (ScratchDir, PathBuf, ProbeReport) {
        let dir = ScratchDir::new("other-sync", name);
        let root = dir.join("work/project");
        fs::create_dir_all(&root).unwrap();
        let report = crate::remote_host::probe_step::fixture(&dir);
        (dir, root, report)
    }

    #[test]
    fn stfolder_above_the_path_refuses_with_the_exact_line() {
        let (dir, root, report) = fixture("marker");
        fs::create_dir(dir.join(".stfolder")).unwrap();
        let why = check(&root, "example-host", &report).unwrap_err().text();
        for text in [
            ".stignore:\n/work/project\n",
            "other devices stop receiving updates",
            "outside the synced folder on both machines",
        ] {
            assert!(why.contains(text), "{why}");
        }
    }

    #[test]
    fn stignore_exact_line_or_parent_line_counts_as_excluded() {
        let (dir, root, report) = fixture("exact");
        fs::create_dir(dir.join(".stfolder")).unwrap();
        for rule in ["/work/project", "/work/project/", "/work", "/work/"] {
            fs::write(dir.join(".stignore"), "#include rules\n").unwrap();
            fs::write(dir.join("rules"), rule).unwrap();
            check(&root, "example-host", &report).unwrap();
        }
        fs::write(dir.join("rules"), "#include nested").unwrap();
        fs::write(dir.join("nested"), "/work/project").unwrap();
        assert!(check(&root, "example-host", &report).is_err());
    }

    #[test]
    fn stignore_line_negated_later_does_not_count() {
        let (dir, root, report) = fixture("negated");
        fs::create_dir(dir.join(".stfolder")).unwrap();
        for rule in ["!/work/project", "!/work", "!/work/project/some-file", "!**/project"] {
            fs::write(dir.join(".stignore"), "/work/\n#include exceptions").unwrap();
            fs::write(dir.join("exceptions"), rule).unwrap();
            assert!(check(&root, "example-host", &report).is_err(), "{rule}");
        }
        fs::write(dir.join(".stignore"), "/work\n!/elsewhere").unwrap();
        check(&root, "example-host", &report).unwrap();
    }

    /// `#escape=o` makes Syncthing read `/work` as `/w\rk`, which does not exclude `/work`; any directive other than
    /// a plain `#include` is not understood here, and a non-ASCII exception may name the root in another Unicode form.
    #[test]
    fn escape_directives_and_non_ascii_exceptions_refuse() {
        let (dir, root, report) = fixture("escape");
        fs::create_dir(dir.join(".stfolder")).unwrap();
        for rules in [
            "#escape=o\n/work",
            "/work\n#escape=o",
            "#unknown\n/work",
            "/work\n!/w\u{f6}rk-other",
        ] {
            fs::write(dir.join(".stignore"), rules).unwrap();
            assert!(check(&root, "example-host", &report).is_err(), "{rules:?}");
        }
        fs::write(dir.join("rules"), "#escape=o").unwrap();
        fs::write(dir.join(".stignore"), "#include rules\n/work").unwrap();
        assert!(check(&root, "example-host", &report).is_err());
        fs::write(dir.join(".stignore"), "// a comment\n/work").unwrap();
        check(&root, "example-host", &report).unwrap();
    }

    #[test]
    fn earlier_exceptions_in_either_file_refuse_even_a_literal_parent_exclusion() {
        let (dir, root, report) = fixture("early-exception");
        fs::write(dir.join(".stfolder"), b"").unwrap();
        for exception in ["!/work/project/notes", "!*.md", "!/work"] {
            fs::write(dir.join("rules"), exception).unwrap();
            for rules in [format!("{exception}\n/work"), "#include rules\n/work".into()] {
                fs::write(dir.join(".stignore"), rules).unwrap();
                assert!(check(&root, "example-host", &report)
                    .unwrap_err()
                    .text()
                    .contains(".stfolder"));
            }
        }
        fs::write(dir.join(".stignore"), "!/work/other\n/work").unwrap();
        check(&root, "example-host", &report).unwrap();
    }

    #[test]
    fn stignore_glob_is_not_recognised_and_refuses() {
        let (dir, root, report) = fixture("glob");
        fs::create_dir(dir.join(".stfolder")).unwrap();
        fs::write(dir.join(".stignore"), "/work/*").unwrap();
        assert!(check(&root, "example-host", &report)
            .unwrap_err()
            .text()
            .contains("/work/project"));
    }

    #[test]
    fn dropbox_marker_refuses() {
        let (dir, root, report) = fixture("dropbox");
        fs::write(dir.join(".stignore"), "/work/project").unwrap();
        for name in [".dropbox", ".dropbox.cache"] {
            fs::create_dir(dir.join(name)).unwrap();
            let why = check(&root, "example-host", &report).unwrap_err().text();
            assert!(why.contains(name) && why.contains("outside the synced folder"), "{why}");
            fs::remove_dir(dir.join(name)).unwrap();
        }
    }

    #[test]
    fn server_marker_comes_from_the_root_aware_probe() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, _, mut report) = fixture("server");
        let root = dir.join("server path with spaces");
        report.sync_marker = Some(dir.join(".stfolder"));
        let helper = dir.join("eitri-server");
        let expected = agent::ssh::encode_args(&[root.as_os_str()]);
        fs::write(
            &helper,
            format!(
                "#!/bin/sh\ntest \"$#:$1:$2:$3\" = '3:probe:--args-hex:{expected}' || exit 1\ncat <<'EOF'\n{}EOF\n",
                report.render()
            ),
        )
        .unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
        let launcher = agent::ssh::LocalExec { helper: helper.clone() };
        let found = crate::remote_host::probe_step::root_probe(&launcher, &helper, "SHA256:example", &root).unwrap();
        assert!(check(&root, "example-host", &found)
            .unwrap_err()
            .text()
            .contains("on example-host"));
        report.sync_excluded = Some(true);
        report.sync_ignore_line = Some("/server path with spaces".into());
        check(&root, "example-host", &ProbeReport::parse(&report.render()).unwrap()).unwrap();
    }
}
