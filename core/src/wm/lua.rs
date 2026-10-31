//! `eitri.layout.*` requests in the controller: which of them it decides, and the key action each stands for
//! when the container selection judges it.

use super::host::{Batch, HostReads};
use super::verbs::module_windows::{self, Asked};
use super::window::{WinId, Window};
use crate::keymap::compositor::{classify_window1, HubVerb};
use crate::keymap::Action;
use crate::layout::ModuleKind;
use crate::lua::layout::LayoutRequest;

/// The key action an `eitri.layout.*` request stands for, so the selection treats a Lua call as it treats the
/// key: `hide` is `module.hide`, `mark` is `mark.toggle`, `show` and `focus` a module key, `split` a split key,
/// `float` `float.toggle`. `None` for `run`, which meets the runner's rules itself, and for `switch`. Only the
/// gate reads it: `mark.toggle` would mark the module with the keys and clear on a repeat where `mark(id)`
/// marks `id`, so a request never runs as its action.
pub fn lua_request_action(request: &LayoutRequest) -> Option<Action> {
    match request {
        LayoutRequest::Open(kind, _) => Some(Action::InstanceNew { kind: *kind }),
        LayoutRequest::Hide(_) => Some(Action::ModuleHide),
        LayoutRequest::Mark(_) => Some(Action::MarkToggle),
        LayoutRequest::Show(id) | LayoutRequest::Focus(id) => Some(Action::Module(id.clone())),
        LayoutRequest::Split(_, axis) => Some(Action::Split {
            axis: *axis,
            cwd: Default::default(),
        }),
        LayoutRequest::Float(..) => Some(Action::FloatToggle),
        LayoutRequest::Run(_) | LayoutRequest::Switch(_) => None,
    }
}

/// Whether [`Controller::lua_request`](super::Controller::lua_request) can carry `request` out: every request,
/// `float` included.
pub fn decided(request: &LayoutRequest) -> bool {
    match request {
        LayoutRequest::Run(_)
        | LayoutRequest::Mark(_)
        | LayoutRequest::Open(..)
        | LayoutRequest::Show(_)
        | LayoutRequest::Hide(_)
        | LayoutRequest::Focus(_)
        | LayoutRequest::Split(..)
        | LayoutRequest::Switch(_)
        | LayoutRequest::Float(..) => true,
    }
}

/// What window 1 of a compositor group does with a request. One that places a chat or a page opens the module's
/// window (or brings it forward) and the placement is the compositor's, which one log line says. One that stands for a
/// key that is off here is off as that key is, since window 1's shape is the editor over its terminal and its record
/// could not hold another: `:Eitri workspace`, a new terminal, a split of the terminal. Nothing is refused, so an
/// `init.lua` written for another mode keeps running. `false` for every other request, which is decided as in `eitri`
/// mode.
pub fn hub_request(
    w: &mut Window,
    reads: &dyn HostReads,
    win: WinId,
    request: &LayoutRequest,
    batch: &mut Batch,
) -> bool {
    let window_module = |id: &crate::layout::ModuleId| matches!(id.kind(), ModuleKind::Agent | ModuleKind::LuaWebview);
    match request {
        LayoutRequest::Open(ModuleKind::Agent, _) => {
            w.say("[layout] open agent: placement is the compositor's".into());
            module_windows::next_chat_window(w, reads, win, &Asked::ByKey, batch);
        }
        LayoutRequest::Split(id, _) if window_module(id) => {
            w.say(format!("[layout] split {id}: placement is the compositor's"));
            module_windows::raise_or_spawn(w, win, id, &Asked::ByKey, batch);
        }
        LayoutRequest::Float(id, _) if window_module(id) => {
            w.say(format!("[layout] float {id}: placement is the compositor's"));
            module_windows::raise_or_spawn(w, win, id, &Asked::ByKey, batch);
        }
        LayoutRequest::Show(id) | LayoutRequest::Focus(id) if window_module(id) => {
            module_windows::raise_or_spawn(w, win, id, &Asked::ByKey, batch);
        }
        LayoutRequest::Hide(id) if window_module(id) => {
            w.say(format!("[layout] hide {id}: the window is the compositor's"));
        }
        LayoutRequest::Switch(_) => module_windows::off_by_name(w, reads, "workspace"),
        _ => match lua_request_action(request) {
            Some(action) if classify_window1(&action) == HubVerb::Off => {
                module_windows::off_by_name(w, reads, &action.name())
            }
            _ => return false,
        },
    }
    true
}
