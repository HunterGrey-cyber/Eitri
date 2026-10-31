//! The window's chrome drawn from the controller's model ([`ChromeModel`]): the shell keeps the widgets and the
//! model says what they show. A `Chrome` op reads the model, and [`reconcile`] compares it with the one drawn
//! last, field by field, touching only the widgets whose field changed.
//!
//! Every widget is made once and kept: a tray chip is never reparented, the strips and the float bars are the
//! grid's own pooled widgets, and a reconcile only sets text (always as plain text, never markup), visibility and
//! classes. The app name's flash and the toast act once per new value of their serials, so the same refusal twice
//! is two flashes and two toasts, and a model read again unchanged does nothing. A toast whose time is up is left
//! to the toast's own timer, which gives back the standing note it may have covered.
//!
//! The hint panel is the model's `prefix_panel`: its heading, groups and footer go to the panel's view
//! (`gtk_host::prefix_panel`) in one call when the field changed, and nothing takes it down but the field going.
//!
//! The model's `rename` is the top bar's rename entry: opened holding the name (plain text, selected), refilled when
//! the name changes while it is open, hidden and emptied when it closes. The keys reach it through the host's `Focus`
//! op, which comes before the `Chrome` op that fills it.
//!
//! The model's y/n (`prefix x`'s, and the window close's) is the prompt overlay's text: the overlay has no question of
//! its own, so what it shows is always the model's. Where it sits needs the prefix strip's left edge and
//! the top bar's and its own heights, which only the widgets know, so it is placed here from their measures, read
//! once it shows its text (a hidden label measures nothing).

use eitri_core::onboarding::OnboardingView;
use eitri_core::wm::chrome::{Chip, ChromeModel, FloatLabel, StripLabels, Toast};
use eitri_core::wm::close::prompt_origin;
use eitri_core::wm::prefix_panel::PrefixPanel;
use eitri_core::wm::prefix_strip::StripPiece;

/// Sets the top bar's project and workspace names.
type WorkspaceSetter = Box<dyn Fn(&str, Option<&str>)>;
/// Sets one optional text; nothing hides its widget.
type TextSetter = Box<dyn Fn(Option<&str>)>;
/// Names every strip entry.
type StripsSetter = Box<dyn Fn(&[StripLabels])>;
/// Names every float bar.
type FloatBarsSetter = Box<dyn Fn(&[FloatLabel])>;
/// Draws the prefix strip's pieces; nothing takes it down.
type PiecesSetter = Box<dyn Fn(Option<&[StripPiece]>)>;
/// Draws the hint panel, or takes it down.
type PanelSetter = Box<dyn Fn(Option<&PrefixPanel>)>;
/// Draws the first-start screen, or takes it down.
type OnboardingSetter = Box<dyn Fn(Option<&OnboardingView>)>;

/// What the y/n prompt is placed by, read from the shell's widgets once it shows its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct PromptMeasures {
    /// Whether the top bar shows: hidden, the prompt sits at the window's top edge.
    pub(crate) bar_shown: bool,
    /// The prefix strip's left edge, where the prompt starts while the bar shows.
    pub(crate) strip_x: i32,
    /// The top bar's height.
    pub(crate) bar_height: i32,
    /// The prompt's own height, holding its text.
    pub(crate) label_height: i32,
}

/// The y/n prompt overlay, as the model's prompt drives it.
pub(crate) struct PromptWidgets {
    /// Shows the prompt holding `text`, as plain text, where it was last placed.
    pub(crate) show: Box<dyn Fn(&str)>,
    /// What it is placed by, read once it shows.
    pub(crate) measures: Box<dyn Fn() -> PromptMeasures>,
    /// Moves it to its origin in the window.
    pub(crate) place: Box<dyn Fn(i32, i32)>,
    /// Hides it.
    pub(crate) hide: Box<dyn Fn()>,
}

/// The top bar's rename entry, as the model's `rename` drives it. The keys are given to it by a `Focus` op, not here.
pub(crate) struct RenameWidgets {
    /// Shows the entry holding `text` as plain text, selected, so typing replaces it.
    pub(crate) open: Box<dyn Fn(&str)>,
    /// Gives the open entry `text` in place of what it holds, selected.
    pub(crate) refill: Box<dyn Fn(&str)>,
    /// Hides the entry and empties it.
    pub(crate) close: Box<dyn Fn()>,
}

