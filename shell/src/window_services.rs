use super::*;

thread_local! {
    /// The timer that ends the refusal flash now showing, if one is ([`show_flash`]).
    static REFUSAL_ENDS: std::cell::RefCell<Option<glib::SourceId>> = const { std::cell::RefCell::new(None) };
}

/// The top bar's app name shows the prefix indicator's block for [`REFUSAL_FLASH`]: one flash the
/// chrome model asked for. Its own class, so a prefix armed meanwhile keeps its indicator; a second flash inside
/// one restarts it rather than being cut short by the first one's timer. One window per process (`NON_UNIQUE`),
/// so one timer per thread.
pub(crate) fn show_flash(app_name: &gtk4::Label) {
    app_name.add_css_class("refused");
    let app_name = app_name.clone();
    let ends = glib::timeout_add_local_once(REFUSAL_FLASH, move || {
        REFUSAL_ENDS.with(|ends| ends.borrow_mut().take());
        app_name.remove_css_class("refused");
    });
    if let Some(earlier) = REFUSAL_ENDS.with(|slot| slot.borrow_mut().replace(ends)) {
        earlier.remove();
    }
}

/// An activation token from `window`'s display, for the other window of the pair to come forward with: GDK asks the
/// compositor for one naming the surface with the keys and the seat's last input. `None` when none is given.
pub(crate) fn activation_token(window: &glib::WeakRef<ApplicationWindow>) -> Option<String> {
    let window = window.upgrade()?;
    let context = WidgetExt::display(&window).app_launch_context();
    context
        .startup_notify_id(None::<&gtk4::gio::AppInfo>, &[])
        .map(|token| token.to_string())
        .filter(|token| !token.is_empty())
}

/// What a plugin's nvim commands do to its module in this window, as the controller decides them through its runner's
/// gates: a show puts it on screen (opening a start-hidden one by its rule the first time) with the keys where they
/// are, a hide sends the keys where any hide sends them. Whether it is shown, and on screen, is read from the
/// controller too.
pub(crate) struct GridFollowWindow {
    pub(crate) driver: std::rc::Weak<eitri_core::wm::Driver<eitri_core::wm::Controller>>,
}

impl GridFollowWindow {
    fn shown(&self, module: &ModuleId) -> (bool, bool) {
        self.driver.upgrade().map_or((false, false), |driver| {
            driver.query(|c| c.module_shown(WinId::MAIN, module))
        })
    }

    fn follow(&self, module: &ModuleId, show: bool) {
        if let Some(driver) = self.driver.upgrade() {
            driver.follow(WinId::MAIN, module, show);
        }
    }
}

impl plugin_panels::FollowWindow for GridFollowWindow {
    fn is_shown(&self, module: &ModuleId) -> bool {
        self.shown(module).0
    }

    fn is_on_screen(&self, module: &ModuleId) -> bool {
        self.shown(module).1
    }

    fn show(&self, module: &ModuleId) {
        self.follow(module, true);
    }

    fn hide(&self, module: &ModuleId) {
        self.follow(module, false);
    }

    fn command(&self, module: &ModuleId, cmd: eitri_core::plugin::follow_feed::FollowCmd) {
        if let Some(driver) = self.driver.upgrade() {
            driver.follow_cmd(WinId::MAIN, module, cmd);
        }
    }
}

