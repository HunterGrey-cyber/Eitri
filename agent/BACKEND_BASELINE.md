# Backend baseline — Phase 0 of the Claude runtime/provider refactor

Recorded 2026-09-09, ahead of the migration described in
`docs/superpowers/specs/2026-09-09-claude-runtime-provider-refactor-design.md`. This is the frozen
behavioral record Phase 1+ must not silently regress. The invocation shape below is a direct
transcription of `agent/src/process.rs`'s real `AgentProcess::spawn_with_binary` — if that function
changes, this file is stale and must be re-generated, not hand-edited to guess at the new shape.

## Claude CLI version at baseline time

```
2.1.266 (Claude Code)
```

## Exact real CLI invocation (`agent/src/process.rs:358-375`, `AgentProcess::spawn_with_binary`)

```
claude --print --input-format stream-json --output-format stream-json --verbose \
  --setting-sources project,local --permission-mode <auto|bypassPermissions> \
  [--disallowedTools <comma-joined tool names>]
```

- `--permission-mode` is `auto` for `PermissionMode::Auto`, `bypassPermissions` for
  `PermissionMode::Bypass` (`agent/src/process.rs:71-75`).
- `--disallowedTools` is appended only when the caller's `disallowed_tools` slice is non-empty;
  `agent-ui`'s real session start always passes `CONSERVATIVE_DISALLOWED_TOOLS = ["Bash", "Write",
  "Edit", "NotebookEdit"]` (`agent/src/process.rs:57`).
- Working directory is `project_dir`, passed verbatim to `Command::current_dir`.
- stdin/stdout/stderr are all piped (`Stdio::piped()`); stdin stays open for the process's whole
  lifetime — one JSON line per turn, not a fresh process per turn.
- Every non-`Bypass` session additionally writes a `PreToolUse` hook into
  `<project_dir>/.claude/settings.local.json` (`agent/src/settings.rs`,
  `HookSettings::generate_with_hook_path`) — this is the directory-scoped file the collision test
  below exercises.

## What `agent/tests/backend_conformance.rs` pins (real CLI, `#[ignore]`d, run explicitly)

| Test | What it proves |
|---|---|
| `real_multi_turn_conversation_in_one_process` | A single long-lived process genuinely carries context across turns without `--resume`. |
| `real_pretooluse_hook_allow_end_to_end` | A real tool call's `PreToolUse` hook round-trips through `agent-hook` and an `allow` answer lets the tool run. |
| `real_pretooluse_hook_deny_end_to_end` | The deny half of the same round trip: the CLI genuinely blocks the tool and surfaces the deny reason verbatim in the tool result. |
| `real_two_sessions_in_the_same_project_dir_cross_wire_permission_hooks` | The real cross-session settings collision (design doc §2.1, problem 2) reproduces exactly as diagnosed: session B receives session A's hook request. |
| `real_interrupt_mid_permission_denies_pending_requests_without_ending_the_session` | `interrupt()` denies/clears pending permissions, the interrupted turn's `TurnFinished` still arrives, and the session survives for a further turn. |

⚠ The `real_two_sessions_in_the_same_project_dir_cross_wire_permission_hooks` row above pins a
**known, open defect** (design doc §2.1, problem 2), not a behavior to preserve. When a later
phase fixes the underlying settings-file collision, this test is expected to start failing and
must be updated or removed as part of that fix — never have its assertion relaxed to keep passing
against fixed behavior.

## What is pinned elsewhere, not re-tested here

- **Concurrent permission requests** (multiple simultaneously-pending `PermissionRequest`s, each
  independently answerable in any order): pinned at the pure-reducer level, with no real CLI cost,
  by `agent/tests/projection.rs::two_concurrent_permission_requests_are_both_retained_and_independently_resolvable_in_either_order`
  (moved here from the now-deleted `agent/tests/session_state.rs` when Phase 1 of the Claude
  runtime/provider refactor replaced `AgentSessionState` with `AgentSessionProjection` --
  `docs/superpowers/plans/2026-09-09-claude-runtime-refactor-phase1-domain-ui.md`).
- **Window close / orphan cleanup** (a real compositor window close reliably runs
  `AgentSession::shutdown()` with zero orphaned `claude`/`nvim`/WebKit processes, confirmed via a
  real `/proc`-based before/after diff repeated 3 times): pinned by real, repeated sandbox
  verification in `shell/MANUAL_VERIFICATION.md`'s "agent-ui verification" section (2026-09-08) —
  a GUI concern outside this crate's own test surface, not repeated here.

## Open gap this Phase 0 pass did NOT close

- **WebView reload** (whether a snapshot correctly rehydrates `agent-ui`'s frontend state after a
  page reload): an earlier draft of this baseline claimed this was already pinned alongside
  window-close/orphan-cleanup above. It is not — `shell/MANUAL_VERIFICATION.md:489-490` records
  the opposite: `applySnapshot` "currently only ever fires once, on the panel's initial `"ready"`
  message -- never again for the life of the panel." Reload rehydration is real, unverified, and
  plausibly broken. Whoever next touches the WebView bridge (a later phase of the runtime/provider
  migration explicitly reworks this exact path with a new revisioned snapshot protocol) should
  verify this for real before claiming it, not carry this baseline's mistake forward.
