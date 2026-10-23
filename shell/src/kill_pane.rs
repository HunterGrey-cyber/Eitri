//! Convert the editor's request watch into the facts used by the close rules.

use eitri_core::wm::close::{NvimAnswer, NvimReply};
use neovide_editor::CallWatch;

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

    #[test]
    fn nvim_answer_reads_what_the_rules_read() {
        let now = Instant::now();
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
        let asked = Asked {
            generation: 1,
            quit: EditorQuit::Window { confirmed: None },
            revealed: false,
            sent: true,
            lent_by: None,
        };
        let cases = [
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
        ];
        for (name, watch, state, request) in cases {
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
