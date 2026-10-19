//! The hosts the probe adds to the grid beside the product's own, the editor's buffer the probe edits, and
//! the page reads that need a script run inside a page.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use gtk4::glib;
use gtk4::prelude::*;
use webkit6::prelude::*;
use webkit6::WebView;

use eitri_core::layout::ModuleId;

use super::{say, Probe};
use crate::agent_panel::AgentPanelHandle;
use crate::module_grid::HostKind;
use crate::terminal::TerminalPane;
use crate::web_host::WebHost;

/// Makes the probe's buffer one nothing asks about on exit, and its screen the same every time: no
/// blinking cursor, no intro, no mode or ruler text that would differ between two identical states.
pub(super) const SETUP_LUA: &str = "vim.o.guicursor = 'a:block-blinkon0' \
    vim.o.shortmess = vim.o.shortmess .. 'I' \
    vim.o.showmode = false \
    vim.o.ruler = false \
    vim.o.showcmd = false \
    vim.o.number = false \
    vim.o.wrap = false \
    vim.bo.buftype = 'nofile' \
    vim.bo.bufhidden = 'hide' \
    vim.bo.swapfile = false";

/// Replaces the whole buffer with the lines it is given (one argument: a list of strings).
pub(super) const SET_LINES_LUA: &str = "vim.api.nvim_buf_set_lines(0, 0, -1, false, (...))";

