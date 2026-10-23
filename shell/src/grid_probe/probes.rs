//! Grid probes and their calibration loop. Each probe logs measurements in `counts`,
//! `webstate`, `sync`, and result lines. The runner reads the log and checks pass conditions;
//! the probes do not judge their own results.

use std::cell::Cell;
use std::rc::Rc;

use gtk4::prelude::*;
use webkit6::prelude::*;
use webkit6::WebView;

use eitri_core::layout::{ModuleId, Rect};

use super::hosts::{float_term_id, static_id};
use super::{say, Probe};
use crate::agent_panel::{AgentPanelHandle, ViewId};
use crate::module_grid::HostKind;
use crate::terminal::TerminalPane;
use crate::web_host::WebHost;
use eitri_core::layout::Axis;

/// The pooled page's view number in the window's panel. Keep it outside 2 through 4,
/// which the view lifecycle and second-chat probes use for the window's own views and pages.
fn pool_view() -> ViewId {
    ViewId::new(9).expect("9 is a chat view's number")
}

/// What a page posts to say its document is ready: the same message the panel's own document sends, so
/// the panel answers it as it answers that one, and logs the view it came from.
const POST_READY: &str =
    "window.webkit.messageHandlers.eitriAgent.postMessage(JSON.stringify({type:'ready',request_id:'probe'})); 'posted'";

/// Whether a page holds the blank document an idle chat page is given in place of the panel.
const SHOWS_IDLE_DOCUMENT: &str = "String(document.documentElement.dataset.eitriIdle === '1')";

/// A position that goes 0 to `range` and back, `period` steps each way.
fn tri(step: u64, period: u64, range: i32) -> i32 {
    let period = period.max(1);
    let at = step % (2 * period);
    let along = if at < period { at } else { 2 * period - at };
    ((along as i64 * range.max(0) as i64) / period as i64) as i32
}

/// What a shell is told to print so that a terminal's picture is the same every time: clear, one line
/// naming `k`, the terminal's size (a 0x0 pty would show here).
fn marker(k: usize) -> String {
    format!("printf '\\033[H\\033[2J'; echo MARK-{k}; stty size\n")
}

/// Like [`marker`], without the size line: the pty is resized on purpose when it is allocated past the
/// grid's edge.
fn plain_marker(k: usize) -> String {
    format!("printf '\\033[H\\033[2J'; echo MARK-{k}\n")
}

const MOVE_PERIOD_MS: u64 = 10;

impl Probe {
    fn editor_id() -> ModuleId {
        ModuleId::editor()
    }

    /// The rectangle a base module's host has now, in the grid's coordinates.
    fn tile_of(&self, id: &ModuleId) -> Rect {
        self.host(id).map(|w| self.bounds_of(&w)).unwrap_or_default()
    }

    fn float_terminal(&self) -> Option<TerminalPane> {
        self.made.borrow().float_term.clone()
    }

    // ---- calibration ------------------------------------------------------------------------------

    /// Matches the overlap and tab-switching probes' hide/show count, pace, editor edits, and
    /// page reads in the same process. Cost comparisons divide by this calibration baseline.
    pub(super) async fn calibration(&self) {
        let (gw, gh) = self.grid_size();
        let tf = float_term_id();
        let place_at = Rect {
            x: gw * 3 / 4,
            y: gh / 2,
            w: (gw / 4 - 20).max(120),
            h: (gh / 4).max(100),
        };
        if let Some(terminal) = self.float_terminal() {
            self.place(&tf, place_at);
            terminal.start();
            self.sleep(800).await;
            let n = self.script.count("cal_p1", 1000);
            self.sample("cal.p1.begin").await;
            for i in 0..n {
                if i % 2 == 0 {
                    self.hide(&tf);
                } else {
                    self.place(&tf, place_at);
                }
                if i % 10 == 0 {
                    self.edit((i / 10 % 3) as usize).await;
                }
                self.sleep(MOVE_PERIOD_MS).await;
            }
            self.sample("cal.p1.end").await;
            self.hide(&tf);
        }
        // Hide and show the chat page with the tab-switching probe's waits, edits, and page read,
        // but without switching to editor or terminal tabs. Use the whole grid, matching the
        // tabs' rectangle: a smaller page costs less to lay out and paint and would inflate the
        // measured cost of tab switching.
        let agent = ModuleId::agent();
        let tile = Rect {
            x: 0,
            y: 0,
            w: gw,
            h: gh,
        };
        let chat = self.chat_view();
        let n = self.script.count("cal_p2", 500);
        self.sample("cal.p2.begin").await;
        for i in 0..n {
            self.hide(&agent);
            self.edit((i % 3) as usize).await;
            self.sleep(60 + 130 + 100 + if i % 10 == 0 { 250 } else { 0 }).await;
            self.place(&agent, tile);
            self.sleep(100).await;
            if let Some(chat) = &chat {
                self.js(chat, "innerWidth + 'x' + innerHeight").await;
            }
        }
        self.sample("cal.p2.end").await;
        self.place(&agent, tile);
        self.sleep(300).await;
    }

    // ---- A GL host overlapped by two floats ---------------------------------------------------

