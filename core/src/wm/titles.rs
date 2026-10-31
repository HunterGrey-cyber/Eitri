//! Text and navigation rules shared by window chrome.

use std::path::Path;

use crate::attention::Attention;
use crate::layout::{chip_label, export::head_name, Layout, ModuleId, ModuleKind, Selection};
use crate::window_link::names::WindowName;

/// The top bar's project label shows the directory's own last path component, never the whole
/// resolved path -- which would run long and, for a project under the user's home directory,
/// would publish it in every screenshot/screen-share.
///
/// `Path::file_name` already does the real work (it strips a trailing slash and any bare `.`/`..`
/// components on its own), so this exists mainly to give that behaviour a name and a fallback:
/// the root `/` has no file name at all, and a host that ever hands this a non-canonicalized or
/// otherwise degenerate path (this crate's own caller always canonicalizes first, but this
/// function does not assume that) gets the path back verbatim rather than an empty label.
pub fn project_display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// What the top bar says about the workspaces: the current one's name while there are several, and
/// nothing while there is one -- a window that never made a second workspace shows no word for it.
pub fn workspace_text(order: &[String], current: &str) -> Option<String> {
    (order.len() > 1).then(|| current.to_string())
}

/// The window's title: the project's name, then the product, then -- while the window has several
/// workspaces -- the one on screen. The window has no title bar, so the title is what a window
/// switcher, an overview and a compositor's window rules read to tell two windows apart.
pub fn window_title(project: &str, workspace: Option<&str>) -> String {
    match workspace {
        Some(name) => format!("{project} \u{2014} Eitri \u{b7} {name}"),
        None => format!("{project} \u{2014} Eitri"),
    }
}

/// A chat or page window's title: fixed when the window is built, since the compositor's window rules and switchers
/// read it. A chat is `<project> — Eitri chat` (its number follows from the second on), a page is
/// `<project> — <plugin name> · Eitri`, by the plugin's id while its name is not known. Window 2 has the
/// plain window title.
pub fn module_window_title(project: &str, name: &WindowName, plugin_name: Option<&str>) -> String {
    match name {
        WindowName::Second => window_title(project, None),
        WindowName::Chat(1) => format!("{project} \u{2014} Eitri chat"),
        WindowName::Chat(n) => format!("{project} \u{2014} Eitri chat {n}"),
        WindowName::Page(id) => format!("{project} \u{2014} {} \u{b7} Eitri", plugin_name.unwrap_or(id)),
    }
}

/// What a page window is named by in its title: its plugin's name, else the title its Lua panel registered with, else
/// nothing, and [`module_window_title`] falls back on the id.
pub fn page_name<'a>(id: &str, plugin: Option<&'a str>, panels: &'a [(ModuleId, String)]) -> Option<&'a str> {
    let module = ModuleId::lua(id);
    plugin.or_else(|| {
        panels
            .iter()
            .find(|(panel, _)| *panel == module)
            .map(|(_, title)| title.as_str())
    })
}

/// What the top bar says about the mark `swap.marked` swaps with: nothing without one. The mark
/// is shown only as this text -- a frame or outline on the marked module would take layout space
/// from it, or be one more kind of chrome around a pane where focus is shown by cursors alone.
pub fn mark_text(mark: Option<&ModuleId>, title: &dyn Fn(&ModuleId) -> String) -> Option<String> {
    mark.map(|id| format!("marked: {}", title(id)))
}

/// `h`/`l` on the top bar: the visible item `step` away from `current`, stopping at either end, or
/// `None` when no item is visible. A tray chip whose module is on screen is not visible, and is
/// stepped over. From no current item, the first visible one.
pub fn step_item(visible: &[bool], current: Option<usize>, step: isize) -> Option<usize> {
    let shown: Vec<usize> = (0..visible.len()).filter(|&i| visible[i]).collect();
    let Some(current) = current.and_then(|c| shown.iter().position(|&i| i == c)) else {
        return shown.first().copied();
    };
    let next = (current as isize + step).clamp(0, shown.len() as isize - 1) as usize;
    Some(shown[next])
}

/// A module's name on its chip and in the prefix strip: the built-ins by kind, a numbered instance by
/// its kind and number (`terminal 2`), a Lua panel by the `title` it registered with (its id if it is
/// somehow not in `lua`).
pub fn module_title(id: &ModuleId, lua: &[(ModuleId, String)]) -> String {
    match id.kind() {
        ModuleKind::Editor => "editor".to_string(),
        ModuleKind::Agent | ModuleKind::Terminal if id.number() >= 2 => {
            format!("{} {}", id.base(), id.number())
        }
        ModuleKind::Agent => "agent".to_string(),
        ModuleKind::Terminal => "terminal".to_string(),
        ModuleKind::Canvas => "canvas".to_string(),
        ModuleKind::LuaWebview => lua
            .iter()
            .find(|(m, _)| m == id)
            .map(|(_, title)| title.clone())
            .unwrap_or_else(|| id.as_str().to_string()),
    }
}

