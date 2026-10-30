//! The project's second window, on the GTK side: which role this window plays in the project's pair, what the
//! window does with its layout because of it, and what is left when the link between the two windows ends.
//!
//! Window 1 is an ordinary window that holds the project's pair lock; `window.new` starts window 2 from it
//! (`spawn`), and the two talk over the link (`driver`). Window 2 is the same executable, started as `eitri
//! linked-window`: a full window with no editor, whose plugin pages and Lua panels stay with window 1. A window
//! that finds the pair lock held by another runs unpaired: it never starts a second window and never writes the
//! pair's files. The pieces here that decide something are plain functions over the layout, so they are tested
//! without a display; the GTK wiring is `build_ui`'s.

pub mod driver;
pub mod editor;
pub(crate) mod focus;
pub mod pair_files;
pub mod relink;
pub mod review;
pub mod spawn;

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use eitri_core::layout::workspace::Workspaces;
use eitri_core::layout::{Frame, Layout, LayoutError, ModuleId, ModuleKind};
use eitri_core::window_link::pair::PairLock;
use eitri_core::window_link::start;
use neovide_editor::NeovideEditorPane;

pub use driver::LinkDriver;
pub use eitri_core::window_link::end::{away_everywhere, held, hold_away};

/// What `window.new` says in a window that runs unpaired.
pub const UNPAIRED_REFUSAL: &str = "this project's second window belongs to another Eitri window";
/// What `window.new` says in a window that could not take part in a pair at all.
pub const ALONE_REFUSAL: &str = "this window cannot start a second window: it has no state directory for the pair";
/// What window 2's editor calls answer once the link to window 1 is gone, by a clean close or not.
pub const UNLINKED_NOTE: &str = "the editor window is closed";
/// The standing note window 2 shows meanwhile, and what its window verbs say: the way back is the editor key.
pub const ORPHAN_NOTE: &str = eitri_core::wm::feedback::ORPHAN_TEXT;
/// What `window.other` says when there is no other window to go to.
pub const NO_OTHER_WINDOW: &str = "this project has no second window open";

/// The window's place in the project's pair, held by `build_ui` for the window's life.
pub enum WindowRole {
    /// No pair is possible here (no state directory for the pair lock): today's window, alone.
    Alone,
    /// The window holding the pair lock: today's files, the editor, and window 2 once `window.new` started it.
    Main { link: Option<LinkDriver> },
    /// The window started by window 1, with no editor.
    Second { link: LinkDriver },
    /// A full window on a project whose pair another window holds.
    Unpaired,
}

/// What `window.new` does in this window, decided before anything is started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowNew {
    /// Start window 2 and link it.
    Spawn,
    /// Window 2 is up: raise it.
    Raise,
    /// Refused with this text.
    Refuse(&'static str),
}

impl WindowRole {
    /// The role a full window starts in: holding the pair lock it is the pair's window 1, finding it held it runs
    /// unpaired, and with no lock to take at all (no state directory, or the lock file could not be opened) it
    /// runs alone. The lock, when taken, is returned to be held for the window's life.
    pub fn of_full_window(taken: io::Result<Option<PairLock>>) -> (WindowRole, Option<PairLock>) {
        match taken {
            Ok(Some(lock)) => (WindowRole::Main { link: None }, Some(lock)),
            Ok(None) => (WindowRole::Unpaired, None),
            Err(err) => {
                eprintln!("[window] the pair lock could not be taken ({err}); this window runs alone");
                (WindowRole::Alone, None)
            }
        }
    }

    /// Whether this window builds and owns the editor: every window but window 2.
    pub fn owns_editor(&self) -> bool {
        !matches!(self, WindowRole::Second { .. })
    }

    /// Whether this window reads and writes the project's plain layout and tabs files. Window 2 and an unpaired
    /// window keep files of their own (`pair_files`).
    pub fn writes_main_files(&self) -> bool {
        matches!(self, WindowRole::Main { .. } | WindowRole::Alone)
    }

    /// The link to the other window, while there is one.
    pub fn link(&self) -> Option<&LinkDriver> {
        match self {
            WindowRole::Main { link } => link.as_ref(),
            WindowRole::Second { link } => Some(link),
            WindowRole::Alone | WindowRole::Unpaired => None,
        }
    }

