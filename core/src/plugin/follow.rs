//! When a plugin that follows the editor's buffer asks nvim for what: the snippet's install, following on
//! and off, the text pulls with their rate limits, and the page's own editor operations. Pure: it holds no
//! editor and no clock; the host tells it what happened and asks `next` with the time.
//!
//! One call is out at a time. The embedded editor serves each request on a thread of its own, so two calls
//! sent back to back may reach nvim in either order: a `follow {on = false}` overtaken by the
//! `follow {on = true}` sent after it would leave nvim reporting views to a hidden page.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// The least time between two pulls of a buffer that keeps changing.
pub const PULL_INTERVAL: Duration = Duration::from_millis(200);
/// The same for a buffer whose last pull measured over `LARGE_TEXT_BYTES`: its render costs more, so it
/// follows a little later rather than lag typing.
pub const LARGE_PULL_INTERVAL: Duration = Duration::from_secs(1);
pub const LARGE_TEXT_BYTES: u64 = 512 * 1024;
/// Past either cap nvim sends the buffer's size instead of its lines, and the page shows a notice.
pub const MAX_TEXT_BYTES: u64 = 4 * 1024 * 1024;
pub const MAX_TEXT_LINES: u64 = 50_000;
/// How long after an install nvim answered too early (before it had read the user's config) it is asked
/// again; also how long the queue rests after a call that failed, so a broken editor is not asked in a loop.
pub const INSTALL_RETRY: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, PartialEq)]
pub enum FollowOp {
    Install,
    Follow(bool),
    Text {
        buf: u64,
    },
    Reveal {
        buf: u64,
        line: u32,
        token: u64,
        re: u64,
    },
    Jump {
        buf: u64,
        line: u32,
        col: u32,
        focus: bool,
        re: u64,
    },
    OpenLink {
        path: PathBuf,
        line: Option<u32>,
        re: u64,
    },
    Teardown,
}

/// What `next` says: send this now (it is then outstanding), look again at that time, or nothing to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Next {
    Send(FollowOp),
    At(Instant),
    Idle,
}

/// How the outstanding call ended, as far as the policy cares.
#[derive(Debug, Clone, PartialEq)]
pub enum Done {
    Installed,
    /// nvim had not finished starting, so it installed nothing.
    Early,
    Followed(bool),
    Pulled {
        buf: u64,
        tick: u64,
        bytes: u64,
    },
    Gone {
        buf: u64,
    },
    Other,
    Failed,
}

#[derive(Debug, Clone, Copy)]
struct Pull {
    buf: u64,
    at: Instant,
    large: bool,
}

pub struct FollowState {
    /// The editor session the state below belongs to.
    session: Option<u64>,
    installed: bool,
    /// When an install nvim answered early is due again; `None` while it is due at once.
    install_due: Option<Instant>,
    /// nvim reported that it has started while an install was out: an early answer to that install is
    /// stale news, so it is asked again at once.
    entered_while_installing: bool,
    /// What nvim was last told and confirmed.
    nvim_follows: bool,
    page_follows: bool,
    on_screen: bool,
    /// The newest view's `(buf, tick)`.
    view: Option<(u64, u64)>,
    /// The `(buf, tick)` whose text (or notice) the page was sent.
    page_text: Option<(u64, u64)>,
    last_pull: Option<Pull>,
    parked: VecDeque<FollowOp>,
    teardown: bool,
    stopped: bool,
    /// The call out, with the session it was sent in.
    outstanding: Option<(FollowOp, Option<u64>)>,
    /// After a failed call nothing is sent before this.
    rest_until: Option<Instant>,
}

impl Default for FollowState {
    fn default() -> Self {
        FollowState::new()
    }
}

