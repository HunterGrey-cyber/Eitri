//! Convert the editor's request watch, and the pane's other facts, into what the quit flow reads.

use eitri_core::wm::close::{NvimAnswer, NvimReply};
use eitri_core::wm::EditorFacts;
use neovide_editor::CallWatch;

/// The editor pane's facts as the quit flow reads them, copied from its getters, none of which waits: whether nvim
/// runs, has exited, exits with its process still alive, is still to launch it, and how its last watched request
/// stands.
pub(crate) fn editor_facts(
    running: bool,
    exited: bool,
    exit_pending: bool,
    starting: bool,
    watch: Option<&CallWatch>,
) -> EditorFacts {
    EditorFacts {
        running,
        exited,
        exit_pending,
        starting,
        watch: watch.map(nvim_answer),
    }
}

pub(crate) fn nvim_answer(watch: &CallWatch) -> NvimAnswer {
    NvimAnswer {
        sent_at: watch.sent_at,
        done: watch.done,
        returned: watch.returned,
        last: watch.last_answer.as_ref().map(|reply| NvimReply {
            at: reply.at,
            mode: reply.mode.clone(),
            blocking: reply.blocking,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::wm::close::{
        nvim_state, quit_request, Asked, EditorQuit, NvimState, QuitRequest, NVIM_QUIT_PATIENCE,
    };
    use std::time::{Duration, Instant};

    // Reading the editor's watch directly keeps the conversion checked against its original facts.
    fn watch_state(watch: Option<&CallWatch>, now: Instant) -> NvimState {
        let Some(watch) = watch else {
            return NvimState::Returned;
        };
        if watch.done {
            return NvimState::Returned;
        }
        if now.saturating_duration_since(watch.last_alive()) >= NVIM_QUIT_PATIENCE {
            return NvimState::NotAnswering;
        }
        match &watch.last_answer {
            Some(answer) if answer.mode == "r?" => NvimState::Asking,
            Some(answer) if answer.blocking => NvimState::WaitingForKey,
            _ => NvimState::Busy,
        }
    }

    fn watch_request(asked: Option<&Asked>, watch: Option<&CallWatch>, now: Instant) -> QuitRequest {
        if asked.is_none() {
            return QuitRequest::Ask;
        }
        match watch_state(watch, now) {
            NvimState::Returned => QuitRequest::Ask,
            NvimState::NotAnswering => QuitRequest::Kill,
            state => QuitRequest::AnswerThere(state),
        }
    }

    pub(super) type Case = (&'static str, Option<CallWatch>, NvimState, QuitRequest);

    /// A watch in each state a quit request can be in, with what the rules make of it.
    pub(super) fn cases(now: Instant) -> Vec<Case> {
        let sent_at = now - NVIM_QUIT_PATIENCE - Duration::from_secs(1);
        let recent = now - Duration::from_millis(250);
        let old = now - NVIM_QUIT_PATIENCE;
        let watch = |done, returned, last_answer| {
            Some(CallWatch {
                sent_at,
                done,
                returned,
                last_answer,
            })
        };
        let reply = |at, mode: &str, blocking| {
            Some(neovide_editor::NvimMode {
                at,
                mode: mode.into(),
                blocking,
            })
        };
        vec![
            ("no watch", None, NvimState::Returned, QuitRequest::Ask),
            (
                "returned",
                watch(true, true, None),
                NvimState::Returned,
                QuitRequest::Ask,
            ),
            (
                "never answered",
                watch(false, false, None),
                NvimState::NotAnswering,
                QuitRequest::Kill,
            ),
            (
                "asking",
                watch(false, false, reply(recent, "r?", false)),
                NvimState::Asking,
                QuitRequest::AnswerThere(NvimState::Asking),
            ),
            (
                "asking while blocking",
                watch(false, false, reply(recent, "r?", true)),
                NvimState::Asking,
                QuitRequest::AnswerThere(NvimState::Asking),
            ),
            (
                "waiting for key",
                watch(false, false, reply(recent, "n", true)),
                NvimState::WaitingForKey,
                QuitRequest::AnswerThere(NvimState::WaitingForKey),
            ),
            (
                "busy",
                watch(false, false, reply(recent, "n", false)),
                NvimState::Busy,
                QuitRequest::AnswerThere(NvimState::Busy),
            ),
            (
                "stopped answering",
                watch(false, false, reply(old, "r?", true)),
                NvimState::NotAnswering,
                QuitRequest::Kill,
            ),
            (
                "exited",
                watch(true, false, reply(old, "r?", true)),
                NvimState::Returned,
                QuitRequest::Ask,
            ),
            (
                "cancelled",
                watch(true, true, reply(old, "r?", true)),
                NvimState::Returned,
                QuitRequest::Ask,
            ),
        ]
    }

    pub(super) fn asked() -> Asked {
        Asked {
            generation: 1,
            quit: EditorQuit::Window { confirmed: None },
            revealed: false,
            sent: true,
            killed: false,
            lent_by: None,
        }
    }

    #[test]
    fn nvim_answer_reads_what_the_rules_read() {
        let now = Instant::now();
        let asked = asked();
        for (name, watch, state, request) in cases(now) {
            let answer = watch.as_ref().map(nvim_answer);
            if let (Some(watch), Some(answer)) = (&watch, &answer) {
                assert_eq!(answer.sent_at, watch.sent_at, "{name}");
                assert_eq!(answer.done, watch.done, "{name}");
                assert_eq!(answer.returned, watch.returned, "{name}");
                assert_eq!(answer.last_alive(), watch.last_alive(), "{name}");
                assert_eq!(
                    answer.last.as_ref().map(|a| (a.at, a.mode.as_str(), a.blocking)),
                    watch.last_answer.as_ref().map(|a| (a.at, a.mode.as_str(), a.blocking)),
                    "{name}"
                );
                // Only an ended request without a returned answer lets the pending close stand.
                assert_eq!(answer.done && !answer.returned, watch.done && !watch.returned, "{name}");
            }
            assert_eq!(watch_state(watch.as_ref(), now), state, "{name}");
            assert_eq!(nvim_state(answer.as_ref(), now), state, "{name}");
            assert_eq!(watch_request(Some(&asked), watch.as_ref(), now), request, "{name}");
            assert_eq!(quit_request(Some(&asked), answer.as_ref(), now), request, "{name}");
            assert_eq!(watch_request(None, watch.as_ref(), now), QuitRequest::Ask, "{name}");
            assert_eq!(quit_request(None, answer.as_ref(), now), QuitRequest::Ask, "{name}");
        }
    }
}

#[cfg(test)]
mod facts_tests {
    use super::*;
    use eitri_core::wm::close::quit_request;
    use std::time::Instant;

    /// The pane's facts carry its watch one to one, in every state a quit request can be in, and the rules read the
    /// same from the facts as from the watch; the pane's four flags pass as they are, starting among them.
    #[test]
    fn editor_facts_read_what_the_quit_rules_read() {
        let now = Instant::now();
        let asked = tests::asked();
        for (name, watch, _, request) in tests::cases(now) {
            let facts = editor_facts(true, false, false, false, watch.as_ref());
            assert_eq!(facts.watch, watch.as_ref().map(nvim_answer), "{name}");
            assert_eq!(quit_request(Some(&asked), facts.watch.as_ref(), now), request, "{name}");
        }
        for (running, exited, exit_pending, starting) in [
            (true, false, false, false),
            (false, true, false, false),
            (false, false, true, false),
            (false, false, false, true),
        ] {
            let facts = editor_facts(running, exited, exit_pending, starting, None);
            assert_eq!(
                (
                    facts.running,
                    facts.exited,
                    facts.exit_pending,
                    facts.starting,
                    facts.watch
                ),
                (running, exited, exit_pending, starting, None)
            );
        }
    }
}