/// A strip entry's text: what the module's chip would read, so a chat view on an inactive tab carries
/// its own tabs' cards' mark (`agent 2 ⚑1`) where a hidden view's chip would. Only a chat view has an
/// attention; `attention` is that view's.
pub fn strip_label(id: &ModuleId, title: &str, attention: Attention) -> String {
    if id.kind() == ModuleKind::Agent {
        chip_label(title, Some(attention))
    } else {
        chip_label(title, None)
    }
}

/// What the top bar says about a selected container: `None` while the selection is the module with
/// the keys, else `selected: <kind> (<titles>)` with the container's shown modules in tree order. The
/// selection is shown only as this text, as the mark is: an outline would take room from the modules.
pub fn selection_text(layout: &Layout, title: &dyn Fn(&ModuleId) -> String) -> Option<String> {
    let Selection::Con(id) = layout.selection() else {
        return None;
    };
    let con = layout.root().find(id)?;
    let kind = head_name(con.layout);
    let titles: Vec<String> = con.leaves().iter().filter(|m| layout.is_shown(m)).map(title).collect();
    Some(format!("selected: {kind} ({})", titles.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_window_is_named_by_its_plugin_then_its_panels_title_then_its_id() {
        let panels = [(ModuleId::lua("notes"), "My Notes".to_string())];
        assert_eq!(page_name("notes", Some("Notes plugin"), &panels), Some("Notes plugin"));
        assert_eq!(page_name("notes", None, &panels), Some("My Notes"));
        assert_eq!(page_name("other", None, &panels), None);
        let name = WindowName::Page("notes".into());
        assert_eq!(
            module_window_title("project", &name, page_name("notes", None, &panels)),
            "project \u{2014} My Notes \u{b7} Eitri"
        );
    }

    #[test]
    fn the_mark_reads_as_text_naming_its_module_and_is_absent_without_one() {
        use super::mark_text;
        let title = |id: &ModuleId| id.as_str().to_string();
        assert_eq!(mark_text(None, &title), None);
        assert_eq!(
            mark_text(Some(&ModuleId::agent()), &title),
            Some("marked: agent".to_string())
        );
    }

    #[test]
    fn the_title_is_the_project_then_eitri() {
        assert_eq!(super::window_title("neovibe", None), "neovibe \u{2014} Eitri");
    }

    #[test]
    fn module_window_titles_are_fixed() {
        use super::module_window_title;
        let chat = WindowName::Chat;
        assert_eq!(
            module_window_title("neovibe", &chat(1), None),
            "neovibe \u{2014} Eitri chat"
        );
        assert_eq!(
            module_window_title("neovibe", &chat(3), None),
            "neovibe \u{2014} Eitri chat 3"
        );
        let page = WindowName::Page("markdown".into());
        assert_eq!(
            module_window_title("neovibe", &page, Some("Markdown preview")),
            "neovibe \u{2014} Markdown preview \u{b7} Eitri"
        );
        assert_eq!(
            module_window_title("neovibe", &page, None),
            "neovibe \u{2014} markdown \u{b7} Eitri",
            "the plugin id stands in for a name that is not known"
        );
        assert_eq!(
            module_window_title("neovibe", &WindowName::Second, None),
            "neovibe \u{2014} Eitri"
        );
    }

    #[test]
    fn a_second_workspace_joins_the_title() {
        assert_eq!(
            super::window_title("neovibe", Some("docs")),
            "neovibe \u{2014} Eitri \u{b7} docs"
        );
    }

    #[test]
    fn the_top_bar_names_the_workspace_only_while_there_are_several() {
        use super::workspace_text;
        let names = |list: &[&str]| list.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        assert_eq!(
            workspace_text(&names(&["main"]), "main"),
            None,
            "one workspace: no word for it"
        );
        assert_eq!(
            workspace_text(&names(&["main", "study"]), "study"),
            Some("study".to_string())
        );
        assert_eq!(
            workspace_text(&names(&["main", "study", "review"]), "main"),
            Some("main".to_string()),
            "main is named too once there are several"
        );
    }

    #[test]
    fn a_normal_path_shows_its_last_component() {
        assert_eq!(
            project_display_name(Path::new("/home/user/src/neovibe")),
            "neovibe"
        );
    }

    #[test]
    fn a_trailing_slash_does_not_change_the_answer() {
        assert_eq!(
            project_display_name(Path::new("/home/user/src/neovibe/")),
            "neovibe"
        );
    }

    #[test]
    fn the_root_directory_has_no_last_component_so_it_falls_back_to_itself() {
        assert_eq!(project_display_name(Path::new("/")), "/");
    }

    /// The tray's chips come and go, and `h`/`l` step over the ones that are not shown,
    /// stopping at the ends as they always did.
    #[test]
    fn h_and_l_step_over_items_that_are_not_shown() {
        use super::step_item;
        // [editor chip (hidden), terminal chip, agent chip (hidden), ↻]
        let visible = [false, true, false, true];
        assert_eq!(
            step_item(&visible, None, 1),
            Some(1),
            "Ctrl+k lands on the first shown item"
        );
        assert_eq!(step_item(&visible, Some(1), 1), Some(3));
        assert_eq!(step_item(&visible, Some(3), 1), Some(3), "stops at the end");
        assert_eq!(step_item(&visible, Some(3), -1), Some(1));
        assert_eq!(step_item(&visible, Some(1), -1), Some(1), "stops at the start");
        assert_eq!(
            step_item(&visible, Some(0), 1),
            Some(1),
            "from a hidden item: the first shown"
        );
        assert_eq!(step_item(&[false, false], None, 1), None);
    }
    #[test]
    fn a_module_is_named_by_its_kind_or_its_registered_title() {
        let lua = [(ModuleId::lua("notes"), "Notes".to_string())];
        assert_eq!(module_title(&ModuleId::editor(), &lua), "editor");
        assert_eq!(module_title(&ModuleId::terminal(), &lua), "terminal");
        assert_eq!(module_title(&ModuleId::lua("notes"), &lua), "Notes");
        assert_eq!(module_title(&ModuleId::lua("gone"), &lua), "lua:gone");
    }
    #[test]
    fn an_instances_title_carries_its_number() {
        let title = |text: &str| module_title(&ModuleId::parse(text).unwrap(), &[]);
        assert_eq!(title("terminal#2"), "terminal 2");
        assert_eq!(title("terminal#9"), "terminal 9");
        assert_eq!(title("agent#3"), "agent 3");
        assert_eq!(title("terminal"), "terminal", "the base keeps its plain name");
        assert_eq!(title("agent"), "agent");
    }
    #[test]
    fn the_selection_reads_its_kind_and_modules() {
        use crate::layout::{focus_parent, Axis, Node};
        let title = |id: &ModuleId| module_title(id, &[]);
        let mut layout = Layout::new(
            Node::split(
                Axis::Row,
                0.5,
                Node::split(
                    Axis::Column,
                    0.5,
                    Node::Leaf(ModuleId::editor()),
                    Node::Leaf(ModuleId::terminal()),
                ),
                Node::Leaf(ModuleId::agent()),
            ),
            ModuleId::editor(),
        )
        .unwrap();
        assert_eq!(
            selection_text(&layout, &title),
            None,
            "no selection: the module with the keys"
        );
        assert!(focus_parent(&mut layout));
        assert_eq!(
            selection_text(&layout, &title),
            Some("selected: column (editor, terminal)".to_string())
        );
        assert!(focus_parent(&mut layout));
        assert_eq!(
            selection_text(&layout, &title),
            Some("selected: row (editor, terminal, agent)".to_string())
        );
        layout.clear_selection();
        assert_eq!(selection_text(&layout, &title), None);
    }
    #[test]
    fn a_strip_entry_reads_like_a_chip() {
        let one = Attention {
            pending: 1,
            ..Default::default()
        };
        assert_eq!(strip_label(&ModuleId::agent(), "agent", one), "agent \u{2691}1");
        assert_eq!(strip_label(&ModuleId::terminal(), "terminal", one), "terminal");
        assert_eq!(strip_label(&ModuleId::editor(), "editor", one), "editor");
    }
    /// The chrome model derives the strips' labels again on every change of a drawn chat's attention, beside
    /// the tray's chips; what each derivation shows is this function over the attention it is given.
    #[test]
    fn a_card_arriving_relabels_the_strip_entry() {
        let none = Attention::default();
        let one = Attention {
            pending: 1,
            arrived: 1,
            ..Default::default()
        };
        let before = strip_label(&ModuleId::agent(), "agent", none);
        let after = strip_label(&ModuleId::agent(), "agent", one);
        assert_eq!(before, "agent");
        assert_eq!(after, "agent \u{2691}1");
        assert_ne!(before, after, "the entry's text changes with the card");
        let cleared = strip_label(&ModuleId::agent(), "agent", none);
        assert_eq!(cleared, "agent", "and back when it is answered");
    }
}
