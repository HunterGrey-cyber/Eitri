# `transcript_lines.jsonl`

Twelve **real** lines, taken whole out of this machine's own `$CLAUDE_CONFIG_DIR/projects` on
2026-09-20 and redacted. Every line was written by CLI **2.1.272** (the four conversational line
types carry that in their own `version` field; the three side records carry no version, which is
itself one of the structural facts these fixtures pin).

They are the on-disk shape, which is **not** the wire shape. The wire fixtures in this same
directory (`assistant_text.json`, `user_tool_result.json`, `v2_*.json`, ...) stay where they are:
the two sets pin two different formats and neither replaces the other. The 2026-09-20 resume-history
design's §12 is the reason this set exists — its earlier draft proposed *assembling* on-disk lines
out of wire fixtures, which would have tested an invention rather than the CLI's real storage.

## What was redacted, and what was deliberately not

Redacted, because the fixtures are here to prove **structure**, not to carry anyone's conversation:
every `message.content` body, `thinking` text and its `signature`, tool `input` values, tool result
`content` and `toolUseResult`, `attachment` bodies, `cwd`, `gitBranch`, `slug`, the `aiTitle` value,
and `file-history-delta`'s `trackingPath`/`backup`.

Kept byte-for-byte, because the parser's decisions turn on exactly these: `type`, `subtype`,
`isSidechain`, `isMeta`, `isCompactSummary`, `isVisibleInTranscriptOnly`, `promptSource`, `origin`,
`sourceToolAssistantUUID`, `uuid`, `parentUuid`, `version`, `permissionMode`, `entrypoint`,
`userType`, every content block's `type`/`id`/`tool_use_id`/`name`/`is_error`, and
`compactMetadata`.

Two more things are verbatim on purpose:

- The compaction summary's opening sentence ("This session is being continued from a previous
  conversation…"). It is a constant the CLI writes, not anyone's words, and it is the exact text
  that must never be rendered as the user's own prompt.
- The editor-context block on the `sdk` prompt, with only the path and the selected text replaced.
  Its template is this repository's own (`neovibe_core::editor_context::compose_turn_text`), and the
  fact that it is sitting on disk inside a stored prompt is the measured finding that
  `strip_composed_block` exists for.

## The twelve lines, in file order

`ai-title`, a `typed` prompt, an `sdk` prompt, an `attachment`, an assistant `thinking` block, an
assistant `tool_use`, its matching `tool_result` (the ids were re-pointed at each other so the pair
matches inside this file), an `sdk` prompt carrying an editor-context block, an `isMeta` user line,
a compaction summary (`isCompactSummary`), its paired `system`/`compact_boundary`, and a
`file-history-delta`.