    pub(super) async fn p1_overlap(&self) {
        let moves = self.script.count("p1_moves", 1000);
        let editor = Self::editor_id();
        let e = self.tile_of(&editor);
        self.say_geom("p1.editor", e);
        let we = self.in_window(e);
        let tf = float_term_id();
        let wf = static_id();
        let web = self.made.borrow().static_view.is_some();
        let Some(terminal) = self.float_terminal() else {
            say("skip p1 no float terminal");
            return;
        };
        // 1. what each content looks like with nothing over it
        for c in 0..3 {
            self.edit(c).await;
            self.sleep(250).await;
            self.capture(&format!("p1.ref.{c}"), we, None).await;
        }
        // 2. half covered: the left half of the editor must show every edit
        let tf_half = Rect {
            x: e.x + e.w / 2,
            y: e.y,
            w: e.w - e.w / 2,
            h: e.h * 6 / 10,
        };
        let wf_half = Rect {
            x: e.x + e.w * 5 / 8,
            y: e.y + e.h * 3 / 10,
            w: e.w * 3 / 8,
            h: e.h * 6 / 10,
        };
        self.place(&tf, tf_half);
        terminal.start();
        if web {
            self.place(&wf, wf_half);
        }
        self.sleep(900).await;
        let uncovered = Rect {
            x: we.x,
            y: we.y,
            w: e.w / 2 - 4,
            h: e.h,
        };
        for c in [1usize, 2, 0] {
            self.edit(c).await;
            self.sleep(250).await;
            self.capture(&format!("p1.half.{c}"), we, Some((&format!("p1.ref.{c}"), uncovered)))
                .await;
        }
        // 3. all covered, edited under the floats, then uncovered: the edit must be there
        for c in [2usize, 0, 1] {
            self.place(&tf, e);
            if web {
                self.place(&wf, e);
            }
            self.sleep(300).await;
            self.edit(c).await;
            self.sleep(250).await;
            self.hide(&tf);
            self.hide(&wf);
            self.sleep(250).await;
            self.capture(&format!("p1.full.{c}"), we, Some((&format!("p1.ref.{c}"), we)))
                .await;
        }
        // 4. the floats move across the editor
        let (fw, fh) = (e.w / 3, e.h / 3);
        self.sample("p1.moves.begin").await;
        for i in 0..moves {
            self.place(
                &tf,
                Rect {
                    x: e.x + tri(i, 90, e.w - fw),
                    y: e.y + tri(i + 13, 70, e.h - fh),
                    w: fw,
                    h: fh,
                },
            );
            if web {
                self.place(
                    &wf,
                    Rect {
                        x: e.x + tri(i + 40, 110, e.w - fw),
                        y: e.y + tri(i, 50, e.h - fh),
                        w: fw,
                        h: fh,
                    },
                );
            }
            if i % 10 == 0 {
                self.edit((i / 10 % 3) as usize).await;
            }
            self.sleep(MOVE_PERIOD_MS).await;
        }
        self.sample("p1.moves.end").await;
        self.hide(&tf);
        self.hide(&wf);
        self.edit(0).await;
        self.sleep(300).await;
        self.capture("p1.after", we, Some(("p1.ref.0", we))).await;
        // 5. one press where both floats and the editor overlap: the top float takes the keys
        let tf_press = Rect {
            x: e.x + e.w / 4,
            y: e.y + e.h / 4,
            w: e.w / 2,
            h: e.h / 2,
        };
        let wf_press = Rect {
            x: e.x + e.w * 3 / 8,
            y: e.y + e.h * 3 / 8,
            w: e.w / 2,
            h: e.h / 2,
        };
        self.place(&tf, tf_press);
        if web {
            self.place(&wf, wf_press);
        }
        self.sleep(600).await;
        let point = self.in_window(Rect {
            x: e.x + e.w * 3 / 8 + e.w / 8,
            y: e.y + e.h * 3 / 8 + e.h / 8,
            w: 0,
            h: 0,
        });
        let expected = if web { wf.to_string() } else { tf.to_string() };
        let picked = self
            .ctx
            .window
            .pick(point.x as f64, point.y as f64, gtk4::PickFlags::DEFAULT);
        say(format!("p1 pick owner={} expect={expected}", self.owner_of(picked)));
        self.sync("press", &format!("x={} y={} expect={expected}", point.x, point.y))
            .await;
        self.sleep(500).await;
        say(format!("p1 press owner={} expect={expected}", self.focus_owner()));
    }

    // ---- The editor, terminal, and chat as tabs of one container -------------------------------

