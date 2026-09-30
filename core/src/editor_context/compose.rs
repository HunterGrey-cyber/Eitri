//! Turning what the editor is showing into the text that rides with one turn.
//!
//! Pure: no socket, no nvim, no GTK. The transport half is [`super::feed`]; this half is what
//! decides what the model actually reads, and it is the half worth testing hardest.

/// A live visual selection, as nvim reports it while the selection still exists.
///
/// `text` is the selected lines themselves, not a coordinate pair, and that is not a nicety. The
/// CLI has no pull path and no dirty check -- `getCurrentSelection`, `getLatestSelection` and
/// `checkDocumentDirty` occur **zero** times in the 2.1.272 binary -- so on a buffer with unsaved
/// changes a line range is not merely stale, it is unfalsifiable: nothing downstream can discover
/// that the file on disk says something else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub start_line: u32,
    pub end_line: u32,
    pub text: String,
}

/// What the editor was showing when the turn was sent.
///
/// `selection: None` is a state of its own, never "an empty selection". Measured on real nvim: with
/// no selection ever made, `getpos('v')` silently equals `getpos('.')` and `getregion` returns a
/// plausible one-character region rather than erroring or returning empty -- so a reader that asks
/// `getregion` first and infers from its answer cannot tell "one character" from "nothing". Two
/// surveyed plugins degrade an unset selection into *the whole buffer*; here that would fire on
/// every turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorContext {
    /// Absolute path of the focused buffer's file. Empty for a buffer with no file, which is why
    /// [`compose_turn_text`] checks it rather than assuming a name exists.
    pub file: String,
    pub selection: Option<Selection>,
}

/// The CLI's own cap on how much selected text it forwards, and its own marker.
///
/// Both read byte-for-byte out of `~/.local/share/claude/approved` (2.1.272, the build production
/// execs): `N2r = 2000`, and the helper appends `"\n... (truncated)"`. Copied rather than chosen so
/// a selection reaches the model looking exactly like one from any other editor that integrates
/// with this CLI.
pub const CONTENT_LIMIT: usize = 2000;
pub const TRUNCATION_MARKER: &str = "\n... (truncated)";

