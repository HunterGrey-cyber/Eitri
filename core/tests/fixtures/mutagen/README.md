# Recorded Mutagen outputs

Outputs of Mutagen 0.18.1 (the official linux-amd64 release), recorded with local endpoints and a private
`MUTAGEN_DATA_DIRECTORY`; local paths are rewritten to `/srv/project` (the server side) and `/srv/project-laptop`.
The engine's parser and the fake `mutagen` replay them; a new Mutagen version is recorded again, never edited by hand.

| file | command | state |
|---|---|---|
| `version.txt` | `mutagen version` | -- |
| `list-ready.json` | `mutagen sync list --template '{{json .}}' <session>` | `watching`, labels, scan counts |
| `list-paused.json` | the same | `paused: true` |
| `list-conflicts.json` | the same | two conflicts (two-sided edit; file versus directory) and three server scan problems (absolute symlink, escaping symlink, non-UTF-8 name) |
| `list-recreated.json` | the same, after `terminate` and `create` over trees that differed | one conflict from the lost ancestor |
| `list-halted.json` | the same, after the server root was emptied | `halted-on-root-emptied` |
| `monitor-staging-one-file.jsonl` | `mutagen sync monitor --template '{{json .}}' <session>` | first and last events of one 1.5 GiB file staging towards the laptop |
| `monitor-staging-batch.jsonl` | the same | a batch of 60 files: `expectedFiles` is the batch's count, `expectedSize` the current file's size |
| `flush-paused.stderr` | `mutagen sync flush <session>` on a paused session (exit 1) | -- |
| `fake-mutagen` | executable Python 3 fixture | logs argv, pid, working directory and the private process environment to `FAKE_MUTAGEN_LOG`; replays the first matching argv prefix from `FAKE_MUTAGEN_SCRIPT` |

A FIFO in the tree produces no entry at all, so no fixture shows one.

The fake's script is a JSON list of objects with `match` (an argv prefix), optional `stdout_file` (relative to
this directory) or `stdout`, `stderr`, `exit` and `sleep` (seconds). It needs no daemon, display or network.

The fake also supplies Mutagen's default missing-name failures for unscripted `sync list <name>` and
`sync monitor <name>` (exit 1, the latter prefixed `list failed: `); only an unscripted label-selector list
returns `[]` successfully. Bare `daemon` prints help with exit 0 and supplies no version report. Scripted
replies still take precedence. Optional fixture controls are `once` (consume a reply once across subprocesses),
`wait_file` (wait for that absolute marker path before answering), and `grandchild` (a marker path for a child
that retains stdout/stderr until the marker exists, then acknowledges release at `<marker>.done`). They exercise
recovery, concurrent commands and timeout
drain detachment without a real daemon. `SSH_ASKPASS_REQUIRE` is included in the environment log.
