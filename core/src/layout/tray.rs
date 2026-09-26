//! The top bar's tray (modules spec §3.3): one chip for each module that is not on screen, hidden
//! or zoomed away, so nothing that is running is out of reach. A chip is a top-bar item, so
//! `Ctrl+k` then `h`/`l`/`Enter` reaches it, HINT labels it, and a click works.

use super::module::ModuleId;
use super::tree::Layout;
use crate::attention::{AgentPlace, Attention};

/// Every module that is not on screen, in tree order: the tray's chips, left to right. A module gone
/// for the rest of the window (`super::kill`) has none: nothing would bring it back.
pub fn tray(layout: &Layout) -> Vec<ModuleId> {
    layout
        .leaves()
        .into_iter()
        .filter(|id| !layout.is_visible(id) && !layout.is_gone(id))
        .collect()
}

/// A chip's text: the module's title, and for the agent what it is holding -- `agent ⚑2` while cards
/// wait (decision b), `agent •` once a turn finished while it was away.
pub fn chip_label(title: &str, attention: Option<Attention>) -> String {
    match attention {
        Some(Attention { pending, unread, .. }) if pending > 0 => {
            let dot = if unread { " \u{2022}" } else { "" };
            format!("{title} \u{2691}{pending}{dot}")
        }
        Some(Attention { unread: true, .. }) => format!("{title} \u{2022}"),
        _ => title.to_string(),
    }
}

/// Where the agent is (`attention::react` decides from it).
pub fn agent_place(layout: &Layout) -> AgentPlace {
    let agent = ModuleId::agent();
    if layout.is_visible(&agent) {
        AgentPlace::OnScreen
    } else if layout.is_shown(&agent) {
        AgentPlace::ZoomedAway
    } else if layout.zoomed().is_some() {
        AgentPlace::HiddenUnderZoom
    } else {
        AgentPlace::Hidden
    }
}

#[cfg(test)]
mod tests {
    use super::super::geometry::{hide, Frame, Size};
    use super::super::module::{ModuleDecl, Placement};
    use super::*;

    fn frame() -> Frame<'static> {
        Frame::new(Size { w: 1280, h: 721 }, 1)
    }

    #[test]
    fn the_tray_holds_every_module_not_on_screen_in_tree_order() {
        let mut layout = Layout::initial(&[ModuleDecl {
            id: ModuleId::terminal(),
            placement: Placement::BelowRoot,
        }])
        .unwrap();
        assert!(tray(&layout).is_empty());
        hide(&mut layout, &ModuleId::terminal(), &frame()).unwrap();
        assert_eq!(tray(&layout), [ModuleId::terminal()]);
        layout.toggle_zoom(&ModuleId::agent());
        assert_eq!(
            tray(&layout),
            [ModuleId::editor(), ModuleId::terminal()],
            "zoomed away is off screen too"
        );
    }

    #[test]
    fn the_agents_chip_says_what_it_holds() {
        assert_eq!(chip_label("terminal", None), "terminal");
        assert_eq!(chip_label("agent", Some(Attention::default())), "agent");
        assert_eq!(
            chip_label(
                "agent",
                Some(Attention {
                    pending: 2,
                    unread: true,
                    ..Default::default()
                })
            ),
            "agent \u{2691}2 \u{2022}"
        );
        assert_eq!(
            chip_label(
                "agent",
                Some(Attention {
                    pending: 0,
                    unread: true,
                    ..Default::default()
                })
            ),
            "agent \u{2022}"
        );
        assert_eq!(
            chip_label(
                "agent",
                Some(Attention {
                    pending: 1,
                    unread: false,
                    ..Default::default()
                })
            ),
            "agent \u{2691}1"
        );
    }

    #[test]
    fn the_agent_is_on_screen_hidden_hidden_under_a_zoom_or_zoomed_away() {
        let mut layout = Layout::initial(&[]).unwrap();
        assert_eq!(agent_place(&layout), AgentPlace::OnScreen);
        layout.toggle_zoom(&ModuleId::editor());
        assert_eq!(agent_place(&layout), AgentPlace::ZoomedAway);
        layout.unzoom();
        hide(&mut layout, &ModuleId::agent(), &frame()).unwrap();
        assert_eq!(agent_place(&layout), AgentPlace::Hidden);
        let mut three = Layout::initial(&[ModuleDecl {
            id: ModuleId::lua("side"),
            placement: Placement::RightOfRoot,
        }])
        .unwrap();
        hide(&mut three, &ModuleId::agent(), &frame()).unwrap();
        three.toggle_zoom(&ModuleId::editor());
        assert_eq!(agent_place(&three), AgentPlace::HiddenUnderZoom);
    }
}