/// The built-in slash commands Claude Code 2.1.283 defines as `type: "local"` under a fixed name,
/// with their aliases -- the commands its own code runs, rather than handing their argument to the
/// model as prompt text.
///
/// Whole-branch review finding 1 (v1 trial, 2026-09-28): a turn whose first word is one of these
/// never carries the context block. The CLI's router splits a slash turn at the first whitespace
/// (`ZEe` in the 2.1.283 bundle: `name` up to the first `\s`, `args` everything after it, trimmed)
/// and each of these commands parses `args` itself, so a block appended after the user's text
/// becomes part of the argument -- the model name (`/model haiku\n\nThe user opened ...`, which
/// `/model` rejects), the effort level ("Invalid argument"), a session name (`/rename`, `/clear
/// <name>`), a path (`/add-dir`), a `key=value` (`/config`), compaction instructions (`/compact`) or a
/// goal (`/goal`). None of them wants it.
///
/// **How it was read, not remembered.** `/scratch/auto-parity/cli-2.1.283.strings`: every object
/// literal carrying `type:"local"`, in any key order, and its `aliases:[…]`. The first extraction
/// matched only the literal `{type:"local",name:"…"` and missed four (v1 trial fix round 2): two whose
/// `name` comes before `type` (`keybindings`; `rewind`, with `checkpoint` and `undo`) and two whose name
/// is a variable (`Sae` = `low-priority`, `MOt` = `claim-credit`). A re-read should search the same
/// way. The lookup is exact (`e.name===n || e.aliases?.includes(n)`), and so is
/// [`names_a_cli_local_command`]. Included whether or not the entry says `supportsNonInteractive`
/// (nine do not: `claim-credit`, `install-slack-app`, `keybindings`, `low-priority`, `radio`,
/// `rewind`, `stickers`, `update`, `voice`) -- a headless CLI answers those without the model either,
/// so a block there is never read.
///
/// **Two kinds of `type: "local"` command this list cannot name, left off on purpose.**
/// - The Claude-for-Enterprise upsell stubs `z0()` builds (`ultraplan`, `teleport`/`tp`,
///   `remote-control`/`rc`, `schedule`/`routines`, `autofix-pr`): each answers an upsell message
///   without reading its argument, and a name like `schedule` is one a user's skill can have too,
///   whose argument is prompt text that should keep the block.
/// - A command a plugin registers at run time through its hooks module (`$.command.register`, built
///   by `Jgn` as `type:"local"`, `supportsNonInteractive:!0`, `loadedFrom:"plugin"`): its
///   `command.run` hook receives `args` the same way, so `/choose-profile staging` with a file open
///   reaches it as `staging` plus the block. Its name is whatever the plugin chose, and Eitri never
///   sees the CLI's command table: `system/init`'s `slash_commands` lists names with no type, and the
///   sidecar does not forward it. Hooks modules are early access in 2.1.283 -- off for installed
///   plugins unless `CLAUDE_CODE_ENABLE_FUNCTION_HOOKS` is set or the `tengu_plugin_hooks_modules`
///   rollout turns them on (built-in plugins load regardless; the one built-in registration found,
///   the diff panel's `/diff`, takes no argument). Still open: closing it needs the command's type on
///   the wire, or the context sent apart from the text.
///
/// **What keeps the block, and why.** The CLI's `type: "prompt"` commands (`/init`, `/review`,
/// `/security-review`, `/doctor`, and every skill) substitute their argument into a prompt the model
/// reads, which is where the block goes on any ordinary turn; an unknown `/word` is either a
/// user's skill or reaches the model as text. Its `type: "local-jsx"` commands (the interactive
/// pickers and wizards: `/help`, `/export`, ...) reply "isn't available in this environment"
/// headless without reading their argument (`docs/canonical/2026-09-27-slash-commands.md`), so
/// they are left out rather than listed for no effect -- `/model` and `/effort` have a `local-jsx`
/// twin too, and it is their `local` definition a headless CLI runs.
///
/// A newer CLI can add a local command this list does not name; that command then gets the block
/// until this list is re-read -- the failure this finding was, for one command, not a wider one.
pub const CLI_LOCAL_COMMANDS: &[&str] = &[
    "__remote-workflow",
    "add-dir",
    "advisor",
    "agents",
    "auto-mode-setup",
    "autocompact",
    "claim-credit",
    "clear",
    "reset",
    "new",
    "color",
    "compact",
    "config",
    "settings",
    "context",
    "design-consent",
    "design-revoke",
    "effort",
    "exit",
    "extra-usage",
    "fast",
    "focus",
    "goal",
    "heapdump",
    "import",
    "install-slack-app",
    "keybindings",
    "list-agents",
    "low-priority",
    "peers",
    "mcp",
    "model",
    "output-style",
    "pause-memory",
    "memory-pause",
    "toggle-memory",
    "plugin-types",
    "radio",
    "recap",
    "reload-plugins",
    "reload-skills",
    "rewind",
    "checkpoint",
    "undo",
    "rename",
    "name",
    "skill-doctor",
    "stickers",
    "stop",
    "ultrareview",
    "update",
    "restart",
    "usage",
    "cost",
    "stats",
    "usage-credits",
    "version",
    "voice",
    "workflow-launch-exec",
];

/// Whether the CLI would run `user_text` as one of [`CLI_LOCAL_COMMANDS`], argument or not: the
/// CLI's own parse -- the text trimmed, a leading `/`, the name up to the first whitespace -- and
/// its own exact match (`/Model` is not `/model` there, so not here). Replaces owner trial item 2's
/// fix-round `is_bare_slash_picker_command`, which exempted only a bare `/model`/`/effort` and so
/// left the argument form -- what the picker itself sends -- broken.
fn names_a_cli_local_command(user_text: &str) -> bool {
    let Some(rest) = user_text.trim().strip_prefix('/') else {
        return false;
    };
    let name = rest.split(char::is_whitespace).next().unwrap_or("");
    CLI_LOCAL_COMMANDS.contains(&name)
}