    pub(super) async fn p2_tabbed(&self) {
        let cycles = self.script.count("p2_cycles", 500);
        let (gw, gh) = self.grid_size();
        let tab = Rect {
            x: 0,
            y: 0,
            w: gw,
            h: gh,
        };
        let wt = self.in_window(tab);
        let (editor, term, agent) = (Self::editor_id(), ModuleId::terminal(), ModuleId::agent());
        self.say_geom("p2.tab", tab);
        let terminal = self.ctx.terminal.clone();
        let chat = self.chat_view();
        let show = |which: &ModuleId| {
            for id in [&editor, &term, &agent] {
                if id == which {
                    self.place(id, tab);
                } else {
                    self.hide(id);
                }
            }
        };
        // what each tab looks like, once
        show(&term);
        terminal.start();
        self.sleep(900).await;
        for k in 0..3 {
            self.type_into(&terminal, &marker(k));
            self.sleep(500).await;
            self.capture(&format!("p2.tref.{k}"), wt, None).await;
        }
        show(&editor);
        for c in 0..3 {
            self.edit(c).await;
            self.sleep(250).await;
            self.capture(&format!("p2.ref.{c}"), wt, None).await;
        }
        show(&agent);
        self.sleep(400).await;
        let mut bad_terminal = 0u32;
        let mut bad_chat = 0u32;
        let mut chat_checked = 0u32;
        self.sample("p2.tabs.begin").await;
        for i in 0..cycles {
            let c = (i % 3) as usize;
            // the editor redraws and the terminal prints while they are hidden
            self.edit(c).await;
            let m = ((i / 10) % 3) as usize;
            if i % 10 == 0 {
                self.type_into(&terminal, &marker(m));
            }
            self.sleep(60).await;
            show(&editor);
            self.sleep(130).await;
            self.capture(&format!("p2.ed.{i}"), wt, Some((&format!("p2.ref.{c}"), wt)))
                .await;
            show(&term);
            self.sleep(100).await;
            let (tw, th) = (terminal.widget().width(), terminal.widget().height());
            if tw <= 0 || th <= 0 {
                bad_terminal += 1;
                say(format!("p2 terminal reports {tw}x{th} after cycle {i}"));
            }
            if i % 10 == 0 {
                self.sleep(250).await;
                self.capture(&format!("p2.term.{i}"), wt, Some((&format!("p2.tref.{m}"), wt)))
                    .await;
            }
            show(&agent);
            self.sleep(100).await;
            if let Some(chat) = &chat {
                chat_checked += 1;
                let sized = self.js(chat, "innerWidth + 'x' + innerHeight").await;
                let want = format!("{}x{}", chat.width(), chat.height());
                if sized.as_deref() != Some(want.as_str()) {
                    bad_chat += 1;
                    say(format!("p2 chat page is {sized:?} in a host of {want} after cycle {i}"));
                }
            }
        }
        self.sample("p2.tabs.end").await;
        say(format!(
            "p2 cycles={cycles} switches={} bad_terminal={bad_terminal} chat_checked={chat_checked} bad_chat={bad_chat}",
            cycles * 3
        ));
    }

    // ---- Hosts partly outside the grid -------------------------------------------------------

    pub(super) async fn p3_outside(&self) {
        let cycles = self.script.count("p3_cycles", 100);
        let (gw, gh) = self.grid_size();
        let inside = Rect {
            x: 0,
            y: 0,
            w: gw,
            h: gh,
        };
        let outside = Rect {
            x: 0,
            y: 0,
            w: gw + 200,
            h: gh + 200,
        };
        let wi = self.in_window(inside);
        // what sits above the grid in the window (the top bar), and a host that reaches into it
        let bar = Rect {
            x: 0,
            y: 0,
            w: wi.w,
            h: wi.y,
        };
        let above = Rect {
            x: 0,
            y: -60,
            w: gw + 200,
            h: gh + 260,
        };
        let (editor, term, agent) = (Self::editor_id(), ModuleId::terminal(), ModuleId::agent());
        let terminal = self.ctx.terminal.clone();
        self.say_geom("p3.grid", inside);
        self.hide(&agent);
        // (name, id, part of the picture compared: the rows that do not depend on the host's own size)
        let mut subjects = vec![
            ("editor", editor.clone(), Rect { h: gh - 70, ..wi }),
            ("terminal", term.clone(), Rect { h: 70, ..wi }),
        ];
        if self.made.borrow().static_view.is_some() {
            subjects.push(("static", static_id(), wi));
        }
        for (name, id, part) in subjects {
            for other in [&editor, &term, &static_id()] {
                if *other != id {
                    self.hide(other);
                }
            }
            self.place(&id, inside);
            match name {
                "editor" => {
                    self.edit(0).await;
                }
                "terminal" => {
                    terminal.start();
                    self.sleep(900).await;
                    self.type_into(&terminal, &plain_marker(0));
                }
                _ => {}
            }
            self.sleep(500).await;
            self.capture(&format!("p3.{name}.in"), wi, None).await;
            if bar.h > 0 {
                self.capture(&format!("p3.{name}.bar"), bar, None).await;
            }
            for _ in 0..cycles {
                self.place(&id, outside);
                self.sleep(25).await;
                self.place(&id, inside);
                self.sleep(25).await;
            }
            self.place(&id, outside);
            self.sleep(600).await;
            if name == "terminal" {
                self.type_into(&terminal, &plain_marker(0));
                self.sleep(500).await;
            }
            self.capture_shifted(
                &format!("p3.{name}.out"),
                wi,
                Some((&format!("p3.{name}.in"), part)),
                12,
            )
            .await;
            // reaching 60 px above the grid: the top bar must not be painted over
            if bar.h > 0 {
                self.place(&id, above);
                self.sleep(500).await;
                self.capture(&format!("p3.{name}.top"), bar, Some((&format!("p3.{name}.bar"), bar)))
                    .await;
            }
            let right = self
                .ctx
                .grid
                .pick(gw as f64 + 60.0, gh as f64 / 2.0, gtk4::PickFlags::DEFAULT);
            let below = self
                .ctx
                .grid
                .pick(gw as f64 / 2.0, gh as f64 + 60.0, gtk4::PickFlags::DEFAULT);
            let within = self
                .ctx
                .grid
                .pick(gw as f64 / 2.0, gh as f64 / 2.0, gtk4::PickFlags::DEFAULT);
            say(format!(
                "p3 {name} pick_right={} pick_below={} pick_within={}",
                self.owner_of(right),
                self.owner_of(below),
                self.owner_of(within)
            ));
        }
    }

