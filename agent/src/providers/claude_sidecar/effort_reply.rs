//! The one way this host can know a Claude session's reasoning effort. The wire carries none: `SessionReady`
//! has no such field, and neither has the CLI's own `system/init`. `CreateSession` sends none either, so
//! the session runs at whatever the user's settings say, which is the CLI's to know. What is left is the CLI saying it
//! out loud: its reply to `/model` names the effort, and its reply to `/effort <level>` confirms the level.
//!
//! A reply is matched only for a message that was one of those commands, since the same words in an ordinary reply
//! are the model's, not the CLI's. A model can change what the effort is, so a `/model` reply that does not
//! name one leaves the effort unknown. Showing nothing is always the fallback: a level that was
//! true before the command is a guess after it.

/// What a sent message asks of the CLI about its effort.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum EffortAsk {
    /// `/model`, with or without an argument.
    Model,
    /// `/effort <level>`. A bare `/effort` answers only its usage, so it is no ask at all.
    SetEffort,
}

/// The ask `text`, a message on its way to the CLI, makes, if any.
pub(super) fn ask_of(text: &str) -> Option<EffortAsk> {
    let mut words = text.split_whitespace();
    match (words.next(), words.next()) {
        (Some("/model"), _) => Some(EffortAsk::Model),
        (Some("/effort"), Some(_)) => Some(EffortAsk::SetEffort),
        _ => None,
    }
}

/// The level `reply` states in answer to `ask`, or `None` when it states none. The whole reply is the CLI's own
/// sentence, so it is read from its opening words and nowhere else.
pub(super) fn reported(ask: EffortAsk, reply: &str) -> Option<String> {
    let reply = reply.trim_start();
    match ask {
        EffortAsk::SetEffort => level(reply.strip_prefix("Set effort level to ")?),
        // The bare command answers `Current model: `Opus` (effort: high)`, and a choice `Set model to Opus with high effort`.
        EffortAsk::Model if reply.starts_with("Current model: ") => level(reply.split_once("(effort: ")?.1),
        EffortAsk::Model if reply.starts_with("Set model to ") => {
            let (_, rest) = reply.split_once(" with ")?;
            level(rest).filter(|word| rest.starts_with(&format!("{word} effort")))
        }
        EffortAsk::Model => None,
    }
}

/// The word `text` opens with, if it can be a level: this reads the CLI's words, it does not list its levels, so a
/// new one shows as it is. Anything longer or stranger is not one, and is not drawn on the band.
fn level(text: &str) -> Option<String> {
    let word = text.split(|c: char| !c.is_ascii_alphanumeric()).next()?;
    (!word.is_empty() && word.len() <= MAX_LEVEL_CHARS && word.chars().all(|c| c.is_ascii_lowercase()))
        .then(|| word.to_string())
}

const MAX_LEVEL_CHARS: usize = 16;

#[cfg(test)]
mod tests {
    use super::*;

    const MODEL_REPLY: &str = "Current model: `Haiku 4.5` (effort: high)\nUsage: /model <name>. Available: sonnet, opus, default, or a full model ID.";

    #[test]
    fn only_the_commands_that_state_an_effort_are_asks() {
        for text in ["/model", "  /model  ", "/model sonnet"] {
            assert_eq!(ask_of(text), Some(EffortAsk::Model), "{text}");
        }
        for text in ["/effort low", "/effort  max\n"] {
            assert_eq!(ask_of(text), Some(EffortAsk::SetEffort), "{text}");
        }
        for text in ["/effort", "/models", "what is /model", "/compact", ""] {
            assert_eq!(ask_of(text), None, "{text}");
        }
    }

    #[test]
    fn reads_the_level_from_the_clis_own_replies() {
        assert_eq!(reported(EffortAsk::Model, MODEL_REPLY).as_deref(), Some("high"));
        assert_eq!(
            reported(
                EffortAsk::SetEffort,
                "Set effort level to low (this session only): Quick, straightforward implementation"
            )
            .as_deref(),
            Some("low")
        );
        assert_eq!(
            reported(EffortAsk::SetEffort, "Set effort level to xhigh: Comprehensive").as_deref(),
            Some("xhigh")
        );
        assert_eq!(
            reported(
                EffortAsk::Model,
                "Set model to Sonnet 5.5 for this session only with high effort"
            )
            .as_deref(),
            Some("high")
        );
    }

    #[test]
    fn the_words_count_only_as_the_clis_own_sentence() {
        assert_eq!(reported(EffortAsk::Model, "I think it runs (effort: max) here"), None);
        assert_eq!(reported(EffortAsk::Model, "Done with high effort"), None);
        assert_eq!(
            reported(EffortAsk::SetEffort, "Sure. Set effort level to max: done"),
            None
        );
    }

    #[test]
    fn a_reply_that_states_no_level_leaves_it_unknown() {
        assert_eq!(reported(EffortAsk::Model, "Set model to Sonnet 5.5"), None);
        assert_eq!(reported(EffortAsk::Model, "Set model to Sonnet with care"), None);
        assert_eq!(
            reported(
                EffortAsk::SetEffort,
                "Invalid argument: bogus. Valid options are: low, medium"
            ),
            None
        );
        assert_eq!(
            reported(
                EffortAsk::SetEffort,
                "Effort set to auto for this session, but CLAUDE_CODE_EFFORT_LEVEL=low still controls this session"
            ),
            None
        );
    }

    #[test]
    fn what_cannot_be_a_level_never_reaches_the_band() {
        assert_eq!(
            reported(EffortAsk::Model, "Current model: `X` (effort: \u{202e}high)"),
            None
        );
        assert_eq!(reported(EffortAsk::Model, "Current model: `X` (effort: High)"), None);
        assert_eq!(
            reported(EffortAsk::SetEffort, &format!("Set effort level to {}", "x".repeat(40))),
            None
        );
    }
}
