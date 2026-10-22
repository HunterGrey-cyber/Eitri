//! Window identities and native window descriptions shared with the host, and the controller's own record of
//! each window.

use super::host::TimerToken;
use super::module::Lifecycle;
use super::persist::Saver;
use crate::layout::{Layout, Rect as LayoutRect, Size, Workspaces};
use std::cell::RefCell;
use std::rc::Rc;

/// A stable identity for one application window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WinId(pub u32);

/// A stable identity for a linked process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PeerId(pub u32);

impl WinId {
    /// The initial main window.
    pub const MAIN: Self = Self(1);
}

/// Which end of a linked pair a window occupies.
#[derive(Debug, Clone, PartialEq)]
pub enum LinkSide {
    /// The first end.
    First,
    /// The second end.
    Second,
}

/// The purpose of a native window.
#[derive(Debug, Clone, PartialEq)]
pub enum WindowRole {
    /// The main workspace.
    Main,
    /// A companion chat window.
    Companion,
    /// One end of a linked pair.
    Linked { side: LinkSide },
}

/// How the window reaches the editor.
#[derive(Debug, Clone, PartialEq)]
pub enum EditorPresence {
    /// Owns the editor process.
    Owned,
    /// Displays an editor owned elsewhere.
    Proxy,
    /// Has no editor.
    None,
}

/// The initial description of a window.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowSpec {
    /// The window role.
    pub role: WindowRole,
    /// The editor relationship.
    pub presence: EditorPresence,
    /// The native title.
    pub title: String,
}

/// What the shell hands the controller when it opens a window: the live layout its grid shares, every workspace
/// beside it, and the saver of its state file (`None` for a window that keeps no layout file).
pub struct WindowParts {
    /// The current workspace's layout, the one on screen.
    pub layout: Rc<RefCell<Layout>>,
    /// Every workspace of the window; the current one's layout is `layout`.
    pub workspaces: Rc<RefCell<Workspaces>>,
    /// Writes the window's layouts to the project's state file.
    pub saver: Option<Saver>,
}

/// The controller's record of one window. The shell's remaining arms still write the layout and the workspaces,
/// so the controller reads them with `try_borrow` and never waits for a shell closure that holds them.
pub struct Window {
    pub(super) spec: WindowSpec,
    pub(super) lifecycle: Lifecycle,
    pub(super) layout: Rc<RefCell<Layout>>,
    pub(super) workspaces: Rc<RefCell<Workspaces>>,
    pub(super) saver: Option<Saver>,
    /// The save's debounce timer, while one is armed.
    pub(super) save_timer: Option<TimerToken>,
}

impl Window {
    pub fn new(spec: WindowSpec, parts: WindowParts) -> Window {
        Window {
            lifecycle: Lifecycle::default(),
            spec,
            layout: parts.layout,
            workspaces: parts.workspaces,
            saver: parts.saver,
            save_timer: None,
        }
    }

    pub fn role(&self) -> &WindowRole {
        &self.spec.role
    }

    pub fn presence(&self) -> &EditorPresence {
        &self.spec.presence
    }

    /// How the shell described the window when it opened it.
    pub fn spec(&self) -> &WindowSpec {
        &self.spec
    }
}

/// The independently controlled window presentation flags.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WindowMode {
    /// Whether surrounding chrome is suppressed.
    pub immersive: bool,
    /// Whether the window fills its output.
    pub fullscreen: bool,
    /// Whether the top bar is temporarily exposed.
    pub top_bar_revealed: bool,
}

/// An opaque output identity supplied by the host.
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorId(pub String);

/// One output in desktop layout coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct Monitor {
    /// The stable output identity.
    pub id: MonitorId,
    /// The desktop rectangle.
    pub rect: LayoutRect,
    /// The output scale.
    pub scale: f64,
    /// The displayed output name.
    pub name: String,
}

/// The current content area and scale of a window.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameMetrics {
    /// The available content size.
    pub size: Size,
    /// The native scale.
    pub scale: f64,
}

/// An opaque native token authorizing presentation.
#[derive(Debug, Clone, PartialEq)]
pub struct ActivationToken(pub String);