    // ---- Sibling reorder --------------------------------------------------------------------

    pub(super) async fn p4_reorder(&self) {
        let n = self.script.count("p4_reorders", 200);
        let sample_every = self.script.count("p4_sample_every", 4).max(1);
        let (editor, term, agent) = (Self::editor_id(), ModuleId::terminal(), ModuleId::agent());
        let _ = self.ctx.grid.show_module(&term);
        self.ctx.terminal.start();
        self.sleep(900).await;
        let we = self.in_window(self.tile_of(&editor));
        for c in 0..3 {
            self.edit(c).await;
            self.sleep(250).await;
            self.capture(&format!("p4.ref.{c}"), we, None).await;
        }
        let subjects = [("terminal", term), ("agent", agent), ("editor", editor.clone())];
        let mut fresh = 0u32;
        for (name, id) in subjects {
            let Some(widget) = self.host(&id) else { continue };
            say(format!("p4 {name} begin keys={}", self.give_keys(&id)));
            self.sleep(300).await;
            let mut lost_keys = 0u32;
            let order: Vec<gtk4::Widget> =
                std::iter::successors(self.ctx.grid.first_child(), |w| w.next_sibling()).collect();
            for i in 0..n {
                let c = (i / sample_every % 3) as usize;
                let sampled = name == "editor" && i % sample_every == 0;
                if sampled {
                    self.edit(c).await;
                }
                let last = self.ctx.grid.last_child();
                if last.as_ref() == Some(&widget) {
                    widget.insert_after(&self.ctx.grid, None::<&gtk4::Widget>);
                } else {
                    widget.insert_after(&self.ctx.grid, last.as_ref());
                }
                self.sleep(if sampled { 140 } else { 12 }).await;
                if self.focus_owner() != id.to_string() {
                    lost_keys += 1;
                }
                if sampled {
                    fresh += 1;
                    self.capture(&format!("p4.fresh.{i}"), we, Some((&format!("p4.ref.{c}"), we)))
                        .await;
                }
            }
            // the original order back, so the next subject starts from it
            let mut previous: Option<gtk4::Widget> = None;
            for child in order {
                child.insert_after(&self.ctx.grid, previous.as_ref());
                previous = Some(child);
            }
            say(format!("p4 {name} reorders={n} lost_keys={lost_keys}"));
        }
        say(format!("p4 fresh_samples={fresh}"));
    }

    /// Moves a host to the end of the grid's sibling list, which is the top of its paint order.
    fn raise(&self, id: &ModuleId) {
        let (Some(widget), Some(last)) = (self.host(id), self.ctx.grid.last_child()) else {
            return;
        };
        if last != widget {
            widget.insert_after(&self.ctx.grid, Some(&last));
        }
    }

    // ---- Web hosts moved, covered, reordered, and hidden --------------------------------------

