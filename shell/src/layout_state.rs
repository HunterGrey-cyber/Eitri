//! The shell's part of the window's layout persistence: the window's [`Saver`], built from the values `build_ui`
//! starts with. Which layout a window opens with, what is written to the state file and when, and the writer
//! thread are `eitri_core::wm::persist`'s; the controller arms the debounce on its scheduler, the shell's hooks
//! only report what changed, and the close handler's save is `Driver::persist_now`.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use eitri_core::layout::{Layout, ModuleDecl, Workspaces};
use eitri_core::wm::persist::Saver;

/// The window's saver. `dir` is `persist::state_dir`'s answer (`None` saves nothing, said once);
/// `layout` and `workspaces` are the ones the grid and the window share, read here as they are at startup,
/// not yet changed: a launch is not a change. `keep_copy` is `choose_startup_layout`'s.
pub(crate) fn saver(
    dir: Option<PathBuf>,
    project_root: &Path,
    layout: &Rc<RefCell<Layout>>,
    workspaces: &Rc<RefCell<Workspaces>>,
    lua: Vec<ModuleDecl>,
    keep_copy: Option<String>,
) -> Saver {
    Saver::new(
        dir,
        project_root,
        lua,
        keep_copy,
        &layout.borrow(),
        &workspaces.borrow(),
    )
}

/// Taken by every test in this crate that runs glib's default main context, so they take turns:
/// a source added there panics while another thread owns it.
#[cfg(test)]
pub(crate) static DEFAULT_CONTEXT_TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());
