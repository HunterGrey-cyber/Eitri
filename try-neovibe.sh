#!/usr/bin/env bash
# Launch neovibe on your real desktop, with the account and the backend stated rather than inherited.
#
#   ./try-neovibe.sh                      # sidecar backend, your own project dir (cwd)
#   ./try-neovibe.sh ~/some/project       # sidecar backend, that project
#   ./try-neovibe.sh --legacy ~/project   # the old backend, for comparison
#   ./try-neovibe.sh --account work ~/p # bill work instead of your own account
#
# Everything it prints before launching is the configuration it is actually using. If a line looks
# wrong, it is wrong -- nothing below is inferred at runtime.
set -euo pipefail
cd "$(dirname "$(realpath "$0")")"

BACKEND=sidecar
ACCOUNT=""            # empty = the sidecar inherits whatever account this shell carries
PROJECT=""
while [[ $# -gt 0 ]]; do
	case "$1" in
		--legacy)  BACKEND=legacy; shift ;;
		--sidecar) BACKEND=sidecar; shift ;;
		--account) ACCOUNT="${2:?--account needs a name}"; shift 2 ;;
		--) shift; PROJECT="${1:-}"; break ;;
		-*) echo "unknown option $1" >&2; exit 64 ;;
		*)  PROJECT="$1"; shift ;;
	esac
done
PROJECT="$(realpath -e -- "${PROJECT:-$PWD}")"

LOG="${TMPDIR:-/tmp}/neovibe-try-$(date +%H%M%S).log"
VERDANDI="${NEOVIBE_VERDANDI_CHECKOUT:-$HOME/src/verdandi}"

echo "project     $PROJECT"
echo "backend     $BACKEND"
if [[ "$BACKEND" == sidecar ]]; then
	echo "sidecar     $VERDANDI @ $(git -C "$VERDANDI" rev-parse --short HEAD 2>/dev/null || echo '?')"
	echo "            (agent pins 650782f; a different revision here is a diagnostic, not a refusal)"
fi
if [[ -n "$ACCOUNT" ]]; then
	echo "account     $ACCOUNT  (stated)"
else
	echo "account     whatever this shell carries: CLAUDE_PROFILE=${CLAUDE_PROFILE:-<unset>}"
	echo "            pass --account <name> to pin it. The sidecar reads its own environment."
fi
echo "log         $LOG"
echo

cargo build -p shell 2>&1 | tail -1

export NEOVIBE_AGENT_BACKEND="$BACKEND"
[[ "$BACKEND" == sidecar ]] && export NEOVIBE_VERDANDI_CHECKOUT="$VERDANDI"
[[ -n "$ACCOUNT" ]] && export VERDANDI_CLAUDE_ACCOUNT="$ACCOUNT"

./target/debug/shell "$PROJECT" 2>&1 | tee "$LOG"
echo
echo "log kept at $LOG"
