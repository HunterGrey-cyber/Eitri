//! The shell's part of the window's layout persistence: the window's [`Saver`], built from the values `build_ui`
//! starts with. Which layout a window opens with, what is written to the state file and when, and the writer
//! thread are `eitri_core::wm::persist`'s; the controller arms the debounce on its scheduler, the shell's hooks
//! only report what changed, and the close's save is the first step of the controller's teardown.

use std::path::{Path, PathBuf};

#[cfg(test)]
use eitri_core::layout::ModuleId;
use eitri_core::layout::{Layout, ModuleDecl, Workspaces};
use eitri_core::window_link::pair::Role;
use eitri_core::wm::persist::Saver;

/// The window's saver. `dir` is `persist::state_dir`'s answer (`None` saves nothing, said once);
/// `layout` and `workspaces` are the ones the window starts with, read here as they are at startup,
/// not yet changed: a launch is not a change. `keep_copy` is `choose_startup_layout`'s. `role` is the window's
/// role in the project's pair, which names its file: the plain file is window 1's (or a window alone's), window 2
/// writes `-w2` and an unpaired window `-u`, so no window writes over another's.
pub(crate) fn saver(
    dir: Option<PathBuf>,
    role: Role,
    project_root: &Path,
    layout: &Layout,
    workspaces: &Workspaces,
    lua: Vec<ModuleDecl>,
    keep_copy: Option<String>,
) -> Saver {
    let mut saver = Saver::new(dir, project_root, lua, keep_copy, layout, workspaces);
    saver.set_role(role);
    saver
}

/// Taken by every test in this crate that runs glib's default main context, so they take turns:
/// a source added there panics while another thread owns it.
#[cfg(test)]
pub(crate) static DEFAULT_CONTEXT_TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linked_window::tests::{id, window, Scratch};
    use eitri_core::layout::persist::{self, file_name_v2_as};

    fn root() -> PathBuf {
        PathBuf::from("/home/someone/project")
    }

    fn shared(layout: Layout) -> (Layout, Workspaces) {
        (layout, Workspaces::single())
    }

    #[test]
    fn a_move_is_an_arrangement_change_and_writes_both_layout_files() {
        let dir = Scratch::new("move-writes");
        let terminal = id("terminal#2");
        // Window 1 holds the instance; window 2 has its placeholder away.
        let (mut one, one_ws) = shared(window(&["terminal#2"]));
        let (mut two, two_ws) = shared({
            let mut l = window(&["terminal#2"]);
            l.set_away(&terminal, true).unwrap();
            l
        });
        let mut first = saver(
            Some(dir.0.clone()),
            Role::Main,
            &root(),
            &one,
            &one_ws,
            Vec::new(),
            None,
        );
        let mut second = saver(
            Some(dir.0.clone()),
            Role::Second,
            &root(),
            &two,
            &two_ws,
            Vec::new(),
            None,
        );
        first.save_now(&one, &one_ws);
        assert!(
            !dir.0.join(persist::file_name_v2(&root())).exists(),
            "a launch is not a change"
        );
        // The move: the terminal is away in window 1 and back in window 2, nothing else changes.
        one.set_away(&terminal, true).unwrap();
        two.set_away(&terminal, false).unwrap();
        assert!(first.save_now(&one, &one_ws));
        assert!(second.save_now(&two, &two_ws));
        let read = |role| match persist::load_as(&dir.0, &root(), role, &[], &[]) {
            persist::Loaded::Restored(opened) => opened.current,
            other => panic!("{role:?}: {other:?}"),
        };
        assert!(
            read(Role::Main).is_away(&terminal),
            "window 1's file has the placeholder"
        );
        assert!(!read(Role::Second).is_away(&terminal), "window 2's file has the module");
        assert!(dir.0.join(file_name_v2_as(&root(), Role::Second)).exists());
    }

    #[test]
    fn moving_the_keys_alone_still_writes_nothing() {
        let dir = Scratch::new("keys-only");
        let (mut one, one_ws) = shared(window(&["terminal#2"]));
        let mut first = saver(
            Some(dir.0.clone()),
            Role::Main,
            &root(),
            &one,
            &one_ws,
            Vec::new(),
            None,
        );
        one.set_focus(&ModuleId::terminal()).unwrap();
        first.save_now(&one, &one_ws);
        assert!(!dir.0.join(persist::file_name_v2(&root())).exists());
    }
}
