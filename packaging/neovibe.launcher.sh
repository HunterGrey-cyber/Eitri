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

# The sidecar backend needs a Verdandi source checkout, Node and npm; it is not in this package and
# cannot be until Verdandi's own packaging story exists. So the default is chosen by what is
# actually present rather than compiled in, and the choice is announced -- an agent panel that
# silently runs a different backend than you think is the failure this line exists to prevent.
VERDANDI="${NEOVIBE_VERDANDI_CHECKOUT:-$HOME/src/verdandi-old-checkout}"
SIDECAR_AVAILABLE=""
[[ -d "$VERDANDI/apps/claude-sidecar" ]] && SIDECAR_AVAILABLE=1

if [[ -z "$BACKEND" ]]; then
	if [[ -n "$SIDECAR_AVAILABLE" ]]; then BACKEND=sidecar; else BACKEND=legacy; fi
fi
if [[ "$BACKEND" == sidecar && -z "$SIDECAR_AVAILABLE" ]]; then
	echo "neovibe: --sidecar needs a Verdandi checkout and $VERDANDI is not one." >&2
	echo "         Set NEOVIBE_VERDANDI_CHECKOUT, or drop --sidecar to use the packaged backend." >&2
	exit 1
fi

export NEOVIBE_AGENT_BACKEND="$BACKEND"
[[ "$BACKEND" == sidecar ]] && export NEOVIBE_VERDANDI_CHECKOUT="$VERDANDI"
[[ -n "$ACCOUNT" ]] && export VERDANDI_CLAUDE_ACCOUNT="$ACCOUNT"

if [[ -z "$QUIET" ]]; then
	{
		echo "neovibe  project $PROJECT"
		if [[ "$BACKEND" == sidecar ]]; then
			echo "         backend sidecar -- $VERDANDI @ $(git -C "$VERDANDI" rev-parse --short HEAD 2>/dev/null || echo '?')"
		else
			echo "         backend legacy (self-contained; --sidecar needs a Verdandi checkout)"
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
