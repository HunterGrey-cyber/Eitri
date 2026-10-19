//! Giving the keys back when the `prefix ?` sheet is closed by its own keys.
//!
//! `prefix ?` shows the chat (which hosts the sheet) and moves the keys into it. Closing the sheet with
//! `?`, `Esc`, `q` or a click outside should leave the person where they were, so the decision lives
//! here, GTK-free and unit-tested: the window reads the layout, calls these, and acts on the answer.
//! Every opening has a number and only the latest opening's close acts, because the page's
//! `keymap_closed` message is asynchronous and can arrive after a newer `prefix ?`.

use crate::layout::ModuleId;

/// What `prefix ?` found and changed when it opened the sheet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opened {
    /// The module that held the keys, read before the sheet's module took them.
    pub from: Option<ModuleId>,
    /// The zoom that `show_on_screen` ended to put the sheet's module on screen.
    pub ended_zoom: Option<ModuleId>,
    /// The sheet's module was hidden, and `show_on_screen` showed it.
    pub showed_sheet: bool,
}

/// The layout and focus as a close finds them.
#[derive(Clone, Copy)]
pub struct AtClose<'a> {
    pub focused: Option<&'a ModuleId>,
    pub zoomed: Option<&'a ModuleId>,
    pub sheet_shown: bool,
    /// Whether a module is in the layout and not hidden now. The module the keys would go back to is
    /// asked before anything moves: it may have been hidden or removed while the sheet was open (a
    /// terminal whose shell exited), and hiding the chat first would then leave the keys nowhere useful.
    pub shown: &'a dyn Fn(&ModuleId) -> bool,
}

/// What the window does after a close, in this order: hide the sheet's module, zoom, give the keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Return {
    pub hide_sheet: bool,
    pub zoom: Option<ModuleId>,
    pub focus: ModuleId,
}

#[derive(Debug, Default)]
pub struct KeysReturn {
    opening: u64,
    kept: Option<Opened>,
}

impl KeysReturn {
    /// `prefix ?` opened the sheet in `sheet`; called only after `show_on_screen` succeeded. Raises the
    /// opening number and returns it. Always replaces what was kept: the new `opened`, or nothing when
    /// `opened.from` is `None` or `sheet`.
    pub fn opened(&mut self, opened: Opened, sheet: &ModuleId) -> u64 {
        self.opening += 1;
        let worth_keeping = matches!(&opened.from, Some(from) if from != sheet);
        self.kept = worth_keeping.then_some(opened);
        self.opening
    }

    /// The keys moved to `to`. Anywhere but `sheet` forgets what was kept.
    pub fn focus_moved(&mut self, to: &ModuleId, sheet: &ModuleId) {
        if to != sheet {
            self.kept = None;
        }
    }

    /// Another route took over the sheet's module (`prefix :` opens the chat's command line): what was
    /// kept is forgotten, so a `keymap_closed` still on its way for the earlier opening, whose number is
    /// still the latest, gives nothing back and cannot move the keys away from the newer request.
    pub fn superseded(&mut self) {
        self.kept = None;
    }

