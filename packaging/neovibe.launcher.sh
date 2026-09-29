#!/usr/bin/env bash
# neovibe. Everything it decides is printed before the window opens.
#
#   neovibe                  open the current directory
#   neovibe ~/some/project   open that project
#   neovibe --legacy ~/p     use the older in-process Claude backend (development builds only --
#                            a release build refuses it, saying it is not in this build)
#   neovibe --account work   bill a specific Claude account (and read its history)
#   neovibe --version        print the version and exit
#   neovibe setup [args]     build the sidecar (and offer nvim), or --nvim-only/--uninstall/
#                            --sidecar-only for one of those alone (neovibe setup --help)
set -euo pipefail

# Derive LIBDIR from the launcher's location for system packages, per-user installs and
# unpacked tarballs. Resolve only the launcher file's symlink chain. Resolving symlinked
# directories too could move a symlinked bin directory into a different tree whose sibling
# lib/neovibe does not belong to this install.
#
# The `cd` below resolves `<resolved file directory>/../lib/neovibe` logically, so `..`
# selects the sibling of the directory as spelled. This also works in POSIX mode.
# Bash outside POSIX mode retries physically if the logical path does not exist, which
# also supports a bindir whose resolved target has the sibling library directory.
#
# Append `..` to reach the parent. A second `dirname` would leave `.` unchanged and break
# `./neovibe` from an unpacked bin directory or `bin/./neovibe` from its root.
#
# Clear CDPATH for `cd` so it cannot print a destination before `pwd`. That extra line
# would corrupt LIBDIR when the launcher is invoked through a relative path.
resolve_launcher_path() {
	local path="$1" target depth=0
	while [[ -L "$path" ]]; do
		depth=$((depth + 1))
		if ((depth > 40)); then
			echo "neovibe: too many levels of symbolic links: $1" >&2
			return 1
		fi
		target="$(readlink "$path")"
		case "$target" in
			/*) path="$target" ;;
			*) path="$(dirname "$path")/$target" ;;
		esac
	done
	printf '%s\n' "$path"
}

NEOVIBE_LAUNCHER_PATH="$(resolve_launcher_path "$0")" || exit 1
LIBDIR="$(CDPATH='' cd -- "$(dirname -- "$NEOVIBE_LAUNCHER_PATH")/../lib/neovibe" && pwd)"
unset -f resolve_launcher_path
unset NEOVIBE_LAUNCHER_PATH

# Handle `neovibe setup [args]` before flag parsing because it opens no project or window.
# Run the POSIX `sh` installer explicitly with `sh` so it works without an execute bit.
# Pass explicit installer modes and -h/--help through exactly once. Adding --sidecar-only
# would conflict with another mode because set_mode refuses a second one.
# The launcher test reads the modes from install.sh so newly added modes must pass through too.
# Without a mode, build the sidecar first, then run the normal --nvim-offer path.
# That offer skips an adequate PATH nvim, an already installed private nvim, or --no-nvim,
# and only warns on download failure. --nvim-only instead fetches even with an adequate
# PATH nvim and fails on download errors. A setup without a mode
# must not fail because nvim was already fine or the network is down.
# --sidecar-only ignores --yes/--with-nvim; --nvim-offer handles them itself.
if [[ "${1:-}" == setup ]]; then
	shift
	for a in "$@"; do
		case "$a" in
		--nvim-only | --nvim-offer | --uninstall | --sidecar-only | --from-source | --build-sidecar-into | -h | --help)
			exec sh "$LIBDIR/neovibe-setup" "$@"
			;;
		esac
	done
	sh "$LIBDIR/neovibe-setup" --sidecar-only "$@"
	for a in "$@"; do
		case "$a" in
		--no-nvim) exit 0 ;;
		esac
	done
	exec sh "$LIBDIR/neovibe-setup" --nvim-offer "$@"
fi

ACCOUNT="${VERDANDI_CLAUDE_ACCOUNT:-}"
ACCOUNT_FROM_FLAG=""
PROJECT=""
QUIET=""
SHELL_ARGS=()

while [[ $# -gt 0 ]]; do
	case "$1" in
		# Forward --legacy as argv so `shell` selects the backend and checks whether it is built in.
		# Selection is per invocation, so two launches of the same binary can choose differently.
		--legacy)  SHELL_ARGS+=(--legacy); shift ;;
		--version) exec "$LIBDIR/shell" --version ;;
		--account) ACCOUNT="${2:?--account needs a name}"; ACCOUNT_FROM_FLAG=1; shift 2 ;;
		--quiet)   QUIET=1; shift ;;
		-h|--help) sed -n '2,11p' "$0" | sed 's/^# \?//'; exit 0 ;;
		--) shift; PROJECT="${1:-}"; break ;;
		-*) echo "neovibe: unknown option $1" >&2; exit 64 ;;
		*)  PROJECT="$1"; shift ;;
	esac
done
PROJECT="$(realpath -e -- "${PROJECT:-$PWD}" 2>/dev/null)" || { echo "neovibe: no such directory" >&2; exit 1; }

# Since 2026-09-21 this variable decides BOTH halves, not just the sidecar's. It always moved which
# account the CLI authenticates as and writes transcripts under; the binary now derives the
# directory it READS transcripts from by the same convention (`agent::account`), so a resume finds
# the history the account really holds. Before that the two disagreed silently on this host, where
# the shell exports this for Verdandi and the window inherited a different `CLAUDE_CONFIG_DIR`.
[[ -n "$ACCOUNT" ]] && export VERDANDI_CLAUDE_ACCOUNT="$ACCOUNT"

# This check only controls the sidecar hint before the window opens. It selects no backend
# and exports no variables. agent::providers::claude_sidecar::spawn::resolve_sidecar_program
# performs discovery. The hint checks the explicit binary override, a packaged sibling and
# the per-user revision path used by user_sidecar_path, so a missing sidecar is reported early.
# Checkout discovery is omitted because it does not apply to installed release builds.
# Unset, empty or relative XDG_DATA_HOME uses $HOME/.local/share, matching the build location.
# A plain ${XDG_DATA_HOME:-default} would incorrectly accept a relative value.
xdg_data_home() {
	case "${XDG_DATA_HOME:-}" in
		/*) printf '%s\n' "$XDG_DATA_HOME" ;;
		*)  printf '%s\n' "$HOME/.local/share" ;;
	esac
}

sidecar_found() {
	[[ -n "${NEOVIBE_SIDECAR_BINARY:-}" && -x "$NEOVIBE_SIDECAR_BINARY" ]] && return 0
	[[ -x "$LIBDIR/verdandi-claude-sidecar" ]] && return 0
	# Use the revision pinned in the RELEASE beside LIBDIR for the per-user sidecar path.
	# A development build without RELEASE skips this check rather than guessing a revision.
	if [[ -f "$LIBDIR/RELEASE" ]]; then
		local rev7
		rev7="$(sed -n 's/^VERDANDI_REV=\(.......\).*/\1/p' "$LIBDIR/RELEASE" | head -n1)"
		[[ -n "$rev7" && -x "$(xdg_data_home)/neovibe/sidecar/$rev7/verdandi-claude-sidecar" ]] && return 0
	fi
	return 1
}

if [[ -z "$QUIET" ]] && ! sidecar_found; then
	# The window still opens without an agent backend. The panel shows the same setup hint.
	echo 'neovibe: no sidecar built yet -- run "neovibe setup" to build one' >&2
fi

if [[ -z "$QUIET" ]]; then
	{
		echo "neovibe  project $PROJECT"
		if [[ -n "$ACCOUNT_FROM_FLAG" ]]; then
			echo "         account $ACCOUNT (--account)"
		elif [[ -n "$ACCOUNT" ]]; then
			echo "         account $ACCOUNT (from VERDANDI_CLAUDE_ACCOUNT in the environment)"
		else
			# Not "not pinned" -- not pinned HERE. `init.lua` can still pin one, and the
			# binary prints what it took and where from; a flat claim here would contradict it.
			echo "         account not pinned here; init.lua may pin one. CLAUDE_PROFILE=${CLAUDE_PROFILE:-<unset>}"
		fi
	} >&2
fi

exec "$LIBDIR/shell" "${SHELL_ARGS[@]}" "$PROJECT"