    pub fn window_new(&self) -> WindowNew {
        match self {
            WindowRole::Alone => WindowNew::Refuse(ALONE_REFUSAL),
            WindowRole::Unpaired => WindowNew::Refuse(UNPAIRED_REFUSAL),
            WindowRole::Main { link: Some(link) } if link.is_up() => WindowNew::Raise,
            WindowRole::Main { .. } => WindowNew::Spawn,
            // Window 2's other window is window 1, which is already there.
            WindowRole::Second { .. } => WindowNew::Raise,
        }
    }

    /// The role as the pair's files and Lua name it: a window alone keeps today's files and is window `1`.
    pub fn pair_role(&self) -> eitri_core::window_link::pair::Role {
        use eitri_core::window_link::pair::Role;
        match self {
            WindowRole::Alone | WindowRole::Main { .. } => Role::Main,
            WindowRole::Second { .. } => Role::Second,
            WindowRole::Unpaired => Role::Unpaired,
        }
    }

    /// How the controller describes the window it opens.
    pub fn wm_spec_role(&self) -> (eitri_core::wm::WindowRole, eitri_core::wm::EditorPresence) {
        use eitri_core::wm::{EditorPresence, LinkSide, WindowRole as WmRole};
        match self {
            WindowRole::Second { .. } => (WmRole::Linked { side: LinkSide::Second }, EditorPresence::None),
            _ => (WmRole::Main, EditorPresence::Owned),
        }
    }
}

/// What an editor request answers in a window with no editor of its own.
pub const NO_EDITOR_HERE: &str = "this window has no editor: it is in the first window";

/// The window's own editor, where it has one: what a verb that only reads the editor's cell holds, so it does
/// nothing in a window with no editor rather than needing an editor to exist. The keys and the editor's focus go
/// through the window controller's services, which hold the same `Option`.
#[derive(Clone)]
pub struct OwnEditor(pub Option<Rc<NeovideEditorPane>>);

impl OwnEditor {
    pub fn cell_size(&self) -> Option<(f64, f64)> {
        self.0.as_ref().and_then(|pane| pane.cell_size())
    }
}

/// The editor link of a window with no editor: every question is answered at once that there is none to ask.
pub struct NoEditor;

impl eitri_core::editor_rpc::EditorRpc for NoEditor {
    fn exec_lua(&self, _code: &'static str, _args: Vec<rmpv::Value>) -> eitri_core::nvim_rpc::Pending {
        eitri_core::nvim_rpc::Pending::failed(eitri_core::nvim_rpc::RpcError::Unavailable(NO_EDITOR_HERE.into()))
    }

    fn target(&self) -> Option<u64> {
        None
    }
}

/// How `main()` found this process started.
pub enum WindowStart {
    /// `eitri [dir]`: a full window, which takes the project's pair lock if it is free.
    Full,
    /// `eitri linked-window ...`: window 2, its link end checked and owned.
    Second {
        root: PathBuf,
        sock: OwnedFd,
        token: Option<String>,
        /// Started again by window 1's launch (`--restore`), as the pair last ended.
        restore: bool,
    },
}

/// Window 2's first acts, before anything else in the process can open, inherit or print: `args` (what follows
/// `linked-window`) parsed, the link end made close-on-exec, checked and owned, then the window's own log put on
/// stdout and stderr. An error is the reason window 2 cannot start; a log that cannot be opened is not one, since
/// stdout and stderr are `/dev/null` from the spawn and a write there cannot fail.
pub fn second_first_acts(args: &[OsString]) -> Result<WindowStart, String> {
    let parsed = start::parse_args(args)?;
    let sock = start::first_acts(&parsed)?;
    let home = state_home(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    );
    if let Some(log) = home.and_then(|home| start::open_log(&home, &parsed.root).ok()) {
        // SAFETY: `dup2` onto the standard descriptors, from a descriptor open for the call. Nothing else in the
        // process runs yet, so nothing is writing to either.
        unsafe {
            libc::dup2(log.as_raw_fd(), libc::STDOUT_FILENO);
            libc::dup2(log.as_raw_fd(), libc::STDERR_FILENO);
        }
    }
    Ok(WindowStart::Second {
        root: parsed.root,
        sock,
        token: parsed.token,
        restore: parsed.restore,
    })
}

/// `<state home>`: `$XDG_STATE_HOME` when it is absolute, else `$HOME/.local/state`, the rule every state
/// directory of Eitri's follows (`state_paths::state_subdir`).
pub fn state_home(xdg_state_home: Option<&std::ffi::OsStr>, home: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    eitri_core::state_paths::state_subdir(xdg_state_home, home, "x")
        .and_then(|dir| dir.parent().and_then(Path::parent).map(Path::to_path_buf))
}