    pub(super) async fn p5_web(&self) {
        let n = self.script.count("p5_steps", 300);
        if !self.web_ok() {
            say("skip p5 no WebKit in this process");
            return;
        }
        let Some(terminal) = self.float_terminal() else {
            say("skip p5 no float terminal");
            return;
        };
        let (gw, gh) = self.grid_size();
        let (s, a, tf) = (static_id(), ModuleId::agent(), float_term_id());
        let (sw, sh) = (gw / 3, gh / 3);
        // moved
        for i in 0..n {
            self.place(
                &s,
                Rect {
                    x: tri(i, 90, gw - sw),
                    y: tri(i + 20, 70, gh - sh),
                    w: sw,
                    h: sh,
                },
            );
            self.place(
                &a,
                Rect {
                    x: tri(i + 45, 110, gw - sw),
                    y: tri(i, 60, gh - sh),
                    w: sw,
                    h: sh,
                },
            );
            self.sleep(MOVE_PERIOD_MS).await;
        }
        self.web_state("p5.moved").await;
        // covered: the float terminal comes last in sibling order, so it paints over both web hosts
        // (at setup it was made before the static page, which would leave that page on top of it)
        let s_at = Rect {
            x: gw * 2 / 3,
            y: 0,
            w: sw,
            h: sh,
        };
        let a_at = Rect {
            x: gw / 3,
            y: gh / 2,
            w: sw,
            h: sh,
        };
        self.place(&s, s_at);
        self.place(&a, a_at);
        self.place(
            &tf,
            Rect {
                x: 0,
                y: 0,
                w: sw,
                h: sh,
            },
        );
        self.raise(&tf);
        terminal.start();
        self.sleep(700).await;
        for i in 0..n {
            self.place(
                &tf,
                Rect {
                    x: tri(i, 90, gw - sw),
                    y: tri(i + 11, 70, gh - sh),
                    w: sw,
                    h: sh,
                },
            );
            self.sleep(MOVE_PERIOD_MS).await;
        }
        self.web_state("p5.covered").await;
        // reordered: the three overlap one another and each in turn is moved to the end of the sibling
        // list, so both web hosts are raised over the terminal and over each other, and lowered again
        let over = |dx: i32, dy: i32| Rect {
            x: s_at.x - dx,
            y: s_at.y + dy,
            w: sw,
            h: sh,
        };
        self.place(&s, over(0, 0));
        self.place(&a, over(sw / 2, sh / 3));
        self.place(&tf, over(sw / 4, sh * 2 / 3));
        let order = [&s, &a, &tf];
        let mut reorders = 0u32;
        for i in 0..n {
            self.raise(order[(i as usize) % order.len()]);
            reorders += 1;
            self.sleep(MOVE_PERIOD_MS).await;
        }
        self.web_state("p5.reordered").await;
        say(format!("p5 steps={n} reorders={reorders}"));
        // hidden
        for i in 0..n {
            if i % 2 == 0 {
                self.hide(&s);
                self.hide(&a);
            } else {
                self.place(&s, s_at);
                self.place(&a, a_at);
            }
            self.sleep(MOVE_PERIOD_MS).await;
        }
        self.place(&s, s_at);
        self.place(&a, a_at);
        self.sleep(400).await;
        self.web_state("p5.hidden").await;
    }

    // ---- Instances repeatedly created and ended ----------------------------------------------

    pub(super) async fn p6_pooled(&self) {
        let cycles = self.script.count("p6_cycles", 20);
        let (gw, gh) = self.grid_size();
        let (cw, ch) = (gw / 4 - 8, gh / 3);
        let y = gh - ch;
        let term_rects: Vec<(ModuleId, Rect)> = (2..=4)
            .map(|n| {
                (
                    ModuleId::lua(&format!("probe-terminal-{n}")),
                    Rect {
                        x: (n - 2) * (cw + 8),
                        y,
                        w: cw,
                        h: ch,
                    },
                )
            })
            .collect();
        let chat_id = ModuleId::lua("probe-chat-pool");
        let chat_at = Rect {
            x: 3 * (cw + 8),
            y,
            w: cw,
            h: ch,
        };
        let web = self.web_ok();
        self.p6_views().await;
        self.sample("p6.c0.killed").await;
        for k in 1..=cycles {
            // create: the first cycle makes the hosts, the others reuse them idle in the pool
            for (id, rect) in &term_rects {
                if k == 1 {
                    let terminal = TerminalPane::new(self.ctx.project_root.clone());
                    self.ctx
                        .grid
                        .add(id.clone(), terminal.widget(), terminal.widget(), HostKind::Direct);
                    self.track(id.as_str(), terminal.widget().upcast_ref());
                    self.made.borrow_mut().pool_terms.push((id.clone(), terminal));
                }
                self.place(id, *rect);
                let terminal = self
                    .made
                    .borrow()
                    .pool_terms
                    .iter()
                    .find(|(m, _)| m == id)
                    .map(|(_, t)| t.clone());
                if let Some(terminal) = terminal {
                    terminal.start();
                }
            }
            if web {
                if k == 1 {
                    self.make_chat_page(&chat_id, pool_view(), true, chat_at, true).await;
                } else {
                    self.place(&chat_id, chat_at);
                    let handle = self.made.borrow().pool_chat.as_ref().map(|(_, _, h)| h.clone());
                    if let Some(handle) = handle {
                        handle.reload_document_by_hand(pool_view());
                    }
                }
            }
            self.sleep(500).await;
            self.sample(&format!("p6.c{k}.created")).await;
            // kill: the shells hung up, the page emptied, the hosts idle
            for (id, _) in &term_rects {
                let terminal = self
                    .made
                    .borrow()
                    .pool_terms
                    .iter()
                    .find(|(m, _)| m == id)
                    .map(|(_, t)| t.clone());
                if let Some(terminal) = terminal {
                    terminal.close();
                }
                self.hide(id);
            }
            if web {
                let view = self.made.borrow().pool_chat.as_ref().map(|(_, v, _)| v.clone());
                if let Some(view) = view {
                    view.load_uri("about:blank");
                }
                self.hide(&chat_id);
            }
            self.sleep(500).await;
            self.sample(&format!("p6.c{k}.killed")).await;
        }
        say(format!("p6 cycles={cycles}"));
    }

    /// Posts a `ready` from `view`'s document, as its own script would, and logs the mark that lets the
    /// runner read which view the panel took the message to come from (`expect` is the module's number).
    async fn post_ready(&self, name: &str, expect: u8, view: &WebView) {
        say(format!("mark ready-{name} expect={expect}"));
        self.js(view, POST_READY).await;
        self.sleep(500).await;
        say("mark none");
    }

