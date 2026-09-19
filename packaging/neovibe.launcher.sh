#!/usr/bin/env bash
# neovibe. Everything it decides is printed before the window opens.
#
#   neovibe                  open the current directory
#   neovibe ~/some/project   open that project
#   neovibe --legacy ~/p     use the older in-process Claude backend
#   neovibe --account work bill a specific Claude account
set -euo pipefail

LIBDIR=/usr/lib/neovibe
BACKEND="${NEOVIBE_AGENT_BACKEND:-}"
ACCOUNT="${VERDANDI_CLAUDE_ACCOUNT:-}"
ACCOUNT_FROM_FLAG=""
PROJECT=""
QUIET=""

while [[ $# -gt 0 ]]; do
	case "$1" in
		--legacy)  BACKEND=legacy;  shift ;;
		--sidecar) BACKEND=sidecar; shift ;;
		--account) ACCOUNT="${2:?--account needs a name}"; ACCOUNT_FROM_FLAG=1; shift 2 ;;
		--quiet)   QUIET=1; shift ;;
		-h|--help) sed -n '2,7p' "$0" | sed 's/^# \?//'; exit 0 ;;
		--) shift; PROJECT="${1:-}"; break ;;
		-*) echo "neovibe: unknown option $1" >&2; exit 64 ;;
		*)  PROJECT="$1"; shift ;;
	esac
done
PROJECT="$(realpath -e -- "${PROJECT:-$PWD}" 2>/dev/null)" || { echo "neovibe: no such directory" >&2; exit 1; }

# WHO DECIDES THE BACKEND, and why it is no longer this script.
#
# It used to choose here, from whether a Verdandi checkout existed. The binary now makes the same
# decision from a better question -- is a shipped sidecar artifact installed beside it? -- and
# announces it on stderr (`neovibe_core::agent_backend::BackendKind::choose`). Two places deciding
# one thing is how they drift, so this script only OVERRIDES, when asked.
#
# The artifact is a self-contained executable with no Node runtime dependency, so when it is in the
# package the sidecar is simply the default: partial streaming, resume, bounded ingestion, and
# immunity to the wrapper collision that broke legacy's only permission gate on hosts where
# something owns `--settings`.
# This default must stay equal to the binary's own
# (agent::providers::claude_sidecar::spawn::locate_verdandi_checkout). It was
# ~/src/verdandi-old-checkout, a detached checkout from while Verdandi's
# protocol-3 merge was outstanding; their main absorbed it on 2026-09-18, and the two
# defaults pointing at different directories is the drift this section is about.
VERDANDI="${NEOVIBE_VERDANDI_CHECKOUT:-$HOME/src/verdandi}"
PACKAGED_SIDECAR="$LIBDIR/verdandi-claude-sidecar"

if [[ "$BACKEND" == sidecar && ! -x "$PACKAGED_SIDECAR" && ! -d "$VERDANDI/apps/claude-sidecar" ]]; then
	echo "neovibe: --sidecar needs either the packaged sidecar at $PACKAGED_SIDECAR" >&2
	echo "         or a Verdandi checkout; $VERDANDI is not one." >&2
	echo "         Set NEOVIBE_VERDANDI_CHECKOUT, or drop --sidecar." >&2
	exit 1
fi

# Only exported when the caller actually chose: an empty value means "not set", and the binary
# reads that as "decide for yourself".
[[ -n "$BACKEND" ]] && export NEOVIBE_AGENT_BACKEND="$BACKEND"
# Handed over only when there is no packaged artifact -- otherwise an unrelated checkout in the
# operator's home directory would silently outrank the binary that shipped with the product.
[[ "$BACKEND" == sidecar && ! -x "$PACKAGED_SIDECAR" ]] && export NEOVIBE_VERDANDI_CHECKOUT="$VERDANDI"
[[ -n "$ACCOUNT" ]] && export VERDANDI_CLAUDE_ACCOUNT="$ACCOUNT"

if [[ -z "$QUIET" ]]; then
	{
		echo "neovibe  project $PROJECT"
		# Says what this script did, not what the backend will be -- the binary announces that
		# itself, from the same decision it acts on. A second guess printed here could disagree
		# with the first, and the reader has no way to tell which one ran.
		if [[ -n "$BACKEND" ]]; then
			echo "         backend $BACKEND (requested here)"
		elif [[ -x "$PACKAGED_SIDECAR" ]]; then
			echo "         backend chosen by neovibe; the packaged sidecar is installed"
		else
			echo "         backend chosen by neovibe; no packaged sidecar here, so legacy"
		fi
		if [[ -n "$ACCOUNT_FROM_FLAG" ]]; then
			echo "         account $ACCOUNT (--account)"
		elif [[ -n "$ACCOUNT" ]]; then
			echo "         account $ACCOUNT (from VERDANDI_CLAUDE_ACCOUNT in the environment)"
		else
			echo "         account not pinned; CLAUDE_PROFILE=${CLAUDE_PROFILE:-<unset>}"
		fi
	} >&2
fi

exec "$LIBDIR/shell" "$PROJECT"
