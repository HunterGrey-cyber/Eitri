//! `eitri.layout.*` requests in the controller: which of them it decides, and the key action each stands for
//! when the container selection judges it.

use crate::keymap::Action;
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

/// Whether [`Controller::lua_request`](super::Controller::lua_request) can carry `request` out: every request but
/// `float`, which is the floats' and still the shell's. What the shell's loop sends it is [`moved`]'s choice.
pub fn decided(request: &LayoutRequest) -> bool {
    match request {
        LayoutRequest::Run(_)
        | LayoutRequest::Mark(_)
        | LayoutRequest::Open(..)
        | LayoutRequest::Show(_)
        | LayoutRequest::Hide(_)
        | LayoutRequest::Focus(_)
        | LayoutRequest::Split(..)
        | LayoutRequest::Switch(_) => true,
        LayoutRequest::Float(..) => false,
    }
}

/// Whether the shell's loop sends `request` whole to the controller
/// ([`Controller::lua_request`](super::Controller::lua_request)): every request it decides, a workspace's switch
/// included; the shell's loop keeps its own arm only for a float's.
pub fn moved(request: &LayoutRequest) -> bool {
    match request {
        LayoutRequest::Run(_)
        | LayoutRequest::Mark(_)
        | LayoutRequest::Open(..)
        | LayoutRequest::Show(_)
        | LayoutRequest::Hide(_)
        | LayoutRequest::Focus(_)
        | LayoutRequest::Split(..)
        | LayoutRequest::Switch(_) => true,
        LayoutRequest::Float(..) => false,
    }
}