    /// Opens a chat view as `instance.new { kind = "agent" }` does, where the open rules put it, and counts its
    /// host from now on. `None` when the window refused.
    fn open_view(&self, tracked: &mut Vec<ModuleId>) -> Option<ModuleId> {
        match (self.ctx.new_chat_view)() {
            Ok(id) => {
                if !tracked.contains(&id) {
                    if let Some(host) = self.host(&id) {
                        self.track(id.as_str(), &host);
                        tracked.push(id.clone());
                    }
                }
                Some(id)
            }
            Err(err) => {
                say(format!(
                    "p6 views refused={}",
                    err.split_whitespace().collect::<Vec<_>>().join("_")
                ));
                None
            }
        }
    }

    /// The window's pooled page of `id`, if it has a live one.
    fn pooled(&self, id: &ModuleId) -> Option<WebView> {
        self.ctx.chat_views.borrow().get(id).map(|(view, _)| view.clone())
    }

    /// Exercises the window's own chat view lifecycle. Kill a startup view as `prefix x y`
    /// would, inspect the next views' numbers and hosts, then repeatedly create and kill
    /// `agent#2` through `agent#4`. Each `p6 ...` line is a result for the runner to judge.
    async fn p6_views(&self) {
        if !self.web_ok() {
            say("skip p6 views no WebKit in this process");
            return;
        }
        let cycles = self.script.count("p6_view_cycles", self.script.count("p6_cycles", 20));
        let four = ViewId::new(4).expect("4 is a chat view's number");
        let four_id = four.module();
        let (gw, gh) = self.grid_size();
        let corner = Rect {
            x: gw - 300,
            y: gh - 200,
            w: 280,
            h: 190,
        };
        // The view the saved layout named: its host exists from the start, with a tab of its own.
        let started = self
            .startup_views
            .iter()
            .find(|(id, _)| *id == four_id)
            .map(|(_, ptr)| *ptr);
        let held = self.pooled(&four_id);
        let ptr_now = held.as_ref().map_or(0, super::page_pointer);
        say(format!(
            "p6 start module={four_id} started_ptr={:#x} ptr={ptr_now:#x} live={} holds_tab={}",
            started.unwrap_or(0),
            held.is_some(),
            self.ctx.panel.views().contains(&four)
        ));
        let mut tracked: Vec<ModuleId> = Vec::new();
        if let Some(host) = self.host(&four_id) {
            self.track(four_id.as_str(), &host);
            tracked.push(four_id.clone());
        }
        let Some(view) = held else {
            say(format!("p6 views the window opened without {four_id}"));
            return;
        };
        // Shown by the probe for the document's load and the message check, then given back to the layout.
        self.place(&four_id, corner);
        self.wait_loaded(&view).await;
        self.sleep(500).await;
        self.post_ready("agent#4-start", 4, &view).await;
        self.ctx.grid.probe_place(&four_id, None);
        // `prefix x` then `y`, with no key pressed.
        say(format!("mark idle-{four_id} expect=0"));
        (self.ctx.kill_chat_view)(&four_id);
        self.sleep(1500).await;
        let idle_document = self
            .js(&view, SHOWS_IDLE_DOCUMENT)
            .await
            .unwrap_or_else(|| "unknown".to_owned());
        say(format!(
            "p6 idle module={four_id} live={} holds_tab={} idle_document={idle_document} ptr={:#x}",
            self.pooled(&four_id).is_some(),
            self.ctx.panel.views().contains(&four),
            super::page_pointer(&view)
        ));
        say("mark none");
        self.sample("p6.v0.killed").await;
        for k in 1..=cycles {
            let mut ids = Vec::new();
            for _ in 0..3 {
                if let Some(id) = self.open_view(&mut tracked) {
                    ids.push(id);
                }
            }
            self.sleep(1000).await;
            for id in &ids {
                let Some(page) = self.pooled(id) else {
                    say(format!("p6 view cycle={k} module={id} ptr=0x0"));
                    continue;
                };
                let ptr = super::page_pointer(&page);
                say(format!("p6 view cycle={k} module={id} ptr={ptr:#x}"));
                if k == 1 {
                    let number = ViewId::of_module(id).map_or(0, |v| v.number());
                    let name = if *id == four_id {
                        "agent#4-reused".to_owned()
                    } else {
                        id.to_string()
                    };
                    self.post_ready(&name, number, &page).await;
                }
            }
            self.sample(&format!("p6.v{k}.created")).await;
            for id in ids.iter().rev() {
                (self.ctx.kill_chat_view)(id);
            }
            self.sleep(700).await;
            self.sample(&format!("p6.v{k}.killed")).await;
        }
        say(format!("p6 view_cycles={cycles}"));
    }