/// Every module that is a web page of its own kind -- a plugin's page or a Lua panel -- in any workspace of the
/// window. In window 2 these are window 1's: their stores and producers live there.
pub fn pages_and_panels(layout: &Layout, workspaces: &Workspaces) -> BTreeSet<ModuleId> {
    workspaces
        .each(layout)
        .into_iter()
        .flat_map(|(_, layout)| layout.modules())
        .filter(|id| id.kind() == ModuleKind::LuaWebview)
        .collect()
}

/// Window 2's start: its editor leaf retired before anything is drawn (no editor is ever built in it), and every
/// plugin page and Lua panel held away, since window 1 holds them. The layouts keep their shape: the retired
/// leaf's space goes to its neighbours, and an away module keeps its place for when it comes back.
pub fn start_second(layout: &mut Layout, workspaces: &mut Workspaces, frame: &Frame) -> Result<(), LayoutError> {
    retire_editor(layout, frame)?;
    workspaces.retire_everywhere(&ModuleId::editor(), frame);
    for id in pages_and_panels(layout, workspaces) {
        hold_away(layout, workspaces, &id, true);
    }
    Ok(())
}

/// The editor retired in one layout, the chat shown first when the editor was the only module on screen.
fn retire_editor(layout: &mut Layout, frame: &Frame) -> Result<(), LayoutError> {
    use eitri_core::layout::kill::{kill, Reopen};
    let editor = ModuleId::editor();
    if !layout.contains(&editor) || layout.is_gone(&editor) {
        return Ok(());
    }
    match kill(layout, &editor, Reopen::Never, frame) {
        Err(LayoutError::LastVisible(_)) => {
            layout.show(&ModuleId::agent())?;
            kill(layout, &editor, Reopen::Never, frame).map(|_| ())
        }
        other => other.map(|_| ()),
    }
}

/// An unpaired window builds its own instance of every module: what the pair's window marked as held by its other
/// window is shown here like any other module.
pub fn ignore_away(layout: &mut Layout, workspaces: &mut Workspaces) {
    let away: BTreeSet<ModuleId> = workspaces
        .each(layout)
        .into_iter()
        .flat_map(|(_, layout)| layout.away())
        .collect();
    for id in away {
        hold_away(layout, workspaces, &id, false);
    }
}

/// How the link ended, as the window that did not ask for the end saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkEnd {
    /// The other window said `bye` and closed: a clean close.
    Bye { why: String },
    /// The link reached its end with no `bye`: the other window died, or the link broke.
    Eof,
}

/// Loads `module`'s page again, from its store as it is now: a plugin's page with no view here (never built, or
/// dropped when it left for the other window) is built, an away host is reopened, and a page already loaded for
/// this return is reused. A Lua panel's view always loads its URL. Whether the page is available.
pub(crate) fn reload_page_back(
    plugin_panels: &crate::plugin_panels::PluginPanels,
    lua_pages: &crate::gtk_host_services::LuaPages,
    module: &ModuleId,
) -> bool {
    use gtk4::prelude::*;
    use webkit6::prelude::*;
    if plugin_panels.modules().contains(module) {
        return plugin_panels.reload_back(module);
    }
    let Some((_, widget, url, _)) = lua_pages.iter().find(|(m, _, _, _)| m == module) else {
        return false;
    };
    match widget.downcast_ref::<webkit6::WebView>() {
        Some(view) => {
            view.load_uri(url);
            true
        }
        None => false,
    }
}

/// The modules whose hosts the window opens with, of the surfaces it declares at startup: every one but an editor
/// the window does not own, for which the controller makes no surface.
pub fn hosted_at_start(
    decls: &[eitri_core::wm::module::SurfaceDecl],
    presence: &eitri_core::wm::EditorPresence,
) -> Vec<ModuleId> {
    decls
        .iter()
        .filter(|decl| {
            !matches!(decl.kind, eitri_core::wm::SurfaceKind::Editor { .. })
                || *presence == eitri_core::wm::EditorPresence::Owned
        })
        .map(|decl| decl.module.clone())
        .collect()
}

/// What this window last told the window at the other end of its link of the modules it holds, so a change of
/// the layout that leaves that set as it was says nothing again. Each new link starts with nothing said: the
/// window at its end has heard nothing yet, whatever an earlier link was told.
#[derive(Default)]
pub struct HeldSaid(std::cell::RefCell<Option<BTreeSet<String>>>);

