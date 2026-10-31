//! What a path argument means. There is one project boundary, the permission policy's: these functions ask it,
//! and turn its answer into what a tool does next. The path nvim is given is the resolved one, never the
//! argument.

use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use agent::{judge_project_path, OutsideReason, PathJudgement};

use super::step::{EditorReach, ToolCx, ToolError};
use super::words;
use crate::agent_bridge::path_holds_control;

/// The most a file may be to load: a huge one freezes the editor.
const FILE_MAX: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Inside the project; this is where, every link followed.
    Inside(PathBuf),
    /// Under the root by its text, and nothing there: an unsaved new file may still be a buffer.
    Missing(PathBuf),
    /// Refused with this sentence, with no card.
    Refused(String),
    /// Outside the project. `shown` is what a card names: where the path leads, as far as it leads anywhere. Compare
    /// this path and never its text, in which two names of different bytes can read alike.
    Outside { shown: PathBuf },
}

fn control_refused(raw: &str) -> ToolError {
    ToolError(format!("refused a path holding a control character: {raw:?}"))
}

/// `path` against `root` by its text alone: `.` gone, `..` taking the name before it. A path with no link in it
/// reads the same on the disk.
fn lexical(root: &Path, path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for part in root.join(path).components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                normal.pop();
            }
            other => normal.push(other.as_os_str()),
        }
    }
    normal
}

/// The policy's judgement of `path`, and of its [`lexical`] form when the policy stopped at a missing name. It looks
/// at nothing past that name, while the lexical form has lost the name with the `..` after it and may be a real
/// path leading anywhere: so `Outside(Missing)` from here means nothing is there on either reading.
fn judge_both_ways(root: &Path, home: Option<&Path>, path: &Path) -> PathJudgement {
    match judge_project_path(root, home, path) {
        PathJudgement::Outside(OutsideReason::Missing) => judge_project_path(root, home, &lexical(root, path)),
        judgement => judgement,
    }
}

/// Where `raw` leads, every link followed. A name missing on the way stops this as it stops the policy, and the
/// lexical form is then the one both go by.
fn leads_to(root: &Path, raw: &str) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(root.join(raw)).or_else(|error| match error.kind() {
        ErrorKind::NotFound | ErrorKind::NotADirectory => std::fs::canonicalize(lexical(root, Path::new(raw))),
        _ => Err(error),
    })
}

/// The policy's judgement of `raw`, on the lexical form too where that matters ([`judge_both_ways`]). A control
/// character is refused before anything looks at the path: it reaches the editor, and no real file name needs one.
pub fn judge(root: &Path, home: Option<&Path>, raw: &str) -> Result<PathJudgement, ToolError> {
    if path_holds_control(raw) {
        return Err(control_refused(raw));
    }
    Ok(judge_both_ways(root, home, Path::new(raw)))
}

/// [`judge`] for one tab. A window-2 tab reaches window 1 only through project paths that follow no link, so what
/// is outside, or inside through a link, is refused there with no card: nobody in window 1 could act on the answer.
pub fn judge_for(cx: &ToolCx<'_>, raw: &str) -> Result<Target, ToolError> {
    let judgement = judge(cx.root, cx.home, raw)?;
    let named = lexical(cx.root, Path::new(raw));
    let linked = cx.reach == EditorReach::Link;
    Ok(match judgement {
        PathJudgement::Inside(resolved) if linked && resolved != named => Target::Refused(words::LINK.to_owned()),
        PathJudgement::Inside(resolved) => Target::Inside(resolved),
        PathJudgement::Outside(OutsideReason::Missing) if named.starts_with(cx.root) => Target::Missing(named),
        PathJudgement::Outside(_) if linked => Target::Refused(words::LINK.to_owned()),
        PathJudgement::Outside(_) => Target::Outside {
            shown: leads_to(cx.root, raw).unwrap_or(named),
        },
    })
}

/// Whether a buffer's name belongs in an answer: `Inside`, or `Missing` under the root.
pub fn buffer_inside(cx: &ToolCx<'_>, name: &Path) -> bool {
    name_inside(cx.root, cx.home, name).is_some()
}

/// Whether the buffer `name` is the outside file `allowed`, named through a link or not.
pub fn names_the_file(name: &Path, allowed: &Path) -> bool {
    name == allowed || std::fs::canonicalize(name).is_ok_and(|real| real == allowed)
}