/// What draws each field of a window's chrome: the shell's widgets behind plain setters, so the reconcile runs with
/// no display in its tests.
pub(crate) struct ChromeWidgets {
    /// The top bar's project name and the current workspace's, the workspace's only while there are several.
    pub(crate) workspace: WorkspaceSetter,
    /// The selected container's text; nothing while no container is selected.
    pub(crate) selection: TextSetter,
    /// The marked module's text; nothing while no module is marked.
    pub(crate) mark: TextSetter,
    pub(crate) other: TextSetter,
    /// One flash of the app name.
    pub(crate) flash: Box<dyn Fn()>,
    /// One tray chip as it is now: its text, whether it shows and whether its chat asks for the user.
    pub(crate) chip: Box<dyn Fn(&Chip)>,
    /// Every strip entry's text.
    pub(crate) strips: StripsSetter,
    /// Every float bar's text.
    pub(crate) float_bars: FloatBarsSetter,
    /// The prefix strip's pieces while the prefix waits, the app name's armed block with them; nothing takes both
    /// down.
    pub(crate) prefix_strip: PiecesSetter,
    /// The hint panel while the prefix waits and the model has one; nothing takes it down.
    pub(crate) prefix_panel: PanelSetter,
    /// A new toast.
    pub(crate) toast: Box<dyn Fn(&Toast)>,
    /// Whether a wait has brought the top bar out, where the window's mode hides it.
    pub(crate) top_bar_revealed: Box<dyn Fn(bool)>,
    /// The y/n prompt.
    pub(crate) prompt: PromptWidgets,
    /// The rename entry.
    pub(crate) rename: RenameWidgets,
    /// The first-start screen, or nothing while the model has none. The keys reach it through the host's `Focus` op,
    /// which comes before the `Chrome` op that fills it.
    pub(crate) onboarding: OnboardingSetter,
}

impl ChromeWidgets {
    pub(crate) fn set_other(&self, text: Option<&str>) {
        (self.other)(text);
    }
}