/// Adds the panel's effects and read-only facts to the window's module services.
/// A move's answer from the panel, as the controller hears it: a frozen tab's descriptors become the handoff the
/// link sends unread.
pub(crate) fn chat_move_event(win: WinId, event: agent_panel::ChatMoveEvent) -> Option<ChatEvent> {
    use agent_panel::ChatMoveEvent as Panel;
    use eitri_core::window_link::moves::MoveId;
    Some(match event {
        Panel::Frozen { view, mv, record, fds } => ChatEvent::Frozen {
            win,
            view,
            mv: MoveId(mv),
            record,
            handoff: gtk_host::peer::handoff_of(fds),
        },
        Panel::FreezeRefused { view, mv, why } => ChatEvent::FreezeRefused {
            win,
            view,
            mv: MoveId(mv),
            why,
        },
        Panel::PresenceHeld { mv } => ChatEvent::PresenceHeld { win, mv: MoveId(mv) },
        Panel::PresenceRefused { mv, why } => ChatEvent::PresenceRefused {
            win,
            mv: MoveId(mv),
            why,
        },
        Panel::LastTabClosed => ChatEvent::TabLeft { win },
        Panel::HandOver(_) => return None,
    })
}

pub(crate) struct PanelServices {
    pub(crate) cm_session: Option<(Rc<RefCell<linked_window::focus::CmSession>>, gtk4::Application)>,
    pub(crate) inner: gtk_host_services::Services,
}

impl gtk_host::ServiceSet for PanelServices {
    fn page_built(&self, module: &ModuleId) -> bool {
        self.inner.page_built(module)
    }

