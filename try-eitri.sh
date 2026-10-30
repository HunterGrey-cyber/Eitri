#!/usr/bin/env bash
# Launch Eitri on your real desktop, with the account and the backend stated rather than inherited.
#
#   ./try-eitri.sh                      # sidecar backend, your own project dir (cwd)
#   ./try-eitri.sh ~/some/project       # sidecar backend, that project
#   ./try-eitri.sh --legacy ~/project   # the old backend, for comparison (a development build:
#                                         # it builds shell with --features shell/legacy-backend)
#   ./try-eitri.sh --account work ~/p # bill work instead of your own account
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

LOG="${TMPDIR:-/tmp}/eitri-try-$(date +%H%M%S).log"
VERDANDI="${EITRI_VERDANDI_CHECKOUT:-$HOME/src/verdandi}"

echo "project     $PROJECT"
echo "backend     $BACKEND"
# An explicit checkout is tried before a packaged sibling or per-user sidecar, and an unusable
# one stops resolution. Export it only when supplied or when the default checkout exists, so a
# missing development checkout does not mask a usable sidecar from `eitri setup`.
VERDANDI_RESOLVED=0
if [[ "$BACKEND" == sidecar ]]; then
	if [[ -n "${EITRI_VERDANDI_CHECKOUT-}" || -d "$VERDANDI" ]]; then
		VERDANDI_RESOLVED=1
		echo "sidecar     $VERDANDI @ $(git -C "$VERDANDI" rev-parse --short HEAD 2>/dev/null || echo '?')"
		echo "            (agent's baseline is agent::EXPECTED_VERDANDI_REVISION; a different revision"
		echo "             here is a diagnostic printed at spawn, not a refusal)"
		# Which of the two shapes will run, said here rather than discovered in the log. A checkout
		# holding a built artifact runs THAT -- the same executable the package ships -- and a checkout
		# without one runs `node dist/`, building it first if needed, which is the slow first start.
		ARTIFACT="$(ls -t "$VERDANDI"/apps/claude-sidecar/dist-bin/verdandi-claude-sidecar-* 2>/dev/null | head -1 || true)"
		if [[ -n "$ARTIFACT" ]]; then
			echo "            runs $(basename "$ARTIFACT") (prebuilt; same shape the package ships)"
		else
			echo "            runs node dist/ -- nothing built in dist-bin/. First start may run npm."
			echo "            \`npm run build:binary -w @verdandi/claude-sidecar\` there builds the artifact once."
		fi
	else
		echo "sidecar     no checkout set and $VERDANDI does not exist -- falling back to a packaged"
		echo "            sibling or a per-user built sidecar (eitri setup), same as an ordinary install"
	fi
fi
if [[ -n "$ACCOUNT" ]]; then
	echo "account     $ACCOUNT  (stated)"
else
	echo "account     whatever this shell carries: CLAUDE_PROFILE=${CLAUDE_PROFILE:-<unset>}"
	echo "            pass --account <name> to pin it. The sidecar reads its own environment."
fi
echo "log         $LOG"
echo

# The legacy backend is compiled only with the `legacy-backend` feature, off by default and in every
# release (it is development-only); a default build refuses EITRI_AGENT_BACKEND=legacy at startup.
# So `--legacy` builds with the feature, the development route that stays available. Switching between the two
# rebuilds shell, agent and eitri-core once each way.
FEATURES=()
[[ "$BACKEND" == legacy ]] && FEATURES=(--features shell/legacy-backend)
cargo build -p shell "${FEATURES[@]}" 2>&1 | tail -1

export EITRI_AGENT_BACKEND="$BACKEND"
[[ "$VERDANDI_RESOLVED" == 1 ]] && export EITRI_VERDANDI_CHECKOUT="$VERDANDI"
[[ -n "$ACCOUNT" ]] && export VERDANDI_CLAUDE_ACCOUNT="$ACCOUNT"

./target/debug/shell "$PROJECT" 2>&1 | tee "$LOG"
echo
echo "log kept at $LOG"