/// Draws `now` on `widgets`, touching only what differs from `last` (everything when nothing was drawn yet), and
/// keeps `now` as the model drawn.
pub(crate) fn reconcile(widgets: &ChromeWidgets, last: &mut Option<ChromeModel>, now: &ChromeModel) {
    let was = last.as_ref();
    let bar = &now.top_bar;
    let old_bar = was.map(|model| &model.top_bar);
    if old_bar.is_none_or(|old| old.project != bar.project || old.workspace != bar.workspace) {
        (widgets.workspace)(&bar.project, bar.workspace.as_deref());
    }
    if old_bar.is_none_or(|old| old.other != bar.other) {
        widgets.set_other(bar.other.as_deref());
    }
    if old_bar.is_none_or(|old| old.selection != bar.selection) {
        (widgets.selection)(bar.selection.as_deref());
    }
    if old_bar.is_none_or(|old| old.mark != bar.mark) {
        (widgets.mark)(bar.mark.as_deref());
    }
    if bar.flash > old_bar.map_or(0, |old| old.flash) {
        (widgets.flash)();
    }
    for chip in &now.tray {
        let before = was.and_then(|model| model.tray.iter().find(|old| old.module == chip.module));
        if before != Some(chip) {
            (widgets.chip)(chip);
        }
    }
    if was.is_none_or(|model| model.strips != now.strips) {
        (widgets.strips)(&now.strips);
    }
    if was.is_none_or(|model| model.float_bars != now.float_bars) {
        (widgets.float_bars)(&now.float_bars);
    }
    if was.is_none_or(|model| model.prefix_strip != now.prefix_strip) {
        (widgets.prefix_strip)(now.prefix_strip.as_deref());
    }
    if was.is_none_or(|model| model.prefix_panel != now.prefix_panel) {
        (widgets.prefix_panel)(now.prefix_panel.as_ref());
    }
    if let Some(toast) = &now.toast {
        let shown = was.and_then(|model| model.toast.as_ref()).map(|old| old.serial);
        if shown != Some(toast.serial) {
            (widgets.toast)(toast);
        }
    }
    let revealed = now.modes.top_bar_revealed;
    if was.map_or(revealed, |model| model.modes.top_bar_revealed != revealed) {
        (widgets.top_bar_revealed)(revealed);
    }
    // After the bar: whether it shows decides where the prompt sits.
    let prompt = now.prompt.as_ref();
    if was.map_or(prompt.is_some(), |model| model.prompt.as_ref() != prompt) {
        let widgets = &widgets.prompt;
        match prompt {
            Some(prompt) => {
                (widgets.show)(&prompt.text);
                let at = (widgets.measures)();
                let (x, y) = prompt_origin(at.bar_shown, at.strip_x, at.bar_height, at.label_height.max(0));
                (widgets.place)(x, y);
            }
            None => (widgets.hide)(),
        }
    }
    let rename = now.rename.as_deref();
    match (was.and_then(|model| model.rename.as_deref()), rename) {
        (None, Some(text)) => (widgets.rename.open)(text),
        (Some(old), Some(text)) if old != text => (widgets.rename.refill)(text),
        (Some(_), None) => (widgets.rename.close)(),
        _ => {}
    }
    // Last: the screen draws over everything else the model shows.
    if was.map_or(now.onboarding.is_some(), |model| model.onboarding != now.onboarding) {
        (widgets.onboarding)(now.onboarding.as_ref());
    }
    *last = Some(now.clone());
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::layout::ModuleId;
    use eitri_core::wm::chrome::{EntryLabel, TopBar};
    use eitri_core::wm::toasts::ToastKind;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// Widgets that write down each setter called, with what it was given.
    fn recording() -> (ChromeWidgets, Rc<RefCell<Vec<String>>>) {
        let heard: Rc<RefCell<Vec<String>>> = Rc::default();
        let say = |heard: &Rc<RefCell<Vec<String>>>| {
            let heard = heard.clone();
            move |line: String| heard.borrow_mut().push(line)
        };
        let widgets = ChromeWidgets {
            workspace: Box::new({
                let say = say(&heard);
                move |project, workspace| say(format!("workspace {project} {workspace:?}"))
            }),
            selection: Box::new({
                let say = say(&heard);
                move |text| say(format!("selection {text:?}"))
            }),
            other: Box::new({
                let say = say(&heard);
                move |text| say(format!("other {text:?}"))
            }),
            mark: Box::new({
                let say = say(&heard);
                move |text| say(format!("mark {text:?}"))
            }),
            flash: Box::new({
                let say = say(&heard);
                move || say("flash".into())
            }),
            chip: Box::new({
                let say = say(&heard);
                move |chip| say(format!("chip {} {} shown={}", chip.module, chip.label, chip.shown))
            }),
            strips: Box::new({
                let say = say(&heard);
                move |strips| say(format!("strips {}", strips.len()))
            }),
            float_bars: Box::new({
                let say = say(&heard);
                move |bars| say(format!("float bars {}", bars.len()))
            }),
            prefix_strip: Box::new({
                let say = say(&heard);
                move |pieces| say(format!("prefix strip {:?}", pieces.map(<[StripPiece]>::len)))
            }),
            prefix_panel: Box::new({
                let say = say(&heard);
                move |panel| say(format!("prefix panel {:?}", panel.map(|panel| panel.groups.len())))
            }),
            toast: Box::new({
                let say = say(&heard);
                move |toast| say(format!("toast {} {}", toast.serial, toast.text))
            }),
            top_bar_revealed: Box::new({
                let say = say(&heard);
                move |revealed| say(format!("revealed {revealed}"))
            }),
            prompt: PromptWidgets {
                show: Box::new({
                    let say = say(&heard);
                    move |text| say(format!("prompt shows {text}"))
                }),
                measures: Box::new(|| MEASURES),
                place: Box::new({
                    let say = say(&heard);
                    move |x, y| say(format!("prompt at {x},{y}"))
                }),
                hide: Box::new({
                    let say = say(&heard);
                    move || say("prompt hidden".into())
                }),
            },
            rename: RenameWidgets {
                open: Box::new({
                    let say = say(&heard);
                    move |text| say(format!("rename opens {text}"))
                }),
                refill: Box::new({
                    let say = say(&heard);
                    move |text| say(format!("rename refills {text}"))
                }),
                close: Box::new({
                    let say = say(&heard);
                    move || say("rename closes".into())
                }),
            },
            onboarding: Box::new({
                let say = say(&heard);
                move |view| say(format!("onboarding {:?}", view.map(|view| view.step.clone())))
            }),
        };
        (widgets, heard)
    }

    /// The double's measures: the bar shown, 40 px high, the strip 100 px in, the prompt 20 px high.
    const MEASURES: PromptMeasures = PromptMeasures {
        bar_shown: true,
        strip_x: 100,
        bar_height: 40,
        label_height: 20,
    };

    fn chip(module: ModuleId, label: &str, shown: bool) -> Chip {
        Chip {
            module,
            label: label.into(),
            shown,
            attention: false,
            stowed: false,
        }
    }

    fn model() -> ChromeModel {
        ChromeModel {
            top_bar: TopBar {
                project: "eitri".into(),
                ..TopBar::default()
            },
            tray: vec![
                chip(ModuleId::editor(), "editor", false),
                chip(ModuleId::agent(), "agent", true),
            ],
            strips: vec![StripLabels {
                entries: vec![EntryLabel {
                    module: ModuleId::terminal(),
                    label: "terminal".into(),
                }],
                active: Some(0),
            }],
            ..ChromeModel::default()
        }
    }

    fn take(heard: &Rc<RefCell<Vec<String>>>) -> Vec<String> {
        std::mem::take(&mut *heard.borrow_mut())
    }

    #[test]
    fn a_peer_summary_changes_only_its_plain_text_label() {
        let (widgets, heard) = recording();
        let mut last = None;
        let mut now = model();
        reconcile(&widgets, &mut last, &now);
        take(&heard);
        now.top_bar.other = Some("2: ⚑1".into());
        reconcile(&widgets, &mut last, &now);
        assert_eq!(take(&heard), [r#"other Some("2: ⚑1")"#]);
        reconcile(&widgets, &mut last, &now);
        assert!(take(&heard).is_empty());
        now.top_bar.other = None;
        reconcile(&widgets, &mut last, &now);
        assert_eq!(take(&heard), ["other None"]);
    }

    #[test]
    fn reconcile_touches_only_the_widgets_whose_field_changed() {
        let (widgets, heard) = recording();
        let mut last = None;
        let first = model();
        reconcile(&widgets, &mut last, &first);
        assert_eq!(
            take(&heard),
            [
                "workspace eitri None",
                "other None",
                "selection None",
                "mark None",
                "chip editor editor shown=false",
                "chip agent agent shown=true",
                "strips 1",
                "float bars 0",
                "prefix strip None",
                "prefix panel None",
            ],
            "the first draw sets every widget once, and neither flashes nor toasts"
        );
        reconcile(&widgets, &mut last, &first);
        assert!(take(&heard).is_empty(), "the same model again draws nothing");

        let mut marked = first.clone();
        marked.top_bar.mark = Some("marked: agent".into());
        reconcile(&widgets, &mut last, &marked);
        assert_eq!(take(&heard), [r#"mark Some("marked: agent")"#]);

        let mut asking = marked.clone();
        asking.tray[1].label = "agent ⚑1".into();
        reconcile(&widgets, &mut last, &asking);
        assert_eq!(
            take(&heard),
            ["chip agent agent ⚑1 shown=true"],
            "only the chip that changed"
        );

        let mut armed = asking.clone();
        armed.prefix_strip = Some(vec![StripPiece::Heading("modules".into())]);
        armed.modes.top_bar_revealed = true;
        reconcile(&widgets, &mut last, &armed);
        assert_eq!(take(&heard), ["prefix strip Some(1)", "revealed true"]);
        reconcile(&widgets, &mut last, &asking);
        assert_eq!(take(&heard), ["prefix strip None", "revealed false"]);
    }

    #[test]
    fn the_prompt_field_shows_places_and_hides_the_overlay() {
        let (widgets, heard) = recording();
        let mut last = None;
        reconcile(&widgets, &mut last, &model());
        assert!(
            !take(&heard).iter().any(|line| line.starts_with("prompt")),
            "no prompt, nothing to hide on the first draw"
        );
        let mut asking = model();
        asking.prompt = Some(eitri_core::wm::chrome::Prompt {
            text: "kill-pane terminal? (y/n)".into(),
            closes_window: false,
            offers_move: false,
        });
        reconcile(&widgets, &mut last, &asking);
        let (x, y) = prompt_origin(true, 100, 40, 20);
        assert_eq!(
            take(&heard),
            [
                "prompt shows kill-pane terminal? (y/n)".to_string(),
                format!("prompt at {x},{y}")
            ],
            "shown once with its text, then placed where the measures put it"
        );
        reconcile(&widgets, &mut last, &asking);
        assert!(take(&heard).is_empty(), "the same prompt again draws nothing");
        reconcile(&widgets, &mut last, &model());
        assert_eq!(take(&heard), ["prompt hidden"]);
    }

    #[test]
    fn the_rename_field_opens_fills_and_closes_the_entry() {
        let (widgets, heard) = recording();
        let mut last = None;
        reconcile(&widgets, &mut last, &model());
        assert!(
            !take(&heard).iter().any(|line| line.starts_with("rename")),
            "no rename, nothing to close on the first draw"
        );
        let renaming = |text: &str| ChromeModel {
            rename: Some(text.into()),
            ..model()
        };
        reconcile(&widgets, &mut last, &renaming("<b>study</b>"));
        assert_eq!(
            take(&heard),
            ["rename opens <b>study</b>"],
            "opened once, holding the name as it is"
        );
        reconcile(&widgets, &mut last, &renaming("<b>study</b>"));
        assert!(take(&heard).is_empty(), "an unchanged field draws nothing");
        reconcile(&widgets, &mut last, &renaming("review"));
        assert_eq!(take(&heard), ["rename refills review"]);
        reconcile(&widgets, &mut last, &model());
        assert_eq!(take(&heard), ["rename closes"]);
        reconcile(&widgets, &mut last, &model());
        assert!(take(&heard).is_empty(), "a closed entry is not closed again");

        let (widgets, heard) = recording();
        let mut last = None;
        reconcile(&widgets, &mut last, &renaming("study"));
        assert!(
            take(&heard).contains(&"rename opens study".to_string()),
            "a first draw with the entry open opens it"
        );
    }

    #[test]
    fn the_first_start_screen_is_drawn_when_its_view_changes_and_taken_down_with_it() {
        use eitri_core::onboarding::OnboardingView;
        let view = |step: &str| OnboardingView {
            title: "Set up".into(),
            step: step.into(),
            rows: Vec::new(),
            question: None,
            items: Vec::new(),
            body: Vec::new(),
            status: Vec::new(),
            footer: String::new(),
            buttons: Vec::new(),
            help: None,
        };
        let shown = |step: &str| ChromeModel {
            onboarding: Some(view(step)),
            ..model()
        };
        let (widgets, heard) = recording();
        let mut last = None;
        reconcile(&widgets, &mut last, &model());
        assert!(
            !take(&heard).iter().any(|line| line.starts_with("onboarding")),
            "no screen, nothing to take down on the first draw"
        );
        reconcile(&widgets, &mut last, &shown("1 / 2"));
        assert_eq!(take(&heard), ["onboarding Some(\"1 / 2\")"]);
        reconcile(&widgets, &mut last, &shown("1 / 2"));
        assert!(take(&heard).is_empty(), "an unchanged view draws nothing");
        reconcile(&widgets, &mut last, &shown("2 / 2"));
        assert_eq!(take(&heard), ["onboarding Some(\"2 / 2\")"]);
        reconcile(&widgets, &mut last, &model());
        assert_eq!(take(&heard), ["onboarding None"]);
    }

    #[test]
    fn a_new_toast_serial_shows_the_toast_once_and_an_old_one_never() {
        let (widgets, heard) = recording();
        let mut last = None;
        reconcile(&widgets, &mut last, &model());
        take(&heard);
        let toast = |serial: u64| Toast {
            text: "no last module".into(),
            kind: ToastKind::Other,
            serial,
        };
        let mut refused = model();
        refused.top_bar.flash = 1;
        refused.toast = Some(toast(1));
        reconcile(&widgets, &mut last, &refused);
        assert_eq!(take(&heard), ["flash", "toast 1 no last module"]);
        reconcile(&widgets, &mut last, &refused);
        assert!(take(&heard).is_empty(), "an old serial is never shown again");

        let mut gone = refused.clone();
        gone.toast = None;
        reconcile(&widgets, &mut last, &gone);
        assert!(take(&heard).is_empty(), "the toast's own timer takes it down");

        let mut again = refused.clone();
        again.top_bar.flash = 2;
        again.toast = Some(toast(2));
        reconcile(&widgets, &mut last, &again);
        assert_eq!(
            take(&heard),
            ["flash", "toast 2 no last module"],
            "the same words again are a new toast"
        );
    }
}