    /// Builds a chat page with `build_chat_page`, attaches it to the window's panel as
    /// `page_view`, adds it to the grid as a web host under `id`, shows it at `at`, and waits
    /// for its document. `related` passes the base page to the constructor. `pool` retains
    /// the page for reuse; otherwise it is kept among the extra pages.
    async fn make_chat_page(
        &self,
        id: &ModuleId,
        page_view: ViewId,
        related: bool,
        at: Rect,
        pool: bool,
    ) -> Option<WebView> {
        let base = self.chat_view();
        if base.is_none() {
            return None;
        }
        let handle: AgentPanelHandle = self.ctx.panel.clone();
        // Attaching a page as a view the window already shows would take that view's routing from its
        // real page, so the probe's numbers must be free in the window it runs in.
        let window_holds =
            self.ctx.chat_views.borrow().get(&page_view.module()).is_some() || handle.views().contains(&page_view);
        if window_holds {
            say(format!(
                "skip {id} chat view {} is the window's own; keep agent#{} out of the probe's layout",
                page_view.number(),
                page_view.number()
            ));
            return None;
        }
        let (view, _page) =
            crate::agent_panel::build_chat_page(&handle, page_view, if related { base.as_ref() } else { None });
        let name = id.to_string();
        view.connect_web_process_terminated(move |_, reason| say(format!("web-process-terminated {name} {reason:?}")));
        let host = WebHost::new(&view);
        self.ctx.grid.add(id.clone(), &host, &view, HostKind::Web);
        self.track(id.as_str(), host.upcast_ref());
        self.place(id, at);
        if pool {
            self.made.borrow_mut().pool_chat = Some((id.clone(), view.clone(), handle));
        } else {
            self.made
                .borrow_mut()
                .extra_chats
                .push((id.clone(), view.clone(), handle));
        }
        self.wait_loaded(&view).await;
        Some(view)
    }

    // ---- A second chat page with related-view ------------------------------------------------

    pub(super) async fn p7_second_chat(&self) {
        let Some(base) = self.chat_view() else {
            say("skip p7 no WebKit in this process");
            return;
        };
        let (gw, gh) = self.grid_size();
        let corner = |n: i32| Rect {
            x: gw - 300 * n,
            y: gh - 200,
            w: 280,
            h: 190,
        };
        self.sample("p7.before").await;
        let second_id = ModuleId::lua("probe-chat-2");
        let (second_view, third_view) = (ViewId::new(2).expect("a view"), ViewId::new(3).expect("a view"));
        let Some(second) = self
            .make_chat_page(&second_id, second_view, true, corner(1), false)
            .await
        else {
            say("p7 related page could not be built");
            return;
        };
        self.sleep(2000).await;
        self.sample("p7.related").await;
        let same_ucm = base.user_content_manager() == second.user_content_manager();
        let same_session = base.network_session() == second.network_session();
        let ephemeral = second.network_session().is_some_and(|s| s.is_ephemeral());
        say(format!(
            "p7 own_content_manager={} own_network_session={} second_session_ephemeral={ephemeral}",
            !same_ucm, !same_session
        ));
        // each page's message reaches only its own manager's handler
        let heard = [Rc::new(Cell::new(0u32)), Rc::new(Cell::new(0u32))];
        for (view, count) in [&base, &second].into_iter().zip(&heard) {
            if let Some(manager) = view.user_content_manager() {
                manager.register_script_message_handler("probe", None);
                let count = count.clone();
                manager.connect_script_message_received(Some("probe"), move |_, _| count.set(count.get() + 1));
            }
        }
        self.js(
            &second,
            "window.webkit.messageHandlers.probe.postMessage('second'); 'posted'",
        )
        .await;
        self.sleep(400).await;
        let after_second = (heard[0].get(), heard[1].get());
        self.js(
            &base,
            "window.webkit.messageHandlers.probe.postMessage('base'); 'posted'",
        )
        .await;
        self.sleep(400).await;
        let after_base = (heard[0].get(), heard[1].get());
        say(format!(
            "p7 routing second_posts(base_heard={},second_heard={}) base_posts(base_heard={},second_heard={})",
            after_second.0,
            after_second.1,
            after_base.0 - after_second.0,
            after_base.1 - after_second.1
        ));
        // the panel's own channel: a `ready` posted from each page is taken for that page's view only
        self.post_ready("p7-second", 2, &second).await;
        self.post_ready("p7-base", 1, &base).await;
        // the policy that holds in the base page holds in the second one. The result is left in a page
        // variable and read back: a script cannot hand a promise's value to the host directly.
        const OUTSIDE: &str = "window.__outside = 'pending'; (async function(){var v=null;\
            document.addEventListener('securitypolicyviolation',function(e){v=e.violatedDirective;});\
            var r;try{await fetch('https://outside.example/',{mode:'no-cors'});r='resolved';}catch(e){r='rejected';}\
            await new Promise(function(t){setTimeout(t,80);});window.__outside = r+'/'+v;})(); 'started'";
        for (name, view) in [("base", &base), ("second", &second)] {
            self.js(view, OUTSIDE).await;
            self.sleep(800).await;
            let answer = self
                .js(view, "window.__outside")
                .await
                .unwrap_or_else(|| "no-answer".to_owned());
            say(format!("p7 fetch page={name} result={answer}"));
        }
        for (name, view) in [("base", &base), ("second", &second)] {
            let before = view.uri().map(|u| u.to_string()).unwrap_or_default();
            self.js(
                view,
                "(function(){var a=document.createElement('a');a.href='ftp://outside.example/';document.body.appendChild(a);a.click();return 'clicked';})()",
            )
            .await;
            self.sleep(600).await;
            let after = view.uri().map(|u| u.to_string()).unwrap_or_default();
            say(format!("p7 navigation page={name} ignored={}", before == after));
        }
        // the cost of the other way, for the fallback's number
        let third_id = ModuleId::lua("probe-chat-3");
        let third = self
            .make_chat_page(&third_id, third_view, false, corner(2), false)
            .await;
        if third.is_some() {
            self.sleep(2000).await;
            self.sample("p7.unrelated").await;
        }
        // The proxy backstop is exercised, not inferred from the session's flags: the host loads an
        // outside address in each page (the page's own fetch and link click are stopped before any
        // networking), and where the request dies says which proxy it went through. The page built without
        // related-view carries the constructor's own session and is the reference; a plain page, with no
        // proxy, shows what the same request does without one. The base page is left on its document.
        if let Some(third) = third {
            let control = WebView::new();
            control.set_hexpand(true);
            control.set_vexpand(true);
            let control_host = WebHost::new(&control);
            let control_id = ModuleId::lua("probe-chat-control");
            self.ctx
                .grid
                .add(control_id.clone(), &control_host, &control, HostKind::Web);
            self.track(control_id.as_str(), control_host.upcast_ref());
            self.place(&control_id, corner(3));
            self.sleep(500).await;
            for (name, view) in [("second", &second), ("unrelated", &third), ("control", &control)] {
                let outcome = self.load_outcome(view, "http://outside.example/probe").await;
                say(format!("p7 proxy page={name} {outcome}"));
            }
            self.made.borrow_mut().control_chat = Some(control);
        }
    }