    fn view_of_tab(&self, tab: eitri_core::tabs::TabId) -> Option<ViewId> {
        self.inner.view_of_tab(tab)
    }
    fn tab_moving(&self, tab: eitri_core::tabs::TabId) -> bool {
        self.inner.tab_moving(tab)
    }
    fn tab_can_step(&self, delta: i32) -> bool {
        self.inner.tab_can_step(delta)
    }
    fn tab_has_last(&self) -> bool {
        self.inner.tab_has_last()
    }
    fn tab_place(&self, n: u16) -> Option<eitri_core::tab_set::TabPlace> {
        self.inner.tab_place(n)
    }
    fn plugin_keys_help(&self, module: &ModuleId) -> Option<eitri_core::plugin::keys::PluginKeysHelp> {
        self.inner
            .plugin_panels
            .declared_rows(module)
            .map(|(title, rows)| eitri_core::plugin::keys::plugin_help(&title, rows))
    }
    fn chat_views(&self) -> Vec<ViewId> {
        self.inner.agent.views()
    }
    fn can_break_out(&self) -> Result<(), String> {
        self.inner.agent.can_break_out()
    }
    fn chat_move(&self, view: ViewId, session: Option<&str>) -> Result<eitri_core::wm::ChatMoveFacts, String> {
        self.inner.agent.chat_move_facts(view, session)
    }
    fn running_chats(&self) -> Vec<(ModuleId, Option<eitri_core::tabs::TabId>)> {
        self.inner.agent.running_chats()
    }
    fn terminal_job_modules(&self) -> Vec<ModuleId> {
        self.inner.terminal_job_modules()
    }
    fn pages_with_tasks(&self) -> Vec<ModuleId> {
        self.inner.pages_with_tasks()
    }
    fn base_tabs(&self) -> Vec<eitri_core::tabs::TabId> {
        self.inner.agent.base_tabs()
    }
    fn active_base_tab(&self) -> Option<eitri_core::tabs::TabId> {
        self.inner.agent.active_base_tab()
    }
    fn arrived_tab(&self, mv: eitri_core::window_link::moves::MoveId) -> Option<eitri_core::tabs::TabId> {
        self.inner.agent.arrived_tab(mv.0)
    }
    fn tab_move(&self, tab: eitri_core::tabs::TabId) -> Result<eitri_core::wm::ChatMoveFacts, String> {
        self.inner.agent.tab_move_facts(tab)
    }
    fn tab_hand_over(&self, tab: eitri_core::tabs::TabId) -> Result<eitri_core::wm::TabHandOver, String> {
        self.inner.agent.tab_hand_over_facts(tab)
    }
    fn moved_tab_key(&self, tab: eitri_core::tabs::TabId) -> Option<eitri_core::window_link::base_tabs::MovedTabKey> {
        self.inner.agent.moved_tab_key(tab)
    }
    fn order_base_tabs(
        &self,
        tabs: &[eitri_core::tabs::TabId],
        active: Option<eitri_core::tabs::TabId>,
    ) -> Result<(), String> {
        self.inner.agent.order_base_tabs(tabs, active)
    }
    fn view_of_session(&self, session: &str) -> Option<ViewId> {
        self.inner.agent.view_of_session(session)
    }
    fn terminal_jobs(&self) -> usize {
        self.inner.terminal_jobs()
    }
    fn cleanup(&self, op: &eitri_core::wm::CleanupOp) -> Result<(), String> {
        if matches!(op, eitri_core::wm::CleanupOp::PairOnClose { .. }) {
            if let Some((session, app)) = &self.cm_session {
                session.borrow_mut().finish(app);
            }
        }
        self.inner.cleanup(op)
    }
    fn pair(&self, op: &eitri_core::wm::PairOp) -> Result<(), String> {
        self.inner.pair(op)
    }
    fn editor(&self, op: &eitri_core::wm::EditorCall) -> Result<(), String> {
        self.inner.editor(op)
    }
    fn open_answer(&self, id: u64, accepted: bool) -> Result<(), String> {
        self.inner.open_answer(id, accepted)
    }
    fn restart_editor(&self, spawn: &eitri_core::wm::EditorSpawn) -> Result<(), gtk_host::RestartRefused> {
        self.inner.restart_editor(spawn)
    }
    fn terminal(&self, op: &eitri_core::wm::TermOp) -> Result<(), String> {
        self.inner.terminal(op)
    }
    fn plugin(&self, op: &eitri_core::wm::PluginOp) -> Result<(), String> {
        self.inner.plugin(op)
    }
    fn chat(&self, op: &eitri_core::wm::ChatOp) -> Result<(), String> {
        use eitri_core::wm::ChatOp;
        let agent = &self.inner.agent;
        match op {
            // A page closes its keys sheet, its `:` line and a tab's details whenever it hears where the keys
            // are, and when the batch moved the keys into this chat, that news is the host's own focus report,
            // which reaches the page only once the batch has been carried out. Sent now, the sheet, the line or
            // the details would be shut by it at once; sent from an idle, they land after the page knows it has
            // the keys.
            ChatOp::OpenKeymap {
                view, plugin, opening, ..
            } => {
                let (agent, view, plugin, opening) = (agent.clone(), *view, plugin.clone(), *opening);
                gtk4::glib::idle_add_local_once(move || agent.open_keymap(view, plugin, opening));
            }
            ChatOp::CommandLine { view, .. } => {
                let (agent, view) = (agent.clone(), *view);
                gtk4::glib::idle_add_local_once(move || agent.open_command_line(view));
            }
            // The tab's details popover is shut by the same news, so it opens from an idle too; every other tab
            // call is carried out at once.
            ChatOp::Tab {
                call: eitri_core::wm::verbs::tabs::TabCall::Info(view),
                ..
            } => {
                let (agent, view) = (agent.clone(), *view);
                gtk4::glib::idle_add_local_once(move || agent.open_detail(view));
            }
            ChatOp::Reload { view, .. } => agent.reload_document_by_hand(*view),
            ChatOp::JoinBack { view, .. } => match agent.join_back(*view) {
                Ok(_) => {
                    agent.arrive(ViewId::BASE);
                    println!("[chat] {}: its tab joined the base view", view.module());
                }
                Err(text) => return Err(text),
            },
            _ => return self.inner.chat(op),
        }
        Ok(())
    }
}

/// The floats over the chat view `view` now, bottom first (`ModuleGrid::covered_now`): each view is
/// judged by what lies over its own module, so a float over one view leaves every other view clear.
/// With no float in the layout there is nothing to arrange for: every divider drag asks this.
pub(crate) fn view_covering(grid: &ModuleGrid, layout: &eitri_core::layout::Layout, view: ViewId) -> Vec<ModuleId> {
    covering_of_view(layout, view, &|id| grid.covered_now(id))
}