/// The page of the static web host: a counter a reload would reset, and a token a reload would change.
const STATIC_PAGE: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><style>\
html,body{margin:0;background:#264653;color:#e9c46a;font:24px monospace}\
#t{position:absolute;left:12px;top:8px}</style></head><body><div id=\"t\">STATIC PAGE</div>\
<script>window.__token=Math.random().toString(36).slice(2);window.__tick=0;\
setInterval(function(){window.__tick++;},100);</script></body></html>";

/// Every page and terminal the probe made, kept alive for the run.
#[derive(Default)]
pub(super) struct Made {
    pub(super) float_term: Option<TerminalPane>,
    pub(super) static_view: Option<WebView>,
    pub(super) control_chat: Option<WebView>,
    pub(super) pool_terms: Vec<(ModuleId, TerminalPane)>,
    pub(super) pool_chat: Option<(ModuleId, WebView, AgentPanelHandle)>,
    pub(super) extra_chats: Vec<(ModuleId, WebView, AgentPanelHandle)>,
}

impl Made {
    pub(super) fn ids(&self) -> Vec<ModuleId> {
        let mut ids = Vec::new();
        if self.float_term.is_some() {
            ids.push(float_term_id());
        }
        if self.static_view.is_some() {
            ids.push(static_id());
        }
        ids.extend(self.pool_terms.iter().map(|(id, _)| id.clone()));
        ids.extend(self.pool_chat.iter().map(|(id, _, _)| id.clone()));
        ids.extend(self.extra_chats.iter().map(|(id, _, _)| id.clone()));
        ids
    }
}

pub(super) fn float_term_id() -> ModuleId {
    ModuleId::lua("probe-float-term")
}

pub(super) fn static_id() -> ModuleId {
    ModuleId::lua("probe-static")
}

/// Thirty lines that differ in their first characters from one `k` to the next, so two different `k` are
/// two different pictures in the left half of the editor as well as the right.
pub(super) fn content_lines(k: usize) -> Vec<String> {
    (0..30)
        .map(|i| {
            let filler = "abcdefghij".repeat(2 + (i + k) % 4);
            format!("K{k} L{i:02} {filler}")
        })
        .collect()
}

impl Probe {
    /// Waits for the editor, counts what the product's own hosts do, makes the probe's hosts. `false` if
    /// the editor never came up.
    pub(super) async fn setup(&self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !self.ctx.pane.is_running() {
            if Instant::now() > deadline {
                return false;
            }
            self.sleep(50).await;
        }
        for (id, host) in self.ctx.grid.hosts() {
            self.track(id.as_str(), &host);
            if let Some(target) = self.ctx.grid.focus_target(&id) {
                if target != host {
                    self.track(&format!("{id}-focus"), &target);
                }
            }
        }
        if !self.setup_editor().await {
            say("warn the editor refused the setup chunk");
        }
        self.edit(0).await;
        if let Some(chat) = self.chat_view() {
            self.wait_loaded(&chat).await;
            chat.connect_web_process_terminated(|_, reason| say(format!("web-process-terminated chat {reason:?}")));
        }
        self.make_float_terminal();
        self.make_static_page().await;
        self.sleep(400).await;
        self.release_all();
        self.sleep(300).await;
        self.web_state("start").await;
        self.snapshot("start");
        true
    }

    pub(super) fn web_ok(&self) -> bool {
        self.chat_view().is_some()
    }

    /// The base chat page's `WebView`, `None` where WebKit cannot run and a notice stands in for it.
    pub(super) fn chat_view(&self) -> Option<WebView> {
        self.ctx.agent.clone().downcast::<WebView>().ok()
    }

    fn make_float_terminal(&self) {
        let terminal = TerminalPane::new(self.ctx.project_root.clone());
        self.ctx
            .grid
            .add(float_term_id(), terminal.widget(), terminal.widget(), HostKind::Direct);
        self.track("probe-float-term", terminal.widget().upcast_ref());
        self.hide(&float_term_id());
        self.made.borrow_mut().float_term = Some(terminal);
    }

    async fn make_static_page(&self) {
        if !self.web_ok() {
            say("skip the static page: no WebKit in this process");
            return;
        }
        let view = WebView::new();
        view.set_hexpand(true);
        view.set_vexpand(true);
        view.load_html(STATIC_PAGE, None);
        view.connect_web_process_terminated(|_, reason| say(format!("web-process-terminated static {reason:?}")));
        let host = WebHost::new(&view);
        self.ctx.grid.add(static_id(), &host, &view, HostKind::Web);
        self.track("probe-static", host.upcast_ref());
        self.hide(&static_id());
        self.made.borrow_mut().static_view = Some(view.clone());
        // A hidden view may not load until it is shown: show it once somewhere small.
        self.place(
            &static_id(),
            eitri_core::layout::Rect {
                x: 0,
                y: 0,
                w: 200,
                h: 120,
            },
        );
        self.wait_loaded(&view).await;
    }

    pub(super) async fn wait_loaded(&self, view: &WebView) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while view.is_loading() || view.uri().is_none() {
            if Instant::now() > deadline {
                say("warn a page did not finish loading in 30 s");
                return;
            }
            self.sleep(50).await;
        }
    }

    // ---- the editor's buffer ----------------------------------------------------------------------

    async fn setup_editor(&self) -> bool {
        let (tx, rx) = futures_channel::oneshot::channel::<Result<(), String>>();
        let sent = self.ctx.pane.exec_lua(
            SETUP_LUA,
            Vec::new(),
            Box::new(move |reply| {
                let _ = tx.send(reply.map(|_| ()));
            }),
        );
        sent && self.reply(rx).await
    }

    /// Replaces the editor's buffer with `content_lines(k)`. `false` if nvim refused or never answered.
    pub(super) async fn edit(&self, k: usize) -> bool {
        let lines: Vec<rmpv::Value> = content_lines(k).into_iter().map(rmpv::Value::from).collect();
        let (tx, rx) = futures_channel::oneshot::channel::<Result<(), String>>();
        let sent = self.ctx.pane.exec_lua(
            SET_LINES_LUA,
            vec![rmpv::Value::Array(lines)],
            Box::new(move |reply| {
                let _ = tx.send(reply.map(|_| ()));
            }),
        );
        if !sent {
            say(format!("warn edit {k} refused: nvim is not running"));
            return false;
        }
        self.reply(rx).await
    }

    async fn reply(&self, mut rx: futures_channel::oneshot::Receiver<Result<(), String>>) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match rx.try_recv() {
                Ok(Some(Ok(()))) => return true,
                Ok(Some(Err(message))) => {
                    say(format!("warn nvim answered with an error: {message}"));
                    return false;
                }
                Ok(None) if Instant::now() < deadline => self.sleep(3).await,
                _ => {
                    say("warn nvim did not answer in 5 s");
                    return false;
                }
            }
        }
    }

    // ---- terminals -------------------------------------------------------------------------------

    /// Types `text` into `terminal`'s shell as a paste (no key events, nothing synthetic).
    pub(super) fn type_into(&self, terminal: &TerminalPane, text: &str) {
        terminal.send(terminal_input::NormalizedInput::Paste {
            text: text.to_owned(),
            bracketed: false,
        });
    }

    // ---- pages ----------------------------------------------------------------------------------

    /// Runs `script` in `view` and returns its result as text, `None` if it failed or took over 5 s.
    pub(super) async fn js(&self, view: &WebView, script: &str) -> Option<String> {
        let slot: std::rc::Rc<RefCell<Option<Option<String>>>> = std::rc::Rc::new(RefCell::new(None));
        {
            let slot = slot.clone();
            let view = view.clone();
            let script = script.to_owned();
            glib::spawn_future_local(async move {
                let result = match view.evaluate_javascript_future(&script, None, None).await {
                    Ok(value) => Some(value.to_str().to_string()),
                    Err(error) => {
                        say(format!("warn script failed: {error}"));
                        None
                    }
                };
                *slot.borrow_mut() = Some(result);
            });
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(result) = slot.borrow_mut().take() {
                return result;
            }
            if Instant::now() > deadline {
                say("warn a script did not answer in 5 s");
                return None;
            }
            self.sleep(5).await;
        }
    }

    /// `webstate <label> static=<token>:<tick> chat=<time origin>`: what a reload or a restart of either
    /// page would change. The runner checks that the token and the origin never change and the tick only
    /// grows.
    pub(super) async fn web_state(&self, label: &str) {
        let view = self.made.borrow().static_view.clone();
        let static_state = match view {
            Some(view) => self
                .js(&view, "window.__token + ':' + window.__tick")
                .await
                .unwrap_or_else(|| "error".to_owned()),
            None => "none".to_owned(),
        };
        let chat_state = match self.chat_view() {
            Some(view) => self
                .js(&view, "String(performance.timeOrigin)")
                .await
                .unwrap_or_else(|| "error".to_owned()),
            None => "none".to_owned(),
        };
        say(format!("webstate {label} static={static_state} chat={chat_state}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_contents_differ_in_their_first_characters_on_every_line() {
        let (a, b) = (content_lines(0), content_lines(1));
        assert_eq!(a.len(), 30);
        for (x, y) in a.iter().zip(&b) {
            assert_ne!(x[..2], y[..2], "{x} / {y}");
        }
    }

    #[test]
    fn the_buffer_chunks_are_constants_the_editor_scan_accepts() {
        for chunk in [SETUP_LUA, SET_LINES_LUA] {
            assert!(!chunk.contains('\n'), "a chunk is one line");
        }
    }
}
