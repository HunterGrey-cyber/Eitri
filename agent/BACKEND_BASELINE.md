# Backend baseline — the legacy backend on Claude Code 2.1.292

Re-recorded 2026-10-08 on CLI 2.1.292 (TEST profile, the test-account wrapper). **Previous baseline: 2.1.266, 2026-09-09**
(Phase 0 of the Claude runtime/provider refactor); that record's invocation section went stale when the gate moved from
a `.claude/settings.local.json` file into argv, so this file replaces it rather than amending it.

This is the behavioural record of the legacy backend (the `claude --print --input-format stream-json` path, compiled only
with `--features legacy-backend`). The invocation below is a transcription of `agent/src/process.rs`'s
`AgentProcess::spawn_with_binary_loading` -- if that function changes, this file is stale and must be re-generated, not
hand-edited to guess at the new shape. The sidecar backend is not covered here; it has its own conformance suites
(`claude_sidecar_*`).

## Claude CLI version at baseline time

```
2.1.292 (Claude Code)
```

Every turn below ran under the TEST profile (the test-account wrapper); the model the CLI picked was `claude-sonnet-5-5`. The
`claude` on `PATH` there is the profile shim.

## Exact real CLI invocation (`agent/src/process.rs:789-823`, `AgentProcess::spawn_with_binary_loading`)

```
claude --print --input-format stream-json --output-format stream-json --verbose \
  --setting-sources <user,project,local | project,local | user | ""> \
  --permission-mode default \
  [--disallowedTools <comma-joined tool names>] \
  --settings '<hook JSON>'
```

- **`--setting-sources`** is always passed, the empty selection too (as an argument that is the empty string): without the
  flag the CLI would load every tier. The value comes from `agent/src/setting_sources.rs` (`for_session`,
  `cli_argument_for`): `user,project,local` for a trusted project with the user tier on (the default; what the runs below
  used), `project,local` with `agent.user_settings = false`, `user` for an untrusted project, `""` for both off.
- **`--permission-mode default`**, always. There is no mode parameter: bypass is Eitri answering `allow` above this crate's
  session API, never a CLI flag, and the CLI is never started in `auto` on this backend.
- **`--disallowedTools`** is appended only when the caller's slice is non-empty. `agent::disallowed_tools()` is always
  empty, so the product passes none; `real_multi_turn_conversation_in_one_process` still passes
  `CONSERVATIVE_DISALLOWED_TOOLS` (`Bash,Write,Edit,NotebookEdit`) to exercise the flag.
- **`--settings`** carries the `PreToolUse` gate as JSON in argv, so nothing is written into the project and no other
  conversation can reach it (`agent/src/settings.rs:45-72`, `hook_settings_arg`). serde_json orders the keys, so the value
  is exactly
  `{"hooks":{"PreToolUse":[{"hooks":[{"command":"EITRI_AGENT_HOOK_SOCKET=<socket> <agent-hook path>","timeout":600,"type":"command"}],"matcher":"*"}]}}`
  (`HOOK_TIMEOUT_SECONDS = 600`, `settings.rs:38`).
- A **preflight** runs first (`process.rs:512-549`, called at `:769`): `claude --settings '<hook JSON>' --version` in the
  project directory must exit 0, so a launcher that owns `--settings` fails with its own message before any session opens.
- Working directory is `project_dir`. stdin, stdout and stderr are all piped (`process.rs:815-817`); stdin stays open for
  the process's whole lifetime -- one JSON line per turn, not a fresh process per turn.
- The child inherits the window's environment. A configured `agent.account` is *not* applied to this spawn (only logged,
  `process.rs:740-746`).

## What `agent/tests/backend_conformance.rs` pins (real CLI, `#[ignore]`d, run explicitly)

Run on 2.1.292 with
`cargo test -p agent --features legacy-backend --test backend_conformance --test environment_conformance -- --ignored --test-threads=1 --nocapture`
(`--nocapture` matters: `environment_conformance` reports a missing or unusable `claude` as a `SKIPPED` line on stderr and still passes; none appeared in this run; after `cargo build -p agent --features legacy-backend --bin agent-hook`). **7 of 7 passed**: 6 in 76.75 s, 1 in 0.59 s.

| Test | Result | What it proves |
|---|---|---|
| `real_multi_turn_conversation_in_one_process` | ok | A single long-lived process carries context across turns without `--resume`. |
| `real_pretooluse_hook_allow_end_to_end` | ok | A real tool call's `PreToolUse` hook round-trips through `agent-hook` and an `allow` lets the tool run; the gated `tool_use_id` names a tool call in the same conversation. |
| `real_pretooluse_hook_deny_end_to_end` | ok | The deny half: the CLI blocks the tool and surfaces the deny reason verbatim in the tool result. |
| `real_two_sessions_in_the_same_project_dir_each_see_only_their_own_permission_hooks` | ok | Two sessions in one directory do not see each other's hook requests, and nothing is written under `.claude/`. This replaces the 2.1.266 baseline's `..._cross_wire_permission_hooks`, which pinned the since-fixed collision of the settings file. |
| `real_interrupt_mid_permission_denies_pending_requests_without_ending_the_session` | ok | `interrupt()` denies and clears pending permissions, the interrupted turn's `TurnCompleted` (outcome `Interrupted`) still arrives, and the session survives for a further turn. |
| `real_edit_under_the_auto_gate_carries_a_reviewable_request_and_then_writes_the_file` | ok | With the product's own (empty) tool deny list the model reaches for `Edit`, the gate stops it with a request carrying what a diff is drawn from, and approving it writes the file. (Added after the 2.1.266 baseline.) |
| `environment_conformance::claude_resolved_from_inherited_path_accepts_the_products_gate_bearing_settings_flag` | ok | The `claude` on `PATH` accepts the product's gate-bearing `--settings` value (`--version` probe, no tokens). |

