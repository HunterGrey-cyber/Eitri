#!/usr/bin/env bash
# Eitri. Everything it decides is printed before the window opens.
#
#   eitri                  open the current directory
#   eitri ~/some/project   open that project
#   eitri --legacy ~/p     use the older in-process Claude backend (development builds only --
#                            a release build refuses it, saying it is not in this build)
#   eitri --account work   bill a specific Claude account (and read its history)
#   eitri --version        print the version and exit
#   eitri setup [args]     build the sidecar (and offer nvim), or --nvim-only/--uninstall/
#                            --sidecar-only for one of those alone (eitri setup --help)
set -euo pipefail

# Derive LIBDIR from the launcher location for system packages, per-user installs and
# unpacked tarballs. Each launcher directory has the corresponding ../lib/eitri.
#
# Resolve the launcher's file symlink chain while preserving symlinked directories.
# Canonicalizing a symlinked bin directory could select the target directory's sibling
# library directory instead of the install's own library directory.
# Resolve ../lib/eitri logically with cd so a symlinked bindir retains its own sibling lib.
# Bash can retry physically outside POSIX mode if that logical path does not exist;
# a layout that only works via the symlink target's sibling library relies on that retry.
#
# Append .. to reach the parent: dirname . is still ., so a second dirname would fail for
# ./eitri from bin/ or bin/./eitri from an unpacked release's root.
# Clear CDPATH for cd so it cannot print an extra destination line into LIBDIR when the
# launcher path is relative.
resolve_launcher_path() {
	local path="$1" target depth=0
	while [[ -L "$path" ]]; do
		depth=$((depth + 1))
		if ((depth > 40)); then
			echo "eitri: too many levels of symbolic links: $1" >&2
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

EITRI_LAUNCHER_PATH="$(resolve_launcher_path "$0")" || exit 1
LIBDIR="$(CDPATH='' cd -- "$(dirname -- "$EITRI_LAUNCHER_PATH")/../lib/eitri" && pwd)"
unset -f resolve_launcher_path
unset EITRI_LAUNCHER_PATH

# Handle `eitri setup [args]` before window flag parsing. Run the POSIX setup script with
# sh so it also works without an execute bit.
#
# Forward --nvim-only, --nvim-offer, --uninstall, --sidecar-only, --from-source,
# --build-sidecar-into and -h/--help unchanged in one invocation. Adding --sidecar-only
# would conflict with an explicit mode because set_mode rejects a second mode.
# packaging/test_launcher.sh reads the installer modes to catch missing forwarding cases.
#
# Without an explicit mode, build the sidecar first and then run --nvim-offer.
# The offer skips an adequate PATH nvim, an already installed release nvim, or --no-nvim;
# a download failure only warns. --nvim-only instead fetches even with an adequate PATH nvim
# and fails on download errors. --sidecar-only ignores --yes and --with-nvim, and the offer
# handles those options itself.
if [[ "${1:-}" == setup ]]; then
	shift
	for a in "$@"; do
		case "$a" in
		--nvim-only | --nvim-offer | --uninstall | --sidecar-only | --from-source | --build-sidecar-into | -h | --help)
			exec sh "$LIBDIR/eitri-setup" "$@"
			;;
		esac
	done
	sh "$LIBDIR/eitri-setup" --sidecar-only "$@"
	for a in "$@"; do
		case "$a" in
		--no-nvim) exit 0 ;;
		esac
	done
	exec sh "$LIBDIR/eitri-setup" --nvim-offer "$@"
fi

ACCOUNT="${VERDANDI_CLAUDE_ACCOUNT:-}"
ACCOUNT_FROM_FLAG=""
PROJECT=""
QUIET=""
SHELL_ARGS=()

while [[ $# -gt 0 ]]; do
	case "$1" in
		# Pass --legacy to shell, which selects the backend and checks whether this build has it.
		# Use argv so each invocation can choose independently.
		--legacy)  SHELL_ARGS+=(--legacy); shift ;;
		--version) exec "$LIBDIR/shell" --version ;;
		--account) ACCOUNT="${2:?--account needs a name}"; ACCOUNT_FROM_FLAG=1; shift 2 ;;
		--quiet)   QUIET=1; shift ;;
		-h|--help) sed -n '2,11p' "$0" | sed 's/^# \?//'; exit 0 ;;
		--) shift; PROJECT="${1:-}"; break ;;
		-*) echo "eitri: unknown option $1" >&2; exit 64 ;;
		*)  PROJECT="$1"; shift ;;
	esac
done
PROJECT="$(realpath -e -- "${PROJECT:-$PWD}" 2>/dev/null)" || { echo "eitri: no such directory" >&2; exit 1; }

# Since 2026-09-21 this variable decides BOTH halves, not just the sidecar's. It always moved which
# account the CLI authenticates as and writes transcripts under; the binary now derives the
# directory it READS transcripts from by the same convention (`agent::account`), so a resume finds
# the history the account really holds. Before that the two disagreed silently on this host, where
# the shell exports this for Verdandi and the window inherited a different `CLAUDE_CONFIG_DIR`.
[[ -n "$ACCOUNT" ]] && export VERDANDI_CLAUDE_ACCOUNT="$ACCOUNT"

# Print a missing-sidecar hint before opening the window. Backend selection remains in
# resolve_sidecar_program; this check neither selects a backend nor exports variables.
# Check the explicit binary override, the packaged sibling and the per-user revision path
# so a missing sidecar is reported before session startup fails. Checkout-based discovery
# is omitted because this launcher ships only with installed release builds.
# Unset, empty or relative XDG_DATA_HOME uses $HOME/.local/share, matching the builder.
# A plain ${XDG_DATA_HOME:-default} would incorrectly accept a relative path.
xdg_data_home() {
	case "${XDG_DATA_HOME:-}" in
		/*) printf '%s\n' "$XDG_DATA_HOME" ;;
		*)  printf '%s\n' "$HOME/.local/share" ;;
	esac
}

sidecar_found() {
	[[ -n "${EITRI_SIDECAR_BINARY:-}" && -x "$EITRI_SIDECAR_BINARY" ]] && return 0
	[[ -x "$LIBDIR/verdandi-claude-sidecar" ]] && return 0
	# Use the revision pinned in the RELEASE beside LIBDIR for the per-user sidecar path.
	# A development build without RELEASE skips this check rather than guessing a revision.
	if [[ -f "$LIBDIR/RELEASE" ]]; then
		local rev7
		rev7="$(sed -n 's/^VERDANDI_REV=\(.......\).*/\1/p' "$LIBDIR/RELEASE" | head -n1)"
		[[ -n "$rev7" && -x "$(xdg_data_home)/eitri/sidecar/$rev7/verdandi-claude-sidecar" ]] && return 0
	fi
	return 1
}

if [[ -z "$QUIET" ]] && ! sidecar_found; then
	# The window still opens without an agent backend. The panel shows the same setup hint.
	echo 'eitri: no sidecar built yet -- run "eitri setup" to build one' >&2
fi

if [[ -z "$QUIET" ]]; then
	{
		echo "eitri    project $PROJECT"
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