impl FollowState {
    pub fn new() -> FollowState {
        FollowState {
            session: None,
            installed: false,
            install_due: None,
            entered_while_installing: false,
            nvim_follows: false,
            page_follows: false,
            on_screen: false,
            view: None,
            page_text: None,
            last_pull: None,
            parked: VecDeque::new(),
            teardown: false,
            stopped: false,
            outstanding: None,
            rest_until: None,
        }
    }

    /// The editor's session (`EditorRpc::target()`). A change forgets the install and nvim's following, makes
    /// the outstanding call's answer stale (`is_stale`), and asks for `Install` when the new one is `Some`.
    /// Buffer numbers and change ticks belong to one nvim, so the view and what the page holds are forgotten
    /// too: the new editor's first view is pulled at once.
    pub fn session(&mut self, target: Option<u64>) -> bool {
        if target == self.session {
            return false;
        }
        self.session = target;
        self.installed = false;
        self.install_due = None;
        self.entered_while_installing = false;
        self.nvim_follows = false;
        self.rest_until = None;
        self.view = None;
        self.page_text = None;
        self.last_pull = None;
        true
    }

    pub fn current_session(&self) -> Option<u64> {
        self.session
    }

    /// nvim reported VimEnter: an install it answered `early` is due at once instead of `INSTALL_RETRY` later.
    pub fn entered(&mut self) {
        if self.installed {
            return;
        }
        if matches!(self.outstanding, Some((FollowOp::Install, s)) if s == self.session) {
            self.entered_while_installing = true;
        } else {
            self.install_due = None;
        }
    }

    /// The current document called `editor.follow`; a new document: false.
    pub fn page_follows(&mut self, on: bool) {
        self.page_follows = on;
    }

    pub fn on_screen(&mut self, on: bool) {
        self.on_screen = on;
    }

    pub fn view(&mut self, buf: u64, tick: u64) {
        self.view = Some((buf, tick));
    }

    /// The page was sent this buffer's text (or its notice) at `tick`.
    pub fn page_has(&mut self, buf: u64, tick: u64) {
        self.page_text = Some((buf, tick));
    }

    /// A new document holds no text.
    pub fn page_forgot(&mut self) {
        self.page_text = None;
    }

    /// What the page was last sent, as `(buf, tick)`.
    pub fn page_text(&self) -> Option<(u64, u64)> {
        self.page_text
    }

    /// The buffer is gone: it is neither viewed nor pulled again.
    pub fn gone(&mut self, buf: u64) {
        if self.view.is_some_and(|(b, _)| b == buf) {
            self.view = None;
        }
        if self.page_text.is_some_and(|(b, _)| b == buf) {
            self.page_text = None;
        }
    }

    /// A page's Reveal/Jump/OpenLink, in order, or Teardown. At most one Reveal waits: a newer one takes the
    /// waiting one's place in line (only where the page wants to be now matters, and a page scrolling fast
    /// would otherwise queue a reveal per frame), and the `re` of the one it replaced is returned.
    pub fn park(&mut self, op: FollowOp) -> Option<u64> {
        if op == FollowOp::Teardown {
            self.teardown = true;
            return None;
        }
        if self.stopped {
            return None;
        }
        if matches!(op, FollowOp::Reveal { .. }) {
            if let Some(waiting) = self.parked.iter_mut().find(|p| matches!(p, FollowOp::Reveal { .. })) {
                let replaced = match waiting {
                    FollowOp::Reveal { re, .. } => Some(*re),
                    _ => None,
                };
                *waiting = op;
                return replaced;
            }
        }
        self.parked.push_back(op);
        None
    }

    /// Takes back every page's op still waiting (not the one out), for a document that was replaced: what it
    /// asked is no longer anyone's to answer, and a page id may name another request in the new document.
    /// Returns their `re`s.
    pub fn drop_page_ops(&mut self) -> Vec<u64> {
        let mut dropped = Vec::new();
        self.parked.retain(|op| match op {
            FollowOp::Reveal { re, .. } | FollowOp::Jump { re, .. } | FollowOp::OpenLink { re, .. } => {
                dropped.push(*re);
                false
            }
            _ => true,
        });
        dropped
    }