## What the parsers do with 2.1.292's real output

One captured session (three turns: a plain reply, a turn that runs `echo`, a turn that thinks), run through the two parsers
by `agent/tests/stream_capture_conformance.rs` (`#[ignore]`d; it reads a capture named by `EITRI_CAPTURE_DIR` and spends no
tokens itself). The capture used the product's argv without `--settings` and with `--allowedTools 'Bash(echo:*)'`, since the
shape under test is the stream, not the gate.

**`--output-format stream-json` stdout through `agent::translate_line`** (22 lines): no line failed to parse, and every
`result` line became a `TurnFinished` (3 of 3).

| Wire line | Count | Becomes |
|---|---|---|
| `system`/`init` | 3 | `SessionStarted` -- one per turn of the same process, as in the earlier captures (`agent/CAPTURE_NOTES.md`) |
| `assistant` (`text` block) | 3 | `AssistantText` |
| `assistant` (`tool_use` block) | 1 | `ToolStarted` |
| `assistant` (`thinking` block) | 1 | `Thinking` -- see the note below |
| `user` (`tool_result` block) | 1 | `ToolResult` |
| `result`/`success` | 3 | `TurnFinished` |
| `rate_limit_event` | 2 | `RateLimit` |
| `system`/`hook_started` | 3 | `Unknown` (dropped by design); all three arrive at session start, before the first `init` |
| `system`/`hook_response` | 3 | `Unknown` (dropped by design); same |
| `system`/`thinking_tokens` | 2 | `Unknown` (dropped by design) |

- The three `Unknown` kinds are lifecycle notices, not conversation; the parser's catch-all exists for them. The `hook_*`
  pair is `SessionStart` hooks of the loaded settings tiers running (one `hook_response` carried ~8 KB of injected context,
  so a consumer that kept `Unknown`'s raw value would hold it). `thinking_tokens` is a running estimate
  (`estimated_tokens`, `estimated_tokens_delta`) that arrives before the thinking block.
- **The `thinking` block arrives with `"thinking": ""` and a `signature`**: in this capture (sonnet-5-5) the model's thinking text is not in the
  stream, so `AgentEvent::Thinking` carries an empty string. This parser reads the field as given; the projection ignores a
  `Thinking` delta, so no row is drawn today, and a consumer that starts drawing one must not show an empty row.
- An assistant message is written as one line per content block, all sharing one `message.id` (the thinking line and the text
  line of the third turn did), which is what `AssistantText.message_id` and the message-boundary logic rely on.
- stderr stayed empty for the whole session.

**The on-disk transcript through `agent::history::read_transcript_history`** (writer version `2.1.292`): nothing malformed,
oversized, orphaned or of an unknown type. 7 conversational items came out (3 prompts, 3 assistant texts, 1 tool call; the
tool result belongs to its call) as 8 events. The reader skipped 36 lines: 20 `attachment`, 3 `system`, 13 side records
(`queue-operation`, `atis-latch`, `last-prompt`, `cost-state`; this transcript holds no `ai-title` line), and dropped 1 content block (the thinking block). In this capture no line type, content-block type or
event kind appeared that either parser misreads or drops without saying so.

Nothing in this capture required a change to `agent/src/wire.rs` or `agent/src/history/`.

## What is pinned elsewhere, not re-tested here

- **Concurrent permission requests** (several pending at once, each answerable in any order): pinned at the pure-reducer
  level, with no real CLI cost, by
  `agent/tests/projection.rs::two_concurrent_permission_requests_are_both_retained_and_independently_resolvable_in_either_order`.
- **Window close / orphan cleanup** (a real compositor window close runs `AgentSession::shutdown()` with zero orphaned
  `claude`/`nvim`/WebKit processes, by a `/proc` before/after diff): sandbox verification in `shell/MANUAL_VERIFICATION.md`
  ("agent-ui verification"). Not repeated on 2.1.292.
- **WebView reload** (a snapshot rehydrates the panel after a reload): sandbox verification in
  `shell/MANUAL_VERIFICATION.md` ("agent-ui verification, protocol v2", check 4).
  Not repeated on 2.1.292.

## Not measured on 2.1.292

- **The stream under the real gate argv.** The capture ran without `--settings`, so it does not show whether the gate's own
  `PreToolUse` hook adds `hook_started`/`hook_response` lines; if it does they fall into `Unknown` like the others, but that
  is not observed. The conformance tests above do run with the gate, and their `AgentSession` consumes the real stream.
- The `control_request`/`can_use_tool` path (the secondary channel; `PreToolUse` is the gate) did not occur in any run.
- A session that ends in a usage limit or a full context: the capture never reached one.
- A resume (`--resume`) of a legacy session.