    /// The sheet of opening `opening` closed. A number other than the latest changes nothing and
    /// returns `None`. Otherwise what was kept is taken (forgotten), and a `Return` is given only when
    /// `by_key` is true, `at.focused` is still `sheet`, and no zoom is on except `kept.from`'s own: a
    /// zoom set meanwhile on any other module (the sheet's included) leaves the keys in the sheet, since
    /// giving them to a module the zoom hides would put them where nobody can see. A `kept.from` that
    /// `at.shown` says is not shown leaves the keys, and the chat, where they are. `hide_sheet` is
    /// `kept.showed_sheet && at.sheet_shown`; `zoom` is `kept.ended_zoom` when `at.zoomed` is `None`;
    /// `focus` is `kept.from`.
    pub fn closed(&mut self, opening: u64, by_key: bool, at: AtClose<'_>, sheet: &ModuleId) -> Option<Return> {
        if opening != self.opening {
            return None;
        }
        let kept = self.kept.take()?;
        if !by_key || at.focused != Some(sheet) {
            return None;
        }
        if at.zoomed.is_some_and(|zoomed| Some(zoomed) != kept.from.as_ref()) {
            return None;
        }
        let from = kept.from?;
        if !(at.shown)(&from) {
            return None;
        }
        Some(Return {
            hide_sheet: kept.showed_sheet && at.sheet_shown,
            zoom: if at.zoomed.is_none() { kept.ended_zoom } else { None },
            focus: from,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sheet() -> ModuleId {
        ModuleId::agent()
    }

    fn plugin() -> ModuleId {
        ModuleId::lua("snotra")
    }

    fn from(id: ModuleId) -> Opened {
        Opened {
            from: Some(id),
            ended_zoom: None,
            showed_sheet: false,
        }
    }

    fn every_module_shown(_: &ModuleId) -> bool {
        true
    }

    fn all_but_the_sheet_shown(id: &ModuleId) -> bool {
        *id != sheet()
    }

    fn at<'a>(focused: Option<&'a ModuleId>, zoomed: Option<&'a ModuleId>, sheet_shown: bool) -> AtClose<'a> {
        AtClose {
            focused,
            zoomed,
            sheet_shown,
            shown: if sheet_shown {
                &every_module_shown
            } else {
                &all_but_the_sheet_shown
            },
        }
    }

    #[test]
    fn a_close_by_key_from_the_chat_returns_the_keys_to_the_plugin() {
        let (sheet, plugin) = (sheet(), plugin());
        let mut keys = KeysReturn::default();
        let n = keys.opened(from(plugin.clone()), &sheet);
        let got = keys.closed(n, true, at(Some(&sheet), None, true), &sheet);
        assert_eq!(
            got,
            Some(Return {
                hide_sheet: false,
                zoom: None,
                focus: plugin
            })
        );
    }

    #[test]
    fn a_forced_close_returns_nothing_and_forgets() {
        let (sheet, plugin) = (sheet(), plugin());
        let mut keys = KeysReturn::default();
        let n = keys.opened(from(plugin), &sheet);
        assert_eq!(keys.closed(n, false, at(Some(&sheet), None, true), &sheet), None);
        // Forgotten: a later close by key of the same opening has nothing to give back.
        assert_eq!(keys.closed(n, true, at(Some(&sheet), None, true), &sheet), None);
    }

    #[test]
    fn a_close_after_the_keys_moved_elsewhere_returns_nothing() {
        let (sheet, plugin) = (sheet(), plugin());
        let editor = ModuleId::editor();
        let mut keys = KeysReturn::default();
        let n = keys.opened(from(plugin), &sheet);
        assert_eq!(keys.closed(n, true, at(Some(&editor), None, true), &sheet), None);
        let n = keys.opened(from(ModuleId::terminal()), &sheet);
        assert_eq!(keys.closed(n, true, at(None, None, true), &sheet), None);
    }

    #[test]
    fn a_focus_move_elsewhere_forgets_and_one_to_the_sheet_keeps() {
        let (sheet, plugin) = (sheet(), plugin());
        let mut keys = KeysReturn::default();
        let n = keys.opened(from(plugin.clone()), &sheet);
        keys.focus_moved(&sheet, &sheet);
        let got = keys.closed(n, true, at(Some(&sheet), None, true), &sheet);
        assert_eq!(got.map(|r| r.focus), Some(plugin.clone()));

        let n = keys.opened(from(plugin.clone()), &sheet);
        keys.focus_moved(&ModuleId::editor(), &sheet);
        keys.focus_moved(&sheet, &sheet);
        assert_eq!(keys.closed(n, true, at(Some(&sheet), None, true), &sheet), None);
    }

    #[test]
    fn an_opening_from_the_chat_or_nowhere_keeps_nothing() {
        let sheet = sheet();
        let mut keys = KeysReturn::default();
        let n = keys.opened(from(sheet.clone()), &sheet);
        assert_eq!(keys.closed(n, true, at(Some(&sheet), None, true), &sheet), None);
        let n = keys.opened(
            Opened {
                from: None,
                ended_zoom: Some(plugin()),
                showed_sheet: true,
            },
            &sheet,
        );
        assert_eq!(keys.closed(n, true, at(Some(&sheet), None, true), &sheet), None);
    }

    #[test]
    fn opened_from_a_plugin_then_from_the_chat_then_closed_returns_nothing() {
        let (sheet, plugin) = (sheet(), plugin());
        let mut keys = KeysReturn::default();
        keys.opened(from(plugin), &sheet);
        let second = keys.opened(from(sheet.clone()), &sheet);
        assert_eq!(keys.closed(second, true, at(Some(&sheet), None, true), &sheet), None);
    }

    #[test]
    fn a_late_close_of_an_earlier_opening_changes_nothing_and_keeps_the_second() {
        let (sheet, plugin) = (sheet(), plugin());
        let editor = ModuleId::editor();
        let mut keys = KeysReturn::default();
        let first = keys.opened(from(plugin), &sheet);
        let second = keys.opened(from(editor.clone()), &sheet);
        assert_ne!(first, second);
        assert_eq!(keys.closed(first, true, at(Some(&sheet), None, true), &sheet), None);
        let got = keys.closed(second, true, at(Some(&sheet), None, true), &sheet);
        assert_eq!(got.map(|r| r.focus), Some(editor));
    }

    #[test]
    fn a_second_close_of_one_opening_returns_nothing() {
        let (sheet, plugin) = (sheet(), plugin());
        let mut keys = KeysReturn::default();
        let n = keys.opened(from(plugin), &sheet);
        assert!(keys.closed(n, true, at(Some(&sheet), None, true), &sheet).is_some());
        assert_eq!(keys.closed(n, true, at(Some(&sheet), None, true), &sheet), None);
    }

    #[test]
    fn the_ended_zoom_comes_back_only_when_no_zoom_is_on() {
        let (sheet, plugin) = (sheet(), plugin());
        let opened = Opened {
            from: Some(plugin.clone()),
            ended_zoom: Some(plugin.clone()),
            showed_sheet: false,
        };
        let mut keys = KeysReturn::default();
        let n = keys.opened(opened.clone(), &sheet);
        let got = keys.closed(n, true, at(Some(&sheet), None, true), &sheet).unwrap();
        assert_eq!(got.zoom, Some(plugin.clone()));

        // The module the keys come back to is zoomed again already: nothing to put back.
        let n = keys.opened(opened, &sheet);
        let got = keys
            .closed(n, true, at(Some(&sheet), Some(&plugin), true), &sheet)
            .unwrap();
        assert_eq!(got.zoom, None);
        assert_eq!(got.focus, plugin);
    }

    #[test]
    fn a_zoom_set_while_the_sheet_was_open_keeps_the_keys_in_the_sheet() {
        let (sheet, plugin) = (sheet(), plugin());
        let editor = ModuleId::editor();
        let mut keys = KeysReturn::default();
        // The chat itself zoomed to read a long sheet: the plugin would be hidden by it.
        let n = keys.opened(from(plugin.clone()), &sheet);
        assert_eq!(keys.closed(n, true, at(Some(&sheet), Some(&sheet), true), &sheet), None);
        // Some other module zoomed.
        let n = keys.opened(from(plugin), &sheet);
        assert_eq!(
            keys.closed(n, true, at(Some(&sheet), Some(&editor), true), &sheet),
            None
        );
    }

    #[test]
    fn a_superseded_opening_gives_nothing_back() {
        let (sheet, plugin) = (sheet(), plugin());
        let mut keys = KeysReturn::default();
        let n = keys.opened(from(plugin), &sheet);
        keys.superseded();
        assert_eq!(keys.closed(n, true, at(Some(&sheet), None, true), &sheet), None);
    }

    #[test]
    fn the_chat_is_hidden_again_only_while_it_is_still_shown() {
        let (sheet, plugin) = (sheet(), plugin());
        let opened = Opened {
            from: Some(plugin),
            ended_zoom: None,
            showed_sheet: true,
        };
        let mut keys = KeysReturn::default();
        let n = keys.opened(opened.clone(), &sheet);
        let got = keys.closed(n, true, at(Some(&sheet), None, true), &sheet).unwrap();
        assert!(got.hide_sheet);

        let n = keys.opened(opened.clone(), &sheet);
        let got = keys.closed(n, true, at(Some(&sheet), None, false), &sheet).unwrap();
        assert!(!got.hide_sheet);

        let n = keys.opened(
            Opened {
                showed_sheet: false,
                ..opened
            },
            &sheet,
        );
        let got = keys.closed(n, true, at(Some(&sheet), None, true), &sheet).unwrap();
        assert!(!got.hide_sheet);
    }

    #[test]
    fn a_return_target_hidden_while_the_sheet_was_open_keeps_the_keys_and_the_chat() {
        let sheet = sheet();
        let terminal = ModuleId::terminal();
        let opened = Opened {
            from: Some(terminal.clone()),
            ended_zoom: None,
            showed_sheet: true,
        };
        let mut keys = KeysReturn::default();
        let n = keys.opened(opened, &sheet);
        let terminal_gone = |id: &ModuleId| *id != terminal;
        let at = AtClose {
            focused: Some(&sheet),
            zoomed: None,
            sheet_shown: true,
            shown: &terminal_gone,
        };
        // Nothing to do: the chat is not hidden again and the keys stay in it.
        assert_eq!(keys.closed(n, true, at, &sheet), None);
        // Taken all the same, so a repeated close of that opening cannot act later.
        let at = AtClose {
            shown: &every_module_shown,
            ..at
        };
        assert_eq!(keys.closed(n, true, at, &sheet), None);
    }
}