    fn send(&mut self, op: FollowOp) -> Next {
        if op == FollowOp::Teardown {
            self.stopped = true;
            self.teardown = false;
            self.parked.clear();
        }
        self.outstanding = Some((op.clone(), self.session));
        Next::Send(op)
    }

    pub fn next(&mut self, now: Instant) -> Next {
        if self.stopped || self.outstanding.is_some() {
            return Next::Idle;
        }
        if self.teardown {
            if self.session.is_some() && self.installed {
                return self.send(FollowOp::Teardown);
            }
            // Nothing was installed in this editor, so there is nothing to take down.
            self.stopped = true;
            self.teardown = false;
            self.parked.clear();
            return Next::Idle;
        }
        if self.session.is_none() {
            return Next::Idle;
        }
        if let Some(until) = self.rest_until {
            if now < until {
                return Next::At(until);
            }
            self.rest_until = None;
        }
        if !self.installed {
            if let Some(due) = self.install_due {
                if now < due {
                    return Next::At(due);
                }
            }
            return self.send(FollowOp::Install);
        }
        let want = self.page_follows && self.on_screen;
        if self.nvim_follows != want {
            return self.send(FollowOp::Follow(want));
        }
        if let Some(op) = self.parked.pop_front() {
            return self.send(op);
        }
        if !self.nvim_follows {
            return Next::Idle;
        }
        let Some((buf, tick)) = self.view else {
            return Next::Idle;
        };
        // A change tick only grows, and a pull may answer a tick newer than the view that asked for it (an
        // edit landed first, and its own view line may never come): the page holding that tick or a later
        // one is up to date.
        if self.page_text.is_some_and(|(b, t)| b == buf && t >= tick) {
            return Next::Idle;
        }
        let due = match (self.page_text, self.last_pull) {
            (Some((page_buf, _)), Some(pull)) if page_buf == buf && pull.buf == buf => {
                pull.at + if pull.large { LARGE_PULL_INTERVAL } else { PULL_INTERVAL }
            }
            // Another buffer (or none) on the page: the user switched, and waits for it.
            _ => now,
        };
        if now < due {
            return Next::At(due);
        }
        self.send(FollowOp::Text { buf })
    }

    pub fn done(&mut self, outcome: Done, now: Instant) {
        let Some((op, session)) = self.outstanding.take() else {
            return;
        };
        let was_install = op == FollowOp::Install;
        let entered = was_install && std::mem::take(&mut self.entered_while_installing);
        if session != self.session {
            // Asked of an earlier editor: what it says is about a process that is gone.
            return;
        }
        match outcome {
            Done::Installed => {
                if was_install {
                    self.installed = true;
                    self.install_due = None;
                }
            }
            Done::Early => {
                self.installed = false;
                self.install_due = if entered { None } else { Some(now + INSTALL_RETRY) };
            }
            Done::Followed(on) => self.nvim_follows = on,
            Done::Pulled { buf, tick: _, bytes } => {
                self.last_pull = Some(Pull {
                    buf,
                    at: now,
                    large: bytes > LARGE_TEXT_BYTES,
                })
            }
            Done::Gone { buf } => self.gone(buf),
            Done::Other => {}
            Done::Failed => self.rest_until = Some(now + INSTALL_RETRY),
        }
    }

    pub fn outstanding(&self) -> Option<&FollowOp> {
        self.outstanding.as_ref().map(|(op, _)| op)
    }

    /// Whether the outstanding call was sent to an earlier editor session.
    pub fn is_stale(&self) -> bool {
        self.outstanding.as_ref().is_some_and(|(_, s)| *s != self.session)
    }