impl HeldSaid {
    /// Forgets what was said, for a link just made.
    pub fn new_link(&self) {
        self.0.borrow_mut().take();
    }

    /// Sends `away{ids}` with `held` unless it is what this link was last told; whether it was sent.
    pub fn say(&self, link: &LinkDriver, held: BTreeSet<String>) -> bool {
        if !link.linked() || self.0.borrow().as_ref() == Some(&held) {
            return false;
        }
        let ids = held.iter().cloned().collect();
        if let Err(err) = link.send(eitri_core::window_link::Msg::Away { ids }, Vec::new()) {
            eprintln!("[link] the modules this window holds could not be said: {err}");
            return false;
        }
        *self.0.borrow_mut() = Some(held);
        true
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use eitri_core::layout::{ModuleDecl, Placement, Size};
    use std::collections::BTreeMap;

    /// A layout directory of the test's own, `<scratch>/eitri/layout` as a state directory is laid out (its
    /// parents are this user's, which the private directory rule checks), removed when the test ends.
    pub(crate) struct Scratch(pub(crate) PathBuf, pub(crate) PathBuf);

    impl Scratch {
        pub(crate) fn new(label: &str) -> Scratch {
            let top = std::env::temp_dir().join(format!(
                "eitri-linked-window-{}-{label}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&top).unwrap();
            Scratch(top.join("eitri").join("layout"), top)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.1);
        }
    }

    pub(crate) fn id(text: &str) -> ModuleId {
        ModuleId::parse(text).unwrap()
    }

    fn frame() -> Frame<'static> {
        Frame::new(Size { w: 1280, h: 720 }, 1)
    }

    /// The editor and chat in a row, a terminal and a terminal instance below the editor, a Lua panel and a
    /// plugin page at the side.
    pub(crate) fn window(extra: &[&str]) -> Layout {
        let mut decls = vec![ModuleDecl {
            id: ModuleId::terminal(),
            placement: Placement::BelowEditor,
        }];
        for text in extra {
            let placement = match id(text).kind() {
                ModuleKind::LuaWebview => Placement::RightOfRoot,
                _ => Placement::BelowEditor,
            };
            decls.push(ModuleDecl {
                id: id(text),
                placement,
            });
        }
        Layout::initial(&decls).unwrap()
    }

    pub(crate) fn two_workspaces(stored: Layout) -> Workspaces {
        Workspaces::from_parts(
            vec!["main".into(), "2".into()],
            BTreeMap::from([("2".to_string(), stored)]),
            "main".into(),
            Default::default(),
        )
        .unwrap()
    }

    #[test]
    fn a_window_without_the_pair_lock_runs_unpaired_and_refuses_window_new() {
        let (role, lock) = WindowRole::of_full_window(Ok(None));
        assert!(lock.is_none());
        assert!(matches!(role, WindowRole::Unpaired));
        assert_eq!(role.window_new(), WindowNew::Refuse(UNPAIRED_REFUSAL));
        assert_eq!(
            UNPAIRED_REFUSAL, "this project's second window belongs to another Eitri window",
            "the refusal names whose the second window is"
        );
        assert!(
            !role.writes_main_files(),
            "an unpaired window never writes the pair's files"
        );
        assert!(
            role.owns_editor(),
            "an unpaired window is a full window with its own editor"
        );

        let dir = Scratch::new("pair-lock");
        let root = Path::new("/home/someone/project");
        let (first, held) = WindowRole::of_full_window(PairLock::try_take(&dir.0, root));
        assert!(matches!(first, WindowRole::Main { link: None }));
        assert!(held.is_some(), "the first window holds the lock");
        assert_eq!(first.window_new(), WindowNew::Spawn);
        assert!(first.writes_main_files());
        let (second, none) = WindowRole::of_full_window(PairLock::try_take(&dir.0, root));
        assert!(none.is_none());
        assert!(
            matches!(second, WindowRole::Unpaired),
            "a second full window on the project finds the lock held"
        );
        drop(held);
        // Another test's fork can hold a copy of the descriptor until it execs: the lock is asked for again for
        // a while rather than once.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let again = loop {
            let (again, lock) = WindowRole::of_full_window(PairLock::try_take(&dir.0, root));
            if lock.is_some() || std::time::Instant::now() > deadline {
                break (again, lock);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert!(
            matches!(again.0, WindowRole::Main { .. }) && again.1.is_some(),
            "the lock is free again once its holder is gone"
        );

        let (alone, none) = WindowRole::of_full_window(Err(io::Error::other("no state directory")));
        assert!(none.is_none());
        assert_eq!(alone.window_new(), WindowNew::Refuse(ALONE_REFUSAL));
        assert!(alone.writes_main_files(), "a window alone is today's window");
    }

    #[test]
    fn window_2s_first_away_set_holds_every_plugin_page_and_lua_panel() {
        let mut live = window(&["lua:notes", "lua:markdown", "terminal#2"]);
        let mut stored = window(&["lua:notes"]);
        stored.show(&id("lua:notes")).unwrap();
        let mut workspaces = two_workspaces(stored);
        start_second(&mut live, &mut workspaces, &frame()).unwrap();

        for (name, layout) in workspaces.each(&live) {
            assert!(layout.is_gone(&ModuleId::editor()), "{name}: the editor is retired");
            assert!(!layout.is_shown(&ModuleId::editor()), "{name}");
            for page in ["lua:notes", "lua:markdown"] {
                if layout.contains(&id(page)) {
                    assert!(layout.is_away(&id(page)), "{name}: {page} is window 1's");
                    assert!(!layout.is_shown(&id(page)), "{name}: {page} is not drawn here");
                }
            }
            for own in ["agent", "terminal"] {
                assert!(!layout.is_away(&id(own)), "{name}: window 2 holds its own {own}");
            }
        }
        assert!(!live.is_away(&id("terminal#2")), "a terminal of window 2's own stays");
        assert!(live.is_shown(&ModuleId::agent()), "the chat takes the editor's space");
        assert_eq!(
            away_everywhere(&live, &workspaces),
            BTreeSet::from([id("lua:markdown"), id("lua:notes")])
        );
        let held = held(&live, &workspaces);
        assert!(held.contains("agent") && held.contains("terminal") && held.contains("terminal#2"));
        assert!(
            !held.contains("lua:notes") && !held.contains("lua:markdown"),
            "{held:?}"
        );
        assert!(!held.contains("editor"), "a retired editor is not held: {held:?}");
    }

    #[test]
    fn a_layout_whose_only_shown_module_is_the_editor_starts_with_the_chat() {
        let mut live = window(&[]);
        eitri_core::layout::hide(&mut live, &ModuleId::terminal(), &frame()).unwrap();
        eitri_core::layout::hide(&mut live, &ModuleId::agent(), &frame()).unwrap();
        assert!(live.is_shown(&ModuleId::editor()) && !live.is_shown(&ModuleId::agent()));
        let mut workspaces = Workspaces::single();
        start_second(&mut live, &mut workspaces, &frame()).unwrap();
        assert!(live.is_gone(&ModuleId::editor()));
        assert!(live.is_shown(&ModuleId::agent()), "a window always shows something");
    }

    #[test]
    fn an_unpaired_window_shows_what_the_pair_held_away() {
        let mut live = window(&["lua:notes", "terminal#2"]);
        live.set_away(&id("terminal#2"), true).unwrap();
        let mut stored = window(&["lua:notes"]);
        stored.set_away(&id("lua:notes"), true).unwrap();
        let mut workspaces = two_workspaces(stored);
        ignore_away(&mut live, &mut workspaces);
        assert!(away_everywhere(&live, &workspaces).is_empty());
        assert!(live.contains(&id("terminal#2")), "every module is this window's own");
    }

    fn startup_decls() -> Vec<eitri_core::wm::module::SurfaceDecl> {
        use eitri_core::tabs::ViewId;
        use eitri_core::wm::module::SurfaceDecl;
        use eitri_core::wm::{EditorSpawn, SurfaceKind};
        vec![
            SurfaceDecl {
                module: ModuleId::editor(),
                kind: SurfaceKind::Editor {
                    spawn: EditorSpawn {
                        env: Vec::new(),
                        args: Vec::new(),
                        cwd: None,
                        clean: false,
                        background: None,
                        open: vec![],
                    },
                },
                deferred: false,
            },
            SurfaceDecl {
                module: ModuleId::agent(),
                kind: SurfaceKind::Chat { view: ViewId::BASE },
                deferred: false,
            },
            SurfaceDecl {
                module: ModuleId::terminal(),
                kind: SurfaceKind::Terminal {
                    cwd: PathBuf::from("/p"),
                },
                deferred: false,
            },
        ]
    }

    /// What the controller asks the host to make for `decls` in a window whose editor is `presence`.
    fn surfaces_made(
        decls: &[eitri_core::wm::module::SurfaceDecl],
        presence: eitri_core::wm::EditorPresence,
    ) -> Vec<ModuleId> {
        use eitri_core::wm::{Controller, HostOp, WinId, WindowParts, WindowRole as WmRole, WindowSpec};
        let mut controller = Controller::new(eitri_core::wm::fake::FakeScheduler::new());
        controller.open_window(
            WinId::MAIN,
            WindowSpec {
                role: WmRole::Main,
                presence,
                title: "w".into(),
            },
            WindowParts::new(window(&[]), Workspaces::single(), None),
        );
        controller
            .create_surfaces(WinId::MAIN, decls)
            .ops()
            .filter_map(|op| match op {
                HostOp::CreateSurface { module, .. } => Some(module.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn an_editorless_start_expects_a_host_only_for_what_the_controller_makes() {
        use eitri_core::wm::EditorPresence;
        let decls = startup_decls();
        let expected = hosted_at_start(&decls, &EditorPresence::None);
        assert!(
            !expected.contains(&ModuleId::editor()),
            "a window with no editor opens without its host: {expected:?}"
        );
        let made = surfaces_made(&decls, EditorPresence::None);
        for id in &expected {
            assert!(
                made.contains(id),
                "{id} is checked for a host the controller never makes"
            );
        }
        assert_eq!(expected, vec![ModuleId::agent(), ModuleId::terminal()]);

        let owned = hosted_at_start(&decls, &EditorPresence::Owned);
        assert_eq!(owned, surfaces_made(&decls, EditorPresence::Owned));
        assert!(
            owned.contains(&ModuleId::editor()),
            "a window with its editor checks the editor's host too"
        );
    }

    #[test]
    fn what_a_window_holds_is_said_again_to_every_new_link() {
        use crate::linked_window::driver::tests::pair;
        use eitri_core::window_link::link::Inbound;
        use eitri_core::window_link::Msg;
        use std::time::Duration;

        let held = BTreeSet::from(["agent".to_string(), "terminal".to_string()]);
        let said = HeldSaid::default();
        let expect_away = |inbound: &std::sync::mpsc::Receiver<Inbound>| match inbound
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
        {
            Inbound::Msg {
                msg: Msg::Away { ids }, ..
            } => ids,
            other => panic!("expected away, got {other:?}"),
        };

        let (first, first_theirs, first_inbound) = pair();
        first.set_linked();
        said.new_link();
        assert!(said.say(&first, held.clone()), "the first link hears it at hello");
        assert_eq!(
            expect_away(&first_inbound),
            vec!["agent".to_string(), "terminal".to_string()]
        );
        assert!(
            !said.say(&first, held.clone()),
            "an unchanged set is not said twice on one link"
        );
        drop(first_theirs);
        drop(first);

        let (second, _second_theirs, second_inbound) = pair();
        second.set_linked();
        said.new_link();
        assert!(
            said.say(&second, held.clone()),
            "a later link was never told, so it is told now though the set is what the first link heard"
        );
        assert_eq!(
            expect_away(&second_inbound),
            vec!["agent".to_string(), "terminal".to_string()]
        );

        let (unlinked, _theirs, _inbound) = pair();
        said.new_link();
        assert!(
            !said.say(&unlinked, held),
            "nothing is said before the handshake is done"
        );
    }

    #[test]
    fn the_roles_describe_their_windows_to_the_controller() {
        use eitri_core::window_link::pair::Role;
        let (role, _lock) = WindowRole::of_full_window(Ok(None));
        assert_eq!(
            role.wm_spec_role(),
            (eitri_core::wm::WindowRole::Main, eitri_core::wm::EditorPresence::Owned)
        );
        assert_eq!(
            role.pair_role(),
            Role::Unpaired,
            "Lua names no window for an unpaired one"
        );
        assert_eq!(WindowRole::Alone.pair_role(), Role::Main);
        assert_eq!(WindowRole::Main { link: None }.pair_role(), Role::Main);
    }

    #[test]
    fn the_state_home_follows_the_xdg_rule() {
        use std::ffi::OsStr;
        assert_eq!(
            state_home(Some(OsStr::new("/state")), Some(OsStr::new("/home/a"))),
            Some(PathBuf::from("/state"))
        );
        assert_eq!(
            state_home(Some(OsStr::new("relative")), Some(OsStr::new("/home/a"))),
            Some(PathBuf::from("/home/a/.local/state"))
        );
        assert_eq!(state_home(None, Some(OsStr::new("rel"))), None);
    }
}
