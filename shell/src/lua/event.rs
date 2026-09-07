use mlua::{Lua, Table};
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Default)]
pub(crate) struct EventBus {
    #[allow(dead_code)]
    _handlers: Vec<()>,
}

pub(crate) fn install(
    _lua: &Lua,
    _neovibe: &Table,
    _bus: Rc<RefCell<EventBus>>,
) -> mlua::Result<()> {
    Ok(())
}