/// Appends what the editor is showing to the user's own text.
///
/// The two templates are the CLI's, verbatim. The trailing hedge sentence is **load-bearing**: with
/// it, a cursor position is context the model may ignore; without it, a passive position reads as
/// an instruction, and the model starts acting on whatever file happens to be open.
///
/// Returns the user's text unchanged when there is nothing worth saying -- no context at all, or a
/// buffer with no file. A turn that carries a context block saying nothing is worse than one
/// carrying none: it spends the model's attention and teaches the reader to ignore the block. Also
/// returns it unchanged for a turn the CLI runs as one of its own local commands, with or without an
/// argument ([`CLI_LOCAL_COMMANDS`]) -- appended there, the block becomes that command's argument.
pub fn compose_turn_text(user_text: &str, context: Option<&EditorContext>) -> String {
    let Some(context) = context else {
        return user_text.to_string();
    };
    if context.file.trim().is_empty() {
        return user_text.to_string();
    }
    if names_a_cli_local_command(user_text) {
        return user_text.to_string();
    }
    let block = match &context.selection {
        Some(selection) => format!(
            "The user selected the lines {} to {} from {}:\n{}\n\nThis may or may not be related to the current task.",
            selection.start_line,
            selection.end_line,
            context.file,
            truncate(&selection.text),
        ),
        // File name only, zero content -- the CLI's own asymmetry, and gemini-cli's. Sending the
        // whole buffer here is the degradation this module exists to avoid.
        None => format!(
            "The user opened the file {} in the IDE. This may or may not be related to the current task.",
            context.file
        ),
    };
    if user_text.is_empty() {
        return block;
    }
    format!("{user_text}\n\n{block}")
}