/// [`view_covering`] over any way of naming the floats over a module, so it can be judged without a
/// window. A view the layout does not hold has nothing over it.
fn covering_of_view(
    layout: &eitri_core::layout::Layout,
    view: ViewId,
    covered_now: &dyn Fn(&ModuleId) -> Vec<ModuleId>,
) -> Vec<ModuleId> {
    let chat = view.module();
    if layout.floats().is_empty() || !layout.contains(&chat) {
        return Vec::new();
    }
    covered_now(&chat)
}

/// How a card's toast names the way back to chat view `view`: the key bound to its module (`C-b a` for
/// the base view), else its chip, which is reachable from the top bar whenever the view is off screen.
pub(crate) fn view_way_back(keymap: &eitri_core::keymap::Keymap, view: ViewId, title: &str) -> Option<String> {
    let key = keymap
        .keys_for(&Action::Module(view.module()))
        .first()
        .map(|key| format!("{} {}", keymap.prefix().human(), key.human()));
    match key {
        Some(key) => Some(key),
        None if view.is_base() => None,
        None => Some(format!("the {title} chip")),
    }
}

/// The window's `select.number` digits and HINT's toggle, as the controller's `Hint` ops reach them: the digits
/// drawn over every module's host in the layout's tree order, a chat whose page is a notice included.
pub(crate) struct ShellHints {
    pub(crate) coordinator: Rc<hint::HintCoordinator>,
    pub(crate) grid: glib::WeakRef<ModuleGrid>,
}

impl gtk_host::HintPort for ShellHints {
    fn show_digits(&self, _win: WinId, digits: &[(ModuleId, u8)]) {
        let Some(grid) = self.grid.upgrade() else { return };
        let plan: Vec<(char, ModuleId)> = digits
            .iter()
            .filter_map(|(id, value)| Some((char::from_digit(u32::from(*value), 10)?, id.clone())))
            .collect();
        let drawn = self.coordinator.show_numbers(&plan, &grid.hosts_in_tree_order());
        println!("[prefix] select.number: {drawn} drawn");
    }
    fn hide_digits(&self, _win: WinId) {
        self.coordinator.hide_numbers();
    }
    fn toggle(&self, _win: WinId, keycode: u32) {
        self.coordinator.toggle_from_prefix(keycode);
    }
}

impl gtk_host::EditorReads for NeovideEditorPane {
    fn facts(&self) -> eitri_core::wm::EditorFacts {
        kill_pane::editor_facts(
            self.is_running(),
            self.nvim_exited(),
            self.nvim_exit_pending(),
            self.is_starting(),
            self.watched_call().as_ref(),
        )
    }
    fn cell(&self) -> Option<(f64, f64)> {
        self.cell_size()
    }
    fn running(&self) -> bool {
        self.is_running()
    }
}

/// The chat view the keys are in, from the layout's focus (the module that last held them); the base
/// view's when that is no chat view.
pub(crate) fn keys_chat_view(layout: &Layout) -> ViewId {
    ViewId::of_module(layout.focus()).unwrap_or(ViewId::BASE)
}

/// What a request for a new chat view says where no `WebView` can be built in this process.
pub(crate) const CHAT_VIEWS_NEED_WEBKIT: &str = "chat views need WebKit, which cannot start here";

/// Every extra chat view (`agent#2`..`agent#9`) any of `layouts` holds, ascending and once each.
pub(crate) fn extra_chat_views(layouts: &[&eitri_core::layout::Layout]) -> Vec<ModuleId> {
    let mut ids: Vec<ModuleId> = layouts
        .iter()
        .flat_map(|layout| layout.modules())
        .filter(|id| id.kind() == ModuleKind::Agent && id.number() >= 2)
        .collect();
    ids.sort();
    ids.dedup();
    ids
}
