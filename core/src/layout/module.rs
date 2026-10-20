//! Modules belong to a closed set of kinds and are named in the layout tree by a [`ModuleId`].
//! The terminal and chat view can have numbered instances; the editor and canvas have one per
//! window. Nothing here knows what a widget is; `shell` maps each id to its host widget.

use std::fmt;

/// The closed set of module kinds keeps widget construction in the host exhaustive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModuleKind {
    Editor,
    Agent,
    /// The canvas module, named so layouts can refer to it before it is available.
    Canvas,
    LuaWebview,
    /// A terminal with its own shell.
    Terminal,
}

impl ModuleKind {
    pub const fn has_instances(self) -> bool {
        matches!(self, Self::Agent | Self::Terminal)
    }
}

/// One digit addresses an instance and keeps its displayed name short.
pub const MAX_INSTANCE: u8 = 9;

/// A module's name: a built-in id, `terminal#2..#9`, `agent#2..#9`, or `lua:<panel id>`. The
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

    /// The base terminal, whose instance number is one.
    pub fn terminal() -> Self {
        ModuleId("terminal".to_string())
    }

    /// A Lua webview panel, by the `id` its `eitri.panel.register` call gave. Any non-empty
    /// string: `parse_panel_spec` refuses an empty id, because `lua:` alone is not a name
    /// [`ModuleId::parse`] accepts, so it would not round-trip.
    pub fn lua(panel_id: &str) -> Self {
        ModuleId(format!("{LUA_PREFIX}{panel_id}"))
    }

    /// The inverse of [`ModuleId::as_str`]. Unknown or noncanonical names are refused.
    pub fn parse(text: &str) -> Result<Self, ModuleError> {
        match text {
            "editor" => return Ok(Self::editor()),
            "agent" => return Ok(Self::agent()),
            "canvas" => return Ok(ModuleId("canvas".to_string())),
            "terminal" => return Ok(Self::terminal()),
            _ => {}
        }
        if let Some((base, suffix)) = text.split_once('#') {
            let why = match base {
                "terminal" | "agent" => {
                    if suffix == "1" {
                        Some(format!("instance 1 is '{base}'"))
                    } else if suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'2'..=b'9') {
                        return Ok(ModuleId(text.to_string()));
                    } else {
                        Some("an instance number is 2 to 9".to_string())
                    }
                }
                "editor" | "canvas" => Some(format!("'{base}' has one instance per window")),
                _ => None,
            };
            if let Some(why) = why {
                return Err(ModuleError::Instance {
                    text: text.to_string(),
                    why,
                });
            }
        }
        match text.strip_prefix(LUA_PREFIX) {
            Some(panel) if !panel.is_empty() => Ok(Self::lua(panel)),
            _ => Err(ModuleError::Unknown(text.to_string())),
        }
    }

    pub fn instance(kind: ModuleKind, n: u8) -> Result<Self, ModuleError> {
        let base = match kind {
            ModuleKind::Editor => "editor",
            ModuleKind::Agent => "agent",
            ModuleKind::Canvas => "canvas",
            ModuleKind::LuaWebview => "lua",
            ModuleKind::Terminal => "terminal",
        };
        let text = format!("{base}#{n}");
        let why = if kind == ModuleKind::LuaWebview {
            "a Lua panel is named by its id".to_string()
        } else if n == 1 {
            return Self::parse(base);
        } else if !kind.has_instances() && n >= 2 {
            format!("'{base}' has one instance per window")
        } else if !(2..=MAX_INSTANCE).contains(&n) {
            "an instance number is 2 to 9".to_string()
        } else {
            return Ok(ModuleId(text));
        };
        Err(ModuleError::Instance { text, why })
    }

    pub fn number(&self) -> u8 {
        if self.kind().has_instances() {
            if let Some((_, number)) = self.0.split_once('#') {
                return number.parse().expect("a canonical instance number");
            }
        }
        1
    }

    pub fn base(&self) -> ModuleId {
        ModuleId(self.base_name().to_string())
    }

    pub fn lowest_free(kind: ModuleKind, taken: &[ModuleId]) -> Option<ModuleId> {
        if !kind.has_instances() {
            return None;
        }
        (2..=MAX_INSTANCE)
            .filter_map(|n| Self::instance(kind, n).ok())
            .find(|id| !taken.contains(id))
    }

    fn base_name(&self) -> &str {
        if self.0.starts_with(LUA_PREFIX) {
            &self.0
        } else {
            self.0.split_once('#').map_or(&self.0, |(base, _)| base)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn kind(&self) -> ModuleKind {
        match self.base_name() {
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
    Instance { text: String, why: String },
}

impl fmt::Display for ModuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModuleError::Unknown(name) => write!(f, "unknown module '{name}'"),
            ModuleError::Instance { text, why } => write!(f, "'{text}': {why}"),
        }
    }
}

impl std::error::Error for ModuleError {}

/// Where a module goes when a layout is first built with it.
/// `PanelSlot` supplies this initial placement hint only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// A new split to the right of the whole root. Lua `position = "side"`.
    RightOfRoot,
    /// A new split below the whole root, full width. Lua `position = "bottom"`.
    BelowRoot,
    /// The editor's own leaf: the module takes the editor's place and the editor is **hidden**,
    /// not gone. Lua `position = "main"`.
    InPlaceOfEditor,
    /// A new column split below only the editor's leaf, pinned like [`Placement::BelowRoot`].
    /// This is the terminal's default placement, keeping the agent and side panels at full height.
    /// Falls back to [`Placement::BelowRoot`] if the editor is absent after
    /// [`super::kill::Reopen::Never`]. Only the terminal uses this for first-launch placement.
    BelowEditor,
}