    /// A Teardown was sent: nothing is sent again.
    pub fn stopped(&self) -> bool {
        self.stopped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Installed in session 1, a following document on screen, nvim following.
    fn following(t: Instant) -> FollowState {
        let mut f = FollowState::new();
        assert!(f.session(Some(1)));
        assert_eq!(f.next(t), Next::Send(FollowOp::Install));
        f.done(Done::Installed, t);
        f.page_follows(true);
        f.on_screen(true);
        assert_eq!(f.next(t), Next::Send(FollowOp::Follow(true)));
        f.done(Done::Followed(true), t);
        f
    }

    #[test]
    fn install_comes_first_and_follow_waits_for_a_following_document_on_screen() {
        let t = Instant::now();
        let mut f = FollowState::new();
        assert_eq!(f.next(t), Next::Idle, "no editor yet");
        assert!(f.session(Some(1)));
        assert!(!f.session(Some(1)), "the same session is no change");
        assert_eq!(f.next(t), Next::Send(FollowOp::Install));
        assert_eq!(f.next(t), Next::Idle, "one call at a time");
        f.done(Done::Installed, t);
        assert_eq!(f.next(t), Next::Idle);
        f.page_follows(true);
        assert_eq!(f.next(t), Next::Idle, "not on screen");
        f.on_screen(true);
        assert_eq!(f.next(t), Next::Send(FollowOp::Follow(true)));
        f.done(Done::Followed(true), t);
        assert_eq!(f.next(t), Next::Idle);
        f.on_screen(false);
        assert_eq!(f.next(t), Next::Send(FollowOp::Follow(false)));
        f.done(Done::Followed(false), t);
        f.on_screen(true);
        assert_eq!(f.next(t), Next::Send(FollowOp::Follow(true)));
        f.done(Done::Followed(true), t);
        f.page_follows(false);
        assert_eq!(
            f.next(t),
            Next::Send(FollowOp::Follow(false)),
            "the new document has not followed"
        );
    }

    #[test]
    fn a_new_buffer_is_pulled_at_once_and_typing_at_most_every_200_ms() {
        let t0 = Instant::now();
        let mut f = following(t0);
        f.view(3, 41);
        assert_eq!(f.next(t0), Next::Send(FollowOp::Text { buf: 3 }));
        f.done(
            Done::Pulled {
                buf: 3,
                tick: 41,
                bytes: 100,
            },
            t0,
        );
        f.page_has(3, 41);
        assert_eq!(f.next(t0), Next::Idle, "the page has the newest tick");
        f.view(3, 42);
        assert_eq!(f.next(t0 + ms(50)), Next::At(t0 + ms(200)));
        assert_eq!(f.next(t0 + ms(200)), Next::Send(FollowOp::Text { buf: 3 }));
        f.done(
            Done::Pulled {
                buf: 3,
                tick: 42,
                bytes: 100,
            },
            t0 + ms(200),
        );
        f.page_has(3, 42);
        f.view(5, 1);
        assert_eq!(
            f.next(t0 + ms(210)),
            Next::Send(FollowOp::Text { buf: 5 }),
            "another buffer: at once"
        );
    }

    #[test]
    fn over_512_kib_pulls_are_spaced_a_second_apart() {
        let t0 = Instant::now();
        let mut f = following(t0);
        f.view(3, 41);
        assert_eq!(f.next(t0), Next::Send(FollowOp::Text { buf: 3 }));
        f.done(
            Done::Pulled {
                buf: 3,
                tick: 41,
                bytes: 600 * 1024,
            },
            t0,
        );
        f.page_has(3, 41);
        f.view(3, 42);
        assert_eq!(f.next(t0 + ms(300)), Next::At(t0 + Duration::from_secs(1)));
        assert_eq!(
            f.next(t0 + Duration::from_secs(1)),
            Next::Send(FollowOp::Text { buf: 3 })
        );
    }

    #[test]
    fn one_call_at_a_time_and_pulls_coalesce() {
        let t0 = Instant::now();
        let mut f = following(t0);
        f.view(3, 42);
        assert_eq!(f.next(t0), Next::Send(FollowOp::Text { buf: 3 }));
        f.view(3, 43);
        f.view(3, 44);
        let reveal = |token| FollowOp::Reveal {
            buf: 3,
            line: 10,
            token,
            re: token,
        };
        let jump = FollowOp::Jump {
            buf: 3,
            line: 5,
            col: 0,
            focus: false,
            re: 9,
        };
        assert_eq!(f.park(reveal(1)), None);
        assert_eq!(f.park(jump.clone()), None);
        assert_eq!(f.next(t0 + ms(10)), Next::Idle, "the pull is still out");
        f.done(
            Done::Pulled {
                buf: 3,
                tick: 42,
                bytes: 100,
            },
            t0 + ms(10),
        );
        f.page_has(3, 42);
        assert_eq!(f.next(t0 + ms(20)), Next::Send(reveal(1)));
        assert_eq!(f.next(t0 + ms(20)), Next::Idle);
        f.done(Done::Other, t0 + ms(20));
        assert_eq!(f.next(t0 + ms(30)), Next::Send(jump));
        f.done(Done::Other, t0 + ms(30));
        assert_eq!(f.next(t0 + ms(40)), Next::At(t0 + ms(210)));
        assert_eq!(f.next(t0 + ms(210)), Next::Send(FollowOp::Text { buf: 3 }));
        f.done(
            Done::Pulled {
                buf: 3,
                tick: 44,
                bytes: 100,
            },
            t0 + ms(210),
        );
        f.page_has(3, 44);
        assert_eq!(f.next(t0 + ms(500)), Next::Idle, "one pull covered both ticks");
    }

    #[test]
    fn only_the_newest_parked_reveal_is_kept() {
        let t0 = Instant::now();
        let mut f = following(t0);
        f.view(3, 41);
        assert_eq!(f.next(t0), Next::Send(FollowOp::Text { buf: 3 }));
        let reveal = |re| FollowOp::Reveal {
            buf: 3,
            line: 10 * re as u32,
            token: re,
            re,
        };
        let jump = FollowOp::Jump {
            buf: 3,
            line: 5,
            col: 0,
            focus: true,
            re: 3,
        };
        assert_eq!(f.park(reveal(1)), None);
        assert_eq!(f.park(reveal(2)), Some(1));
        assert_eq!(f.park(jump.clone()), None);
        assert_eq!(f.park(reveal(4)), Some(2));
        f.done(
            Done::Pulled {
                buf: 3,
                tick: 41,
                bytes: 100,
            },
            t0,
        );
        f.page_has(3, 41);
        assert_eq!(f.next(t0), Next::Send(reveal(4)), "in the first reveal's place");
        f.done(Done::Other, t0);
        assert_eq!(f.next(t0), Next::Send(jump));
        f.done(Done::Other, t0);
        assert_eq!(f.next(t0), Next::Idle);
    }

    #[test]
    fn a_replaced_documents_ops_are_taken_back_and_the_outstanding_one_kept() {
        let t0 = Instant::now();
        let mut f = following(t0);
        let reveal = FollowOp::Reveal {
            buf: 3,
            line: 10,
            token: 1,
            re: 1,
        };
        f.park(reveal.clone());
        assert_eq!(f.next(t0), Next::Send(reveal.clone()));
        f.park(FollowOp::OpenLink {
            path: PathBuf::from("/p/a.md"),
            line: None,
            re: 2,
        });
        f.park(FollowOp::Jump {
            buf: 3,
            line: 5,
            col: 0,
            focus: false,
            re: 3,
        });
        assert_eq!(f.drop_page_ops(), [2, 3]);
        assert_eq!(f.outstanding(), Some(&reveal));
        f.done(Done::Other, t0);
        assert_eq!(f.next(t0), Next::Idle);
    }

    #[test]
    fn an_answer_behind_the_newest_view_pulls_once_more() {
        let t0 = Instant::now();
        let mut f = following(t0);
        f.view(3, 41);
        assert_eq!(f.next(t0), Next::Send(FollowOp::Text { buf: 3 }));
        f.view(3, 42);
        f.done(
            Done::Pulled {
                buf: 3,
                tick: 41,
                bytes: 100,
            },
            t0 + ms(5),
        );
        f.page_has(3, 41);
        assert_eq!(f.next(t0 + ms(10)), Next::At(t0 + ms(205)));
        assert_eq!(f.next(t0 + ms(205)), Next::Send(FollowOp::Text { buf: 3 }));
        f.done(
            Done::Pulled {
                buf: 3,
                tick: 42,
                bytes: 100,
            },
            t0 + ms(206),
        );
        f.page_has(3, 42);
        assert_eq!(f.next(t0 + ms(900)), Next::Idle);
    }

    #[test]
    fn a_pull_that_answers_a_newer_tick_than_its_view_is_not_pulled_again() {
        let t0 = Instant::now();
        let mut f = following(t0);
        f.view(3, 41);
        assert_eq!(f.next(t0), Next::Send(FollowOp::Text { buf: 3 }));
        // An edit landed before the pull ran, and the view line for tick 42 never came.
        f.done(
            Done::Pulled {
                buf: 3,
                tick: 42,
                bytes: 100,
            },
            t0 + ms(5),
        );
        f.page_has(3, 42);
        assert_eq!(f.next(t0 + ms(10)), Next::Idle);
        assert_eq!(f.next(t0 + ms(900)), Next::Idle);
        f.view(3, 43);
        assert_eq!(f.next(t0 + ms(900)), Next::Send(FollowOp::Text { buf: 3 }));
    }

    #[test]
    fn a_new_session_asks_for_install_again_and_marks_the_outstanding_call_stale() {
        let t0 = Instant::now();
        let mut f = following(t0);
        f.view(3, 41);
        assert_eq!(f.next(t0), Next::Send(FollowOp::Text { buf: 3 }));
        assert!(!f.is_stale());
        assert!(f.session(Some(2)));
        assert!(f.is_stale());
        assert_eq!(f.current_session(), Some(2));
        f.done(Done::Failed, t0);
        assert_eq!(f.outstanding(), None);
        assert_eq!(
            f.next(t0),
            Next::Send(FollowOp::Install),
            "a stale failure does not rest the queue"
        );
        f.done(Done::Installed, t0);
        assert_eq!(f.next(t0), Next::Send(FollowOp::Follow(true)), "still wanted");
        f.done(Done::Followed(true), t0);
        assert_eq!(f.next(t0), Next::Idle, "the old editor's view is forgotten");
        f.view(1, 3);
        assert_eq!(f.next(t0), Next::Send(FollowOp::Text { buf: 1 }));
        f.done(Done::Other, t0);
        assert!(f.session(None));
        assert_eq!(f.next(t0 + ms(1)), Next::Idle);
    }

    #[test]
    fn a_stale_answer_changes_nothing() {
        let t0 = Instant::now();
        let mut f = FollowState::new();
        f.session(Some(1));
        assert_eq!(f.next(t0), Next::Send(FollowOp::Install));
        f.session(Some(2));
        f.done(Done::Installed, t0);
        assert_eq!(
            f.next(t0),
            Next::Send(FollowOp::Install),
            "the install was of the old editor"
        );
    }

    #[test]
    fn an_early_install_is_asked_again_250_ms_later_or_at_once_after_entered() {
        let t0 = Instant::now();
        let mut f = FollowState::new();
        f.session(Some(1));
        assert_eq!(f.next(t0), Next::Send(FollowOp::Install));
        f.done(Done::Early, t0);
        assert_eq!(f.next(t0), Next::At(t0 + ms(250)));
        assert_eq!(f.next(t0 + ms(250)), Next::Send(FollowOp::Install));
        f.done(Done::Early, t0 + ms(250));
        f.entered();
        assert_eq!(f.next(t0 + ms(260)), Next::Send(FollowOp::Install));
        f.done(Done::Installed, t0 + ms(260));
        assert_eq!(f.next(t0 + ms(260)), Next::Idle, "nothing else wanted");
    }

    #[test]
    fn vimenter_reported_while_the_install_is_out_retries_its_early_answer_at_once() {
        let t0 = Instant::now();
        let mut f = FollowState::new();
        f.session(Some(1));
        assert_eq!(f.next(t0), Next::Send(FollowOp::Install));
        f.entered();
        f.done(Done::Early, t0);
        assert_eq!(f.next(t0), Next::Send(FollowOp::Install));
        f.done(Done::Early, t0);
        assert_eq!(f.next(t0), Next::At(t0 + ms(250)), "only once per report");
    }

    #[test]
    fn a_failed_call_rests_the_queue_before_the_next() {
        let t0 = Instant::now();
        let mut f = FollowState::new();
        f.session(Some(1));
        assert_eq!(f.next(t0), Next::Send(FollowOp::Install));
        f.done(Done::Failed, t0);
        assert_eq!(f.next(t0 + ms(100)), Next::At(t0 + ms(250)));
        assert_eq!(f.next(t0 + ms(250)), Next::Send(FollowOp::Install));
    }

    #[test]
    fn after_teardown_nothing_is_sent() {
        let t0 = Instant::now();
        let mut f = following(t0);
        f.view(3, 41);
        f.park(FollowOp::Teardown);
        assert_eq!(f.next(t0), Next::Send(FollowOp::Teardown), "ahead of a due pull");
        f.done(Done::Other, t0);
        assert!(f.stopped());
        f.view(3, 50);
        f.on_screen(false);
        assert_eq!(f.next(t0 + ms(500)), Next::Idle);
        f.session(Some(2));
        assert_eq!(f.next(t0 + ms(600)), Next::Idle, "not even in a new editor");
    }

    #[test]
    fn a_teardown_with_nothing_installed_sends_nothing() {
        let t0 = Instant::now();
        let mut f = FollowState::new();
        f.session(Some(1));
        assert_eq!(f.next(t0), Next::Send(FollowOp::Install));
        f.done(Done::Early, t0);
        f.park(FollowOp::Teardown);
        assert_eq!(f.next(t0 + ms(300)), Next::Idle);
        assert!(f.stopped());
    }

    #[test]
    fn a_gone_buffer_is_not_pulled_again() {
        let t0 = Instant::now();
        let mut f = following(t0);
        f.view(3, 41);
        assert_eq!(f.next(t0), Next::Send(FollowOp::Text { buf: 3 }));
        f.done(Done::Gone { buf: 3 }, t0);
        assert_eq!(f.next(t0 + ms(500)), Next::Idle);
    }

    #[test]
    fn idle_asks_for_no_time() {
        let t0 = Instant::now();
        let mut f = following(t0);
        f.on_screen(false);
        assert_eq!(f.next(t0), Next::Send(FollowOp::Follow(false)));
        f.done(Done::Followed(false), t0);
        f.view(3, 41);
        assert_eq!(f.next(t0), Next::Idle, "not on screen, nothing parked");
        f.on_screen(true);
        assert_eq!(f.next(t0), Next::Send(FollowOp::Follow(true)));
        f.done(Done::Followed(true), t0);
        assert_eq!(
            f.next(t0),
            Next::Send(FollowOp::Text { buf: 3 }),
            "back on screen, the tick moved"
        );
        f.done(
            Done::Pulled {
                buf: 3,
                tick: 41,
                bytes: 10,
            },
            t0,
        );
        f.page_has(3, 41);
        assert_eq!(
            f.next(t0 + ms(1)),
            Next::Idle,
            "on screen, the page has the newest tick"
        );
    }
}
