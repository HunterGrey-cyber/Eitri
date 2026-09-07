use mlua::{Lua, Table};
use std::cell::RefCell;
use std::rc::Rc;

pub(crate) type CommandEntry = ();

#[derive(Default)]
pub(crate) struct CommandRegistry {
    #[allow(dead_code)]
    _entries: Vec<CommandEntry>,
}

pub(crate) fn install(
    _lua: &Lua,
    _neovibe: &Table,
    _registry: Rc<RefCell<CommandRegistry>>,
) -> mlua::Result<()> {
    Ok(())
}