/// Whether a buffer's text may go in an answer: a buffer of the project, or of the one outside file that was allowed.
/// A name that is not absolute is never a file of the project.
pub fn buffer_told(cx: &ToolCx<'_>, name: &Path, allowed: Option<&Path>) -> bool {
    name.is_absolute() && (buffer_inside(cx, name) || allowed.is_some_and(|allowed| names_the_file(name, allowed)))
}

/// The same judgement with no tab. `Some(resolved)` for a name inside the root, or a name not yet on disk that lies
/// lexically under it; a name holding a control character is `None`.
pub fn name_inside(root: &Path, home: Option<&Path>, name: &Path) -> Option<PathBuf> {
    if path_holds_control(&name.to_string_lossy()) {
        return None;
    }
    match judge_both_ways(root, home, name) {
        PathJudgement::Inside(resolved) => Some(resolved),
        PathJudgement::Outside(OutsideReason::Missing) => {
            let named = lexical(root, name);
            named.starts_with(root).then_some(named)
        }
        PathJudgement::Outside(_) => None,
    }
}

/// Where an approved outside path leads, every link followed, as [`judge_for`]'s `shown` names it.
pub fn outside_target(root: &Path, raw: &str) -> Result<PathBuf, ToolError> {
    if path_holds_control(raw) {
        return Err(control_refused(raw));
    }
    leads_to(root, raw).map_err(|error| match error.kind() {
        ErrorKind::NotFound => ToolError(format!("no such file: {raw}")),
        _ => ToolError(format!("{raw} could not be resolved: {error}")),
    })
}