    // ---- A press near a divider reaches the divider ------------------------------------------

    /// With the bottom terminal shown, the window holds a vertical divider (editor beside the chat) and a
    /// horizontal one (the terminal under both). GTK's own pick, at the middle of each divider's line and one
    /// pixel either side, must find that divider's handle and never a module: a terminal that gets the press
    /// starts a text selection instead of the divider moving.
    pub(super) async fn p8_divider_pick(&self) {
        let term = ModuleId::terminal();
        let shown = self.ctx.grid.show_module(&term).unwrap_or(false);
        self.ctx.terminal.start();
        self.sleep(900).await;
        let dividers = self.ctx.grid.probe_dividers();
        say(format!("p8 dividers={}", dividers.len()));
        for (n, (axis, r, handle)) in dividers.iter().enumerate() {
            let name = match axis {
                Axis::Row => "vertical",
                Axis::Column => "horizontal",
            };
            for off in [-1, 0, 1] {
                let (x, y) = match axis {
                    Axis::Row => (r.x + r.w / 2 + off, r.y + r.h / 2),
                    Axis::Column => (r.x + r.w / 2, r.y + r.h / 2 + off),
                };
                let picked = self.ctx.grid.pick(x as f64, y as f64, gtk4::PickFlags::DEFAULT);
                let owner = if picked.as_ref() == Some(handle) {
                    "divider".to_owned()
                } else {
                    self.owner_of(picked)
                };
                say(format!(
                    "p8 pick divider={n} axis={name} off={off} at={x},{y} owner={owner}"
                ));
            }
        }
        if shown {
            let _ = self.ctx.grid.hide_module(&term, &|id| self.give_keys(id));
        }
    }

    /// Loads `uri` in `view` from the host and reports how it ended: `loaded`, or `failed=<error text>`
    /// (spaces turned into underscores so that the line stays one word per field).
    async fn load_outcome(&self, view: &WebView, uri: &str) -> String {
        let slot: Rc<std::cell::RefCell<Option<String>>> = Rc::new(std::cell::RefCell::new(None));
        let failed = {
            let slot = slot.clone();
            view.connect_load_failed(move |_, _, _, error| {
                slot.borrow_mut().get_or_insert_with(|| {
                    format!(
                        "failed={}",
                        error.message().split_whitespace().collect::<Vec<_>>().join("_")
                    )
                });
                false
            })
        };
        let finished = {
            let slot = slot.clone();
            view.connect_load_changed(move |_, event| {
                if event == webkit6::LoadEvent::Finished {
                    slot.borrow_mut().get_or_insert_with(|| "loaded".to_owned());
                }
            })
        };
        view.load_uri(uri);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while slot.borrow().is_none() && std::time::Instant::now() < deadline {
            self.sleep(100).await;
        }
        view.disconnect(failed);
        view.disconnect(finished);
        let outcome = slot.borrow().clone();
        outcome.unwrap_or_else(|| "failed=no-answer-in-15-s".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_position_goes_out_and_back_within_its_range() {
        let seen: Vec<i32> = (0..8).map(|i| tri(i, 4, 100)).collect();
        assert_eq!(seen, vec![0, 25, 50, 75, 100, 75, 50, 25]);
        assert!((0..1000).all(|i| (0..=100).contains(&tri(i, 7, 100))));
        assert!(
            (0..=10).contains(&tri(5, 0, 10)),
            "a zero period must not divide by zero"
        );
    }

    #[test]
    fn a_marker_ends_its_line_and_names_its_number() {
        assert!(marker(2).contains("MARK-2") && marker(2).ends_with('\n'));
        assert!(!plain_marker(2).contains("stty"));
    }
}
