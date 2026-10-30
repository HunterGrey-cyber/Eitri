//! Resolves init.lua's editor preference after the window's configuration phase.

use crate::lua::LuaEngine;
use eitri_core::lua::config::{parse_editor_presentation, EditorPresentation, EDITOR_PRESENTATION_KEY};
use neovide_editor::Presentation;

/// The pane preference init.lua names, or `None` when the key is unset: an unset key leaves the pane on
/// [`Presentation::DEFAULT`] (`auto`).
pub(crate) fn from_config(engine: &LuaEngine) -> Result<Option<Presentation>, String> {
    let config = engine.config.borrow();
    let raw = config.get(EDITOR_PRESENTATION_KEY);
    let value = parse_editor_presentation(raw)?;
    Presentation::validate_environment()?;
    if raw.is_none() {
        return Ok(None);
    }
    Ok(Some(match value {
        EditorPresentation::Auto => Presentation::Auto,
        EditorPresentation::Subsurface => Presentation::Subsurface,
        EditorPresentation::Gtk => Presentation::Gtk,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lua::LuaEngine;
    use neovide_editor::Presentation;

    fn read_init(text: &str) -> Result<Option<Presentation>, String> {
        let scratch = eitri_core::plugin::test_support::Scratch::new("editor-presentation");
        let init = scratch.0.join("init.lua");
        std::fs::write(&init, text).unwrap();
        let engine = LuaEngine::new(scratch.0.clone()).unwrap();
        engine.run_and_check_init_file(&init).unwrap();
        from_config(&engine)
    }

    #[test]
    fn an_init_lua_that_sets_subsurface_reaches_the_pane_value() {
        assert_eq!(
            read_init(r#"eitri.config.set("editor.presentation", "subsurface")"#),
            Ok(Some(Presentation::Subsurface))
        );
    }

    #[test]
    fn an_init_lua_without_the_key_leaves_the_pane_default() {
        assert_eq!(read_init(""), Ok(None));
    }

    #[test]
    fn an_init_lua_that_sets_auto_asks_for_the_automatic_choice() {
        assert_eq!(
            read_init(r#"eitri.config.set("editor.presentation", "auto")"#),
            Ok(Some(Presentation::Auto))
        );
    }

    #[test]
    fn an_invalid_init_value_names_the_key_and_choices() {
        let error = read_init(r#"eitri.config.set("editor.presentation", "x")"#).unwrap_err();
        for word in ["editor.presentation", "auto", "subsurface", "gtk"] {
            assert!(error.contains(word), "{error}");
        }
    }
}