/// Whether a file may be loaded: a regular file of at most 16 MiB. By `symlink_metadata`, so nothing is opened: a
/// load of a FIFO would block the editor.
pub fn file_check(path: &Path) -> Result<(), ToolError> {
    let shown = path.display();
    let metadata = std::fs::symlink_metadata(path).map_err(|error| match error.kind() {
        ErrorKind::NotFound => ToolError(format!("no such file: {shown}")),
        _ => ToolError(format!("{shown} could not be examined: {error}")),
    })?;
    if !metadata.is_file() {
        return Err(ToolError(format!("{shown} is not a regular file; it was not opened")));
    }
    if metadata.len() > FILE_MAX {
        return Err(ToolError(format!("{shown} is over 16 MiB; it was not opened")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::memory::Memory;
    use super::super::step::{AgentCall, EditorSight};
    use super::*;
    use crate::test_scratch_dir::ScratchDir;
    use agent::setting_sources::ProjectTrust;
    use agent::PermissionMode;
    use std::ffi::CString;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;

    /// A canonical project root under a scratch directory, with a sibling outside it.
    struct Project {
        _scratch: ScratchDir,
        root: PathBuf,
        beside: PathBuf,
    }

    fn project(label: &str) -> Project {
        let scratch = ScratchDir::new("editor-tools-paths", label);
        let base = fs::canonicalize(&*scratch).unwrap();
        let root = base.join("project");
        let beside = base.join("beside");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(&beside).unwrap();
        fs::write(root.join("src/a.rs"), "a").unwrap();
        fs::write(beside.join("b.rs"), "b").unwrap();
        Project {
            _scratch: scratch,
            root,
            beside,
        }
    }

    fn with_cx<R>(root: &Path, reach: EditorReach, f: impl FnOnce(&ToolCx<'_>) -> R) -> R {
        let memory = Memory::default();
        let cx = ToolCx {
            mode: PermissionMode::Manual,
            trust: ProjectTrust::Trusted,
            root,
            home: None,
            reach,
            sight: EditorSight::OnScreen,
            context: None,
            tab: 1,
            session: "session",
            call: "call",
            agent_call: AgentCall::Host,
            memory: &memory,
        };
        f(&cx)
    }

    fn target(root: &Path, reach: EditorReach, raw: &str) -> Target {
        with_cx(root, reach, |cx| judge_for(cx, raw)).unwrap()
    }

    #[test]
    fn a_control_character_is_refused_before_the_disk() {
        let project = project("control");
        for raw in ["src/a\n.rs", "src/\u{1b}a.rs", "\0", "a\u{7f}"] {
            let refused = judge(&project.root, None, raw).unwrap_err();
            assert!(refused.0.contains("control character"), "{refused:?}");
            assert_eq!(outside_target(&project.root, raw).unwrap_err(), refused);
        }
        let file = project.root.join("new\nline");
        fs::write(&file, "x").unwrap();
        assert!(
            judge(&project.root, None, file.to_str().unwrap()).is_err(),
            "even when it exists"
        );
        assert_eq!(name_inside(&project.root, None, &file), None);
    }

    #[test]
    fn a_path_resolves_to_where_it_leads() {
        let project = project("inside");
        let file = project.root.join("src/a.rs");
        symlink(&file, project.root.join("alias.rs")).unwrap();
        for raw in ["src/a.rs", "./src/../src/a.rs", "alias.rs", file.to_str().unwrap()] {
            assert_eq!(
                target(&project.root, EditorReach::Own, raw),
                Target::Inside(file.clone()),
                "{raw}"
            );
        }
    }

    #[test]
    fn a_fifo_is_refused_without_opening_it() {
        let project = project("fifo");
        let fifo = project.root.join("pipe");
        let path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let refused = file_check(&fifo).unwrap_err();
        assert!(refused.0.contains("not a regular file"), "{refused:?}");
    }

    #[test]
    fn a_file_over_16_mib_is_refused() {
        let project = project("large");
        let file = project.root.join("large");
        let handle = fs::File::create(&file).unwrap();
        handle.set_len(FILE_MAX).unwrap();
        assert_eq!(file_check(&file), Ok(()));
        handle.set_len(FILE_MAX + 1).unwrap();
        let refused = file_check(&file).unwrap_err();
        assert!(refused.0.contains("over 16 MiB"), "{refused:?}");
    }

    #[test]
    fn a_missing_or_linked_file_fails_its_check() {
        let project = project("check");
        let file = project.root.join("src/a.rs");
        assert_eq!(file_check(&file), Ok(()));
        let missing = project.root.join("nothing");
        assert_eq!(
            file_check(&missing).unwrap_err().0,
            format!("no such file: {}", missing.display())
        );
        let link = project.root.join("link");
        symlink(&file, &link).unwrap();
        assert!(file_check(&link).is_err(), "a path handed here is already resolved");
        assert!(file_check(&project.root).is_err(), "a directory");
    }

    #[test]
    fn a_window_2_tab_is_refused_outside_and_through_a_link() {
        let project = project("link");
        let refused = Target::Refused(words::LINK.to_owned());
        let file = project.root.join("src/a.rs");
        assert_eq!(
            target(&project.root, EditorReach::Link, "src/a.rs"),
            Target::Inside(file.clone())
        );
        assert_eq!(
            target(&project.root, EditorReach::Link, "src/../src/./a.rs"),
            Target::Inside(file.clone()),
            "a path that only reads longer follows no link"
        );

        let outside = project.beside.join("b.rs");
        assert_eq!(
            target(&project.root, EditorReach::Link, outside.to_str().unwrap()),
            refused
        );
        assert_eq!(target(&project.root, EditorReach::Link, "../beside/b.rs"), refused);

        symlink(&file, project.root.join("alias.rs")).unwrap();
        assert_eq!(target(&project.root, EditorReach::Link, "alias.rs"), refused);
        assert_eq!(
            target(&project.root, EditorReach::Own, "alias.rs"),
            Target::Inside(file),
            "the editor's own window takes the same link"
        );
        symlink(&outside, project.root.join("leaves.rs")).unwrap();
        assert_eq!(target(&project.root, EditorReach::Link, "leaves.rs"), refused);
    }

    #[test]
    fn a_missing_file_under_the_root_is_missing_not_outside() {
        let project = project("missing");
        let new = project.root.join("src/new.rs");
        for reach in [EditorReach::Own, EditorReach::Link] {
            for raw in ["src/new.rs", "./src/x/../new.rs", new.to_str().unwrap()] {
                assert_eq!(target(&project.root, reach, raw), Target::Missing(new.clone()), "{raw}");
            }
        }
    }

    #[test]
    fn a_missing_file_outside_the_root_is_outside() {
        let project = project("missing-outside");
        let new = project.beside.join("new.rs");
        for raw in ["../beside/new.rs", new.to_str().unwrap()] {
            assert_eq!(
                target(&project.root, EditorReach::Own, raw),
                Target::Outside { shown: new.clone() },
                "{raw}"
            );
            assert_eq!(
                target(&project.root, EditorReach::Link, raw),
                Target::Refused(words::LINK.to_owned()),
                "{raw}"
            );
        }
    }

    #[test]
    fn a_missing_name_before_dotdot_hides_no_link_out_of_the_root() {
        let project = project("hidden-link");
        let root = project.root.as_path();
        let secret = project.beside.join("b.rs");
        symlink(&secret, root.join("leaves.rs")).unwrap();
        symlink(&project.beside, root.join("leavesdir")).unwrap();
        for raw in [
            "nothing/../leaves.rs",
            "nothing/../leavesdir/b.rs",
            "src/a.rs/nothing/../../../leaves.rs",
            "nothing/../../beside/b.rs",
        ] {
            assert_eq!(
                target(root, EditorReach::Own, raw),
                Target::Outside { shown: secret.clone() },
                "{raw}"
            );
            assert_eq!(
                target(root, EditorReach::Link, raw),
                Target::Refused(words::LINK.to_owned()),
                "{raw}"
            );
            assert_eq!(
                outside_target(root, raw),
                Ok(secret.clone()),
                "the path an approval gives is the one the card named: {raw}"
            );
            assert_eq!(name_inside(root, None, Path::new(raw)), None, "{raw}");
            assert_eq!(name_inside(root, None, &root.join(raw)), None, "{raw}");
        }
    }

    #[test]
    fn a_missing_name_before_dotdot_inside_the_root_reads_as_what_it_reduces_to() {
        let project = project("reduced");
        let root = project.root.as_path();
        let file = root.join("src/a.rs");
        let new = root.join("src/new.rs");
        symlink(&file, root.join("alias.rs")).unwrap();
        for reach in [EditorReach::Own, EditorReach::Link] {
            assert_eq!(
                target(root, reach, "nothing/../src/a.rs"),
                Target::Inside(file.clone()),
                "a file that is there is not a new file"
            );
            assert_eq!(
                target(root, reach, "nothing/../src/new.rs"),
                Target::Missing(new.clone())
            );
        }
        assert_eq!(
            target(root, EditorReach::Own, "nothing/../alias.rs"),
            Target::Inside(file.clone())
        );
        assert_eq!(
            target(root, EditorReach::Link, "nothing/../alias.rs"),
            Target::Refused(words::LINK.to_owned()),
            "the name it reduces to follows a link"
        );
        assert_eq!(name_inside(root, None, Path::new("nothing/../alias.rs")), Some(file));
        assert_eq!(name_inside(root, None, Path::new("nothing/../src/new.rs")), Some(new));
    }

    #[test]
    fn a_card_names_where_an_outside_path_leads() {
        let project = project("outside");
        let outside = project.beside.join("b.rs");
        symlink(&outside, project.root.join("leaves.rs")).unwrap();
        for raw in ["leaves.rs", "../beside/b.rs", outside.to_str().unwrap()] {
            assert_eq!(
                target(&project.root, EditorReach::Own, raw),
                Target::Outside { shown: outside.clone() },
                "{raw}"
            );
            assert_eq!(outside_target(&project.root, raw), Ok(outside.clone()), "{raw}");
        }
        assert_eq!(
            outside_target(&project.root, "../beside/nothing").unwrap_err().0,
            "no such file: ../beside/nothing"
        );
    }

    #[test]
    fn a_missing_buffer_under_the_root_is_inside_for_an_answer() {
        let project = project("buffer");
        let root = project.root.as_path();
        let new = root.join("src/new.rs");
        with_cx(root, EditorReach::Own, |cx| {
            assert!(buffer_inside(cx, &root.join("src/a.rs")));
            assert!(buffer_inside(cx, &new));
            assert!(!buffer_inside(cx, &project.beside.join("b.rs")));
            assert!(!buffer_inside(cx, &project.beside.join("new.rs")));
        });
        assert_eq!(name_inside(root, None, &new), Some(new.clone()));
        symlink(project.beside.join("b.rs"), root.join("leaves.rs")).unwrap();
        assert_eq!(
            name_inside(root, None, &root.join("leaves.rs")),
            None,
            "a name under the root that leads out of it"
        );
        assert_eq!(
            name_inside(root, Some(root), &root.join("src/a.rs")),
            None,
            "a root that is no boundary holds no name"
        );
    }
}
