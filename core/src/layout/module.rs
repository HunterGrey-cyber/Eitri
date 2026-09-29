//! What a module is (modules spec §3.1, decision a): one of a **closed** set of kinds, one instance
//! of each built-in per window, named in the layout tree by a [`ModuleId`]. Nothing here knows what
//! a widget is; `shell` maps each id to the one widget that hosts it.

use std::fmt;

/// The kinds a module can be. **Closed on purpose** (decision a, "customization scope A"): a new
/// kind is a change to this enum and to `shell`'s own closed `match`, never a plugin, which is what
/// keeps this from becoming the "dynamic plugin ABI" CLAUDE.md rules out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModuleKind {
    Editor,
    Agent,
    /// P3. Named now so a layout that mentions it parses the day it exists.
    Canvas,
    LuaWebview,
    /// The bottom terminal (docs/superpowers/specs/2026-09-23-bottom-terminal-design.md), hosted as a
    /// module since it reached `main` (modules P1, Task 11). Reserved, and refused by
    /// [`ModuleId::parse`], until its engine landed.
    Terminal,
}

/// A module's name in the layout: `editor`, `agent`, `terminal`, `canvas` or `lua:<panel id>`. The
/// only ways to get one are the constructors below, so [`ModuleId::kind`] is total.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModuleId(String);

const LUA_PREFIX: &str = "lua:";

impl ModuleId {
    pub fn editor() -> Self {
        ModuleId("editor".to_string())
    }

    pub fn agent() -> Self {
        ModuleId("agent".to_string())
    }

    /// The bottom terminal. One per window, like the editor and the agent.
    pub fn terminal() -> Self {
        ModuleId("terminal".to_string())
    }

    /// A Lua webview panel, by the `id` its `neovibe.panel.register` call gave. Any non-empty
    /// string: `parse_panel_spec` refuses an empty id, because `lua:` alone is not a name
    /// [`ModuleId::parse`] accepts, so it would not round-trip.
    pub fn lua(panel_id: &str) -> Self {
        ModuleId(format!("{LUA_PREFIX}{panel_id}"))
    }

    /// The inverse of [`ModuleId::as_str`], for text that names a module (P2's state file and
    /// `neovibe.layout.*`). Anything unknown is refused naming what was given.
    pub fn parse(text: &str) -> Result<Self, ModuleError> {
        match text {
            "editor" => Ok(Self::editor()),
            "agent" => Ok(Self::agent()),
            "canvas" => Ok(ModuleId("canvas".to_string())),
            "terminal" => Ok(Self::terminal()),
            _ => match text.strip_prefix(LUA_PREFIX) {
                Some(panel) if !panel.is_empty() => Ok(Self::lua(panel)),
                _ => Err(ModuleError::Unknown(text.to_string())),
            },
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn kind(&self) -> ModuleKind {
        match self.0.as_str() {
            "editor" => ModuleKind::Editor,
            "agent" => ModuleKind::Agent,
            "canvas" => ModuleKind::Canvas,
            "terminal" => ModuleKind::Terminal,
            _ => ModuleKind::LuaWebview,
        }
    }
}

impl fmt::Display for ModuleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleError {
    Unknown(String),
}

impl fmt::Display for ModuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModuleError::Unknown(name) => write!(f, "unknown module '{name}'"),
        }
    }
}

impl std::error::Error for ModuleError {}

/// Where a module goes the first time a layout is built with it (spec §4.5). A hint for the first
/// launch only: `PanelSlot` survives as exactly this and nothing more (decision 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// A new split to the right of the whole root. Lua `position = "side"`.
    RightOfRoot,
    /// A new split below the whole root, full width. Lua `position = "bottom"`.
    BelowRoot,
    /// The editor's own leaf: the module takes the editor's place and the editor is **hidden**,
    /// not gone. Lua `position = "main"`.
    InPlaceOfEditor,
    /// A new column split below the editor's own leaf only, pinned the way [`Placement::BelowRoot`]
    /// is -- the terminal's default place since v1 trial item 6 (2026-09-28, the owner: "同意，默认
    /// 放到编辑器下面"): below the editor column only, so the agent panel (and any side panel) keep
    /// their full height. Superseded `Placement::BelowEditorAndAgent` (task 6, 2026-09-26), which
    /// wrapped `[editor | agent]` together -- in the default window that was the whole root, so the
    /// terminal used to come back full width below everything, exactly what this variant now avoids.
    /// Falls back to [`Placement::BelowRoot`] if the editor is not in the tree (it was killed with
    /// [`super::kill::Reopen::Never`]). Never a first-launch placement of anything but the terminal
    /// today -- `place_new`'s own doc.
    BelowEditor,
}

/// A module the layout is built with beyond the two built-ins every window has (the editor and the
/// agent). The spec's `title` and `key` (§3.1) live beside it, not on it (modules P2): a module's key
/// is in [`super::keys::ModuleKeys`] and its title is `shell`'s `tray::module_title`, because
/// `shell/src/terminal/mod.rs::initial_layout` builds this struct by literal and a new field would be
/// an edit in the terminal's own files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleDecl {
    pub id: ModuleId,
    pub placement: Placement,
}
#[cfg(test)]
mod tests {
    use super::*;

    /// Every id a constructor can be handed round-trips. `ModuleId::lua("")` would not (`lua:` is
    /// refused below); nothing reaches it with one, because `lua::panel::parse_panel_spec` refuses
    /// an empty panel id (`rejects_an_empty_id`).
    #[test]
    fn every_constructor_round_trips_through_parse() {
        for id in [
            ModuleId::editor(),
            ModuleId::agent(),
            ModuleId::terminal(),
            ModuleId::lua("notes"),
        ] {
            assert_eq!(ModuleId::parse(id.as_str()), Ok(id.clone()), "{id}");
        }
        assert_eq!(ModuleId::parse("canvas").map(|id| id.kind()), Ok(ModuleKind::Canvas));
    }

    #[test]
    fn the_kind_follows_the_name() {
        assert_eq!(ModuleId::editor().kind(), ModuleKind::Editor);
        assert_eq!(ModuleId::agent().kind(), ModuleKind::Agent);
        assert_eq!(ModuleId::lua("x").kind(), ModuleKind::LuaWebview);
        // A Lua panel whose own id happens to be "editor" is still a Lua panel.
        assert_eq!(ModuleId::lua("editor").kind(), ModuleKind::LuaWebview);
        assert_ne!(ModuleId::lua("editor"), ModuleId::editor());
    }

    /// The reservation ends with the engine: `terminal` is a module like the editor and the agent,
    /// and a Lua panel whose own id is "terminal" is still a Lua panel.
    #[test]
    fn terminal_parses_now_that_its_engine_exists() {
        assert_eq!(ModuleId::parse("terminal"), Ok(ModuleId::terminal()));
        assert_eq!(ModuleId::terminal().kind(), ModuleKind::Terminal);
        assert_eq!(ModuleId::lua("terminal").kind(), ModuleKind::LuaWebview);
        assert_ne!(ModuleId::lua("terminal"), ModuleId::terminal());
    }

    /// Decision a: the set is closed, and anything else is refused by name, not as a typo.
    #[test]
    fn anything_unknown_is_refused() {
        for bad in ["", "Editor", "Terminal", "lua:", "plugin:x", "chat"] {
            assert_eq!(
                ModuleId::parse(bad),
                Err(ModuleError::Unknown(bad.to_string())),
                "{bad:?}"
            );
        }
    }
}