/// Truncates on a CHARACTER boundary, not a byte one.
///
/// `&s[..CONTENT_LIMIT]` panics mid-codepoint, and a selection of CJK or emoji reaches this
/// function as a matter of course in this project -- the editor's own IME work exists because the
/// owner types Chinese into it.
fn truncate(content: &str) -> String {
    if content.chars().count() <= CONTENT_LIMIT {
        return content.to_string();
    }
    let kept: String = content.chars().take(CONTENT_LIMIT).collect();
    format!("{kept}{TRUNCATION_MARKER}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor_context::strip_composed_block;

    fn ctx(file: &str, selection: Option<Selection>) -> EditorContext {
        EditorContext {
            file: file.to_string(),
            selection,
        }
    }

    #[test]
    fn a_selection_carries_its_lines_and_their_real_text() {
        let out = compose_turn_text(
            "why is this slow?",
            Some(&ctx(
                "/p/src/main.rs",
                Some(Selection {
                    start_line: 12,
                    end_line: 14,
                    text: "fn a() {}".into(),
                }),
            )),
        );
        assert_eq!(
            out,
            "why is this slow?\n\nThe user selected the lines 12 to 14 from /p/src/main.rs:\nfn a() {}\n\nThis may or may not be related to the current task."
        );
    }

    /// The asymmetry is the whole design: no selection means the file NAME, and nothing else.
    #[test]
    fn no_selection_sends_the_file_name_and_not_one_byte_of_content() {
        let out = compose_turn_text("hi", Some(&ctx("/p/src/main.rs", None)));
        assert_eq!(
            out,
            "hi\n\nThe user opened the file /p/src/main.rs in the IDE. This may or may not be related to the current task."
        );
    }

    /// Without the hedge a passive cursor position reads as an instruction. It is quoted here so
    /// that deleting it from the templates fails a test rather than quietly changing what the model
    /// believes it was asked to do.
    #[test]
    fn both_templates_carry_the_hedge_sentence() {
        const HEDGE: &str = "This may or may not be related to the current task.";
        let with = compose_turn_text(
            "q",
            Some(&ctx(
                "/f",
                Some(Selection {
                    start_line: 1,
                    end_line: 1,
                    text: "x".into(),
                }),
            )),
        );
        let without = compose_turn_text("q", Some(&ctx("/f", None)));
        assert!(with.ends_with(HEDGE), "{with}");
        assert!(without.ends_with(HEDGE), "{without}");
    }

    #[test]
    fn no_context_leaves_the_users_text_exactly_as_typed() {
        assert_eq!(compose_turn_text("just a question", None), "just a question");
    }

    /// A scratch buffer has no file. Saying "the user opened the file  in the IDE" is worse than
    /// saying nothing: it is a sentence that reads as information and carries none.
    #[test]
    fn a_buffer_with_no_file_contributes_nothing() {
        assert_eq!(compose_turn_text("q", Some(&ctx("", None))), "q");
        assert_eq!(
            compose_turn_text(
                "q",
                Some(&ctx(
                    "   ",
                    Some(Selection {
                        start_line: 1,
                        end_line: 2,
                        text: "x".into()
                    })
                ))
            ),
            "q"
        );
    }

    /// Owner trial item 2, fix round: without this check, a bare `/model` or `/effort` with a file
    /// open -- the ordinary case in an editor -- reached the CLI with a context block glued on right
    /// after it, which the CLI's local-command router reads as an argument, so it never replied with
    /// the text this panel's picker parses.
    #[test]
    fn a_bare_model_or_effort_command_gets_no_context_appended() {
        assert_eq!(
            compose_turn_text("/model", Some(&ctx("/p/src/main.rs", None))),
            "/model"
        );
        assert_eq!(
            compose_turn_text("/effort", Some(&ctx("/p/src/main.rs", None))),
            "/effort"
        );
        // Whitespace-only padding is still the command: the CLI trims before it looks for `/`.
        assert_eq!(
            compose_turn_text("  /model  ", Some(&ctx("/p/src/main.rs", None))),
            "  /model  "
        );
    }

    /// Whole-branch review finding 1 (2026-09-28, v1 trial): the argument form is broken the same
    /// way, and it is the form the picker itself sends (`chooseSlashOption`: `/model <choice>`,
    /// `/effort <level>`). The CLI takes everything after the command name as the argument,
    /// trimmed (`ZEe` in 2.1.283's bundle), so `/model haiku` with a file open reached it as the
    /// model name `haiku\n\nThe user opened the file ...`, and `/effort low` as an invalid level.
    /// This test used to pin exactly that broken shape as intended.
    #[test]
    fn a_local_command_with_an_argument_gets_no_context_either() {
        let selection = Some(Selection {
            start_line: 1,
            end_line: 2,
            text: "fn a() {}".into(),
        });
        for typed in ["/model sonnet", "/effort low", "/model haiku\n", "  /effort   max  "] {
            assert_eq!(compose_turn_text(typed, Some(&ctx("/p/src/main.rs", None))), typed);
            assert_eq!(
                compose_turn_text(typed, Some(&ctx("/p/src/main.rs", selection.clone()))),
                typed
            );
        }
    }

    /// The rule is the CLI's own `type: "local"` list, not just the two picker commands: every one of
    /// them parses its argument itself, so a block would become a session name (`/rename`,
    /// `/clear <name>`), a path (`/add-dir`), a `key=value` (`/config`), compaction instructions
    /// (`/compact`) or a goal (`/goal`). Aliases resolve to the same command in the CLI
    /// (`aliases.includes`), so they are covered too; the CLI's match is exact, and so is this one.
    #[test]
    fn every_cli_local_command_and_alias_is_sent_as_typed() {
        for typed in [
            "/rename refactor",
            "/name refactor",
            "/clear",
            "/reset",
            "/new fresh start",
            "/compact keep the test names",
            "/config autoCompact=false",
            "/settings autoCompact=false",
            "/add-dir ../other",
            "/goal all tests pass",
            "/usage",
            "/cost",
            "/output-style Concise",
            "/mcp reconnect",
        ] {
            assert_eq!(
                compose_turn_text(typed, Some(&ctx("/p/src/main.rs", None))),
                typed,
                "{typed}"
            );
        }
        assert!(CLI_LOCAL_COMMANDS.contains(&"model") && CLI_LOCAL_COMMANDS.contains(&"effort"));
    }

    /// v1 trial fix round 2 (re-review of finding 1): the first list came from the literal pattern
    /// `{type:"local",name:"…"`, which misses a definition with its `name` before its `type`
    /// (`keybindings`, `rewind` with `checkpoint`/`undo`) or a name held in a variable (`Sae` is
    /// `low-priority`, `MOt` is `claim-credit`). All four are built in and fixed by name.
    #[test]
    fn local_commands_defined_name_first_or_by_variable_are_sent_as_typed() {
        for typed in [
            "/keybindings",
            "/rewind",
            "/checkpoint",
            "/undo last",
            "/low-priority",
            "/claim-credit",
        ] {
            assert_eq!(
                compose_turn_text(typed, Some(&ctx("/p/src/main.rs", None))),
                typed,
                "{typed}"
            );
        }
    }

    /// Everything else still carries the context: ordinary text, a command whose argument becomes
    /// prompt text the model reads (`/review`, `/init`, a skill -- the CLI's `type: "prompt"`
    /// commands, where the block is the same context it is on any turn), an unknown `/word`, a
    /// slash that is not the first thing typed, and a local command's name in another case (the
    /// CLI's own match is case-sensitive, so `/Model` is not its model command). Also `/schedule`,
    /// one of the Claude-for-Enterprise upsell stubs (`z0()` in the bundle) left off the list: a stub
    /// ignores its argument, and the name is one a skill can have, whose argument is prompt text.
    #[test]
    fn prompt_commands_and_ordinary_text_still_get_context_appended() {
        const BLOCK: &str =
            "\n\nThe user opened the file /p/src/main.rs in the IDE. This may or may not be related to the current task.";
        for typed in [
            "/review this change",
            "/init",
            "/my-skill do the thing",
            "/schedule every morning",
            "/modelx",
            "/Model sonnet",
            "use /model sonnet later",
            "why is this slow?",
        ] {
            assert_eq!(
                compose_turn_text(typed, Some(&ctx("/p/src/main.rs", None))),
                format!("{typed}{BLOCK}"),
                "{typed}"
            );
        }
    }

    #[test]
    fn selected_text_longer_than_the_cli_s_own_limit_is_truncated_and_says_so() {
        // `Z`, not `a`: the templates themselves contain `a` ("related", "task"), so counting `a`
        // counts the frame as well as the content. The first draft of this test did exactly that
        // and failed against correct code.
        let long = "Z".repeat(CONTENT_LIMIT + 50);
        let out = compose_turn_text(
            "q",
            Some(&ctx(
                "/f",
                Some(Selection {
                    start_line: 1,
                    end_line: 9,
                    text: long,
                }),
            )),
        );
        assert!(
            out.contains(TRUNCATION_MARKER),
            "the marker must be present: {}",
            &out[..120]
        );
        assert_eq!(out.matches('Z').count(), CONTENT_LIMIT);
    }

    /// `&s[..2000]` panics mid-codepoint. The owner types Chinese into this editor; a selection of
    /// it is not an exotic case here.
    #[test]
    fn truncation_counts_characters_not_bytes_so_multibyte_text_cannot_panic() {
        let long = "字".repeat(CONTENT_LIMIT + 10);
        let out = compose_turn_text(
            "q",
            Some(&ctx(
                "/f",
                Some(Selection {
                    start_line: 1,
                    end_line: 2,
                    text: long,
                }),
            )),
        );
        assert_eq!(out.matches('字').count(), CONTENT_LIMIT);
        assert!(out.contains(TRUNCATION_MARKER));
    }

    /// Exactly at the limit is not truncated -- an off-by-one here adds a "(truncated)" to text that
    /// is complete, which is a lie the model has no way to check.
    #[test]
    fn content_exactly_at_the_limit_is_not_marked_truncated() {
        let exact = "Z".repeat(CONTENT_LIMIT);
        let out = compose_turn_text(
            "q",
            Some(&ctx(
                "/f",
                Some(Selection {
                    start_line: 1,
                    end_line: 2,
                    text: exact,
                }),
            )),
        );
        assert!(!out.contains(TRUNCATION_MARKER), "{out}");
    }

    /// An empty prompt with context still sends the context, and without a leading blank line.
    #[test]
    fn an_empty_prompt_still_carries_the_context_cleanly() {
        let out = compose_turn_text("", Some(&ctx("/f", None)));
        assert!(out.starts_with("The user opened"), "{out}");
    }

    /// The round trip the resume-history design (§3.1.2) requires, kept beside the forward cases so
    /// the two directions cannot be changed independently.
    ///
    /// It matters because the prompt text a transcript stores is the WIRE turn, not what the user
    /// typed: measured 2026-09-20, 2 of the 6 SDK-driven sessions on this machine end with a block
    /// this function appended. Rendering that as
    /// `AgentDomainEvent::UserPromptSubmitted` -- whose own doc says `text` is "the text the user
    /// typed, deliberately NOT the text that went on the wire" -- would show a file path and a
    /// selection nobody wrote.
    ///
    /// `strip_composed_block` lives in `agent`, not in this file, because `eitri-core` depends on
    /// `agent`; see this module's re-export for the full reason.
    #[test]
    fn stripping_the_composed_block_recovers_exactly_what_the_user_typed() {
        let selections = [
            None,
            Some(Selection {
                start_line: 1,
                end_line: 1,
                text: "one line".into(),
            }),
            Some(Selection {
                start_line: 12,
                end_line: 40,
                text: "fn a() {}\nfn b() {}\n\nfn c() {}".into(),
            }),
            // Blank selected text: the template still emits `:\n` with nothing after it.
            Some(Selection {
                start_line: 3,
                end_line: 3,
                text: String::new(),
            }),
            // Long enough that `compose_turn_text` appends its own truncation marker, which is part
            // of the block and must come off with it.
            Some(Selection {
                start_line: 1,
                end_line: 999,
                text: "字".repeat(CONTENT_LIMIT + 10),
            }),
        ];
        let texts = [
            "",
            "why is this slow?",
            "a prompt\n\nwith a blank line in it",
            "a prompt ending in the hedge: This may or may not be related to the current task.",
            // A prompt that quotes a whole composed block. Stripping must take off only the one
            // this call appends.
            "see: The user opened the file /x in the IDE. This may or may not be related to the current task.",
        ];
        let files = ["/p/src/main.rs", "/p/a b/c-d.rs", "/p/中文/文件.rs"];
        for text in texts {
            for file in files {
                for selection in &selections {
                    let context = ctx(file, selection.clone());
                    let composed = compose_turn_text(text, Some(&context));
                    assert_eq!(
                        strip_composed_block(&composed),
                        text,
                        "text={text:?} file={file:?} selection={selection:?}"
                    );
                }
            }
        }
    }

    /// The negative control for the round trip: text this function never touched comes back
    /// byte-for-byte. Without it, a `strip_composed_block` that returned `""` for everything would
    /// still pass the test above for the one input that is already empty.
    #[test]
    fn stripping_leaves_a_turn_that_carried_no_context_untouched() {
        for text in ["", "just a question", "The user opened a can of worms."] {
            let composed = compose_turn_text(text, None);
            assert_eq!(composed, text);
            assert_eq!(strip_composed_block(&composed), text);
        }
    }
}