/// A module beyond the editor and agent built into every window. Its key lives in
/// [`super::keys::ModuleKeys`] and its title in `shell`'s `tray::module_title`.
/// Keeping them separate avoids adding fields to the literal constructed by
/// `shell/src/terminal/mod.rs::initial_layout`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleDecl {
    pub id: ModuleId,
    pub placement: Placement,
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_ids_round_trip_and_name_their_kind() {
        for (text, kind, n, base) in [
            ("terminal#2", ModuleKind::Terminal, 2, "terminal"),
            ("terminal#9", ModuleKind::Terminal, 9, "terminal"),
            ("agent#2", ModuleKind::Agent, 2, "agent"),
            ("agent#9", ModuleKind::Agent, 9, "agent"),
        ] {
            let id = ModuleId::parse(text).unwrap();
            assert_eq!(
                (id.as_str(), id.kind(), id.number(), id.base().as_str()),
                (text, kind, n, base)
            );
            assert_eq!(ModuleId::instance(kind, n), Ok(id));
        }
        assert_eq!(ModuleId::instance(ModuleKind::Terminal, 1), Ok(ModuleId::terminal()));
        assert_eq!(ModuleId::instance(ModuleKind::Agent, 1), Ok(ModuleId::agent()));
        assert_eq!(ModuleId::instance(ModuleKind::Editor, 1), Ok(ModuleId::editor()));
        assert_eq!(ModuleId::instance(ModuleKind::Canvas, 1), ModuleId::parse("canvas"));
        for single in [ModuleId::editor(), ModuleId::terminal(), ModuleId::lua("x#2")] {
            assert_eq!(single.number(), 1, "{single}");
            assert_eq!(single.base(), single, "{single}");
        }
    }

    #[test]
    fn a_second_spelling_or_an_instance_of_a_single_kind_is_refused() {
        let range = "an instance number is 2 to 9";
        for (text, why) in [
            ("terminal#1", "instance 1 is 'terminal'"),
            ("agent#1", "instance 1 is 'agent'"),
            ("terminal#0", range),
            ("terminal#02", range),
            ("terminal#10", range),
            ("terminal#", range),
            ("terminal#x", range),
            ("editor#2", "'editor' has one instance per window"),
            ("canvas#2", "'canvas' has one instance per window"),
        ] {
            let err = ModuleId::parse(text).unwrap_err();
            assert_eq!(
                err,
                ModuleError::Instance {
                    text: text.to_string(),
                    why: why.to_string()
                },
                "{text}"
            );
            assert_eq!(err.to_string(), format!("'{text}': {why}"));
        }
        assert_eq!(
            ModuleId::parse("chat#2"),
            Err(ModuleError::Unknown("chat#2".to_string()))
        );
        assert_eq!(
            ModuleId::instance(ModuleKind::Editor, 2),
            Err(ModuleError::Instance {
                text: "editor#2".into(),
                why: "'editor' has one instance per window".into()
            })
        );
        assert_eq!(
            ModuleId::instance(ModuleKind::Terminal, 10),
            Err(ModuleError::Instance {
                text: "terminal#10".into(),
                why: range.into()
            })
        );
        assert_eq!(
            ModuleId::instance(ModuleKind::LuaWebview, 1),
            Err(ModuleError::Instance {
                text: "lua#1".into(),
                why: "a Lua panel is named by its id".into()
            })
        );
    }

    #[test]
    fn a_lua_panel_named_like_an_instance_stays_a_lua_panel() {
        assert_eq!(ModuleId::parse("lua:x#2"), Ok(ModuleId::lua("x#2")));
        assert_eq!(ModuleId::lua("x#2").kind(), ModuleKind::LuaWebview);
        assert_eq!(ModuleId::lua("terminal#2").kind(), ModuleKind::LuaWebview);
    }

    #[test]
    fn a_new_instance_takes_the_lowest_free_number_from_two() {
        let t = |n| ModuleId::instance(ModuleKind::Terminal, n).unwrap();
        assert_eq!(ModuleId::lowest_free(ModuleKind::Terminal, &[]), Some(t(2)));
        assert_eq!(
            ModuleId::lowest_free(ModuleKind::Terminal, &[ModuleId::terminal(), t(2), t(4)]),
            Some(t(3))
        );
        let all: Vec<ModuleId> = (2..=9).map(t).collect();
        assert_eq!(ModuleId::lowest_free(ModuleKind::Terminal, &all), None);
        assert_eq!(ModuleId::lowest_free(ModuleKind::Editor, &[]), None);
        assert_eq!(
            ModuleId::lowest_free(ModuleKind::Agent, &[ModuleId::agent()]),
            Some(ModuleId::instance(ModuleKind::Agent, 2).unwrap())
        );
    }

    #[test]
    fn only_the_terminal_and_the_chat_view_have_instances() {
        assert!(ModuleKind::Terminal.has_instances() && ModuleKind::Agent.has_instances());
        for kind in [ModuleKind::Editor, ModuleKind::Canvas, ModuleKind::LuaWebview] {
            assert!(!kind.has_instances(), "{kind:?}");
        }
    }

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
