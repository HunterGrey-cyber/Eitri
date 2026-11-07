#!/usr/bin/env bash
# Eitri. Everything it decides is printed before the window opens.
#
#   eitri                  open the current directory
#   eitri ~/some/project   open that project (a folder or a file; +N before a file starts on line N)
#   eitri src/main.rs      open a file: the project is found from it, and an open window of
#                            that project is reused (--new-window always opens a new one)
#   eitri --legacy ~/p     use the older in-process Claude backend (development builds only --
#                            a release build refuses it, saying it is not in this build)
#   eitri --clean ~/p      start nvim with --clean (no init.lua, no plugins) for this window
#   eitri --account work   bill a specific Claude account (and read its history)
#   eitri panel --nvim ADDR [DIR]
#                          open the agent panel beside the nvim at ADDR (:EitriPanel runs this)
#   eitri page ID [--nvim ADDR] [DIR]
#                          open a plugin page beside nvim (--list lists the available page ids)
#   eitri split [DIR]      open the editor (Neovide) and the agent panel as two windows (needs Neovide)
#   eitri agents [--json] [--watch] [--schema 1]
#                          the agents every open window shares: a table, one JSON view, or a stream
#   eitri remote COMMAND   remote control from a phone: serve, stop, status, add-device, devices,
#                            revoke, grant, ssh-key (eitri remote --help lists them)
#   eitri --version        print the version and exit
#   eitri sidecar status   what new tabs run, what is bundled, what is kept; no network
#   eitri sidecar check    ask for the newest sidecar release; changes nothing
#   eitri sidecar update [--version X]
#                          fetch a signed sidecar release, build it here, try it and select it
#   eitri sidecar use bundled
#                          go back to the sidecar Eitri came with; no network
#   eitri sidecar use VERSION
#                          select an update already built
#   eitri setup lua-ls     completion and checks for Eitri's init.lua (lua_ls/luals also accepted)
#   eitri setup [args]     build the sidecar (and offer nvim), or --nvim-only/--uninstall/
#                            --sidecar-only for one of those alone (eitri setup --help)
set -euo pipefail

# Derive LIBDIR from the launcher: /usr/bin/eitri uses /usr/lib/eitri, a user install's
# ~/.local/bin/eitri uses ~/.local/lib/eitri, and an unpacked tarball's bin/eitri uses lib/eitri.
# Resolve only the launcher's own symlink chain, never a symlinked directory: resolving a stow
# bin/ directory physically would pick the dotfiles repository's sibling lib instead of this install's.
#
# Logical `cd` removes `..` from the spelled directory, following symlinks only to check it exists.
# This also works with POSIXLY_CORRECT=1. Bash outside POSIX mode can additionally retry physically
# when the logical path is absent; only that retry supports a target directory with its own sibling lib.
# Append `..` rather than taking dirname twice: `dirname .` is `.`, so the latter breaks ./eitri
# from inside bin/ and bin/./eitri from the tarball root.
# Clear CDPATH so `cd` cannot print a second path into LIBDIR when the launcher's path is relative.
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

sidecar_help() {
	sed -n '/^#   eitri sidecar /,/^#   eitri setup /p' "$0" | sed -e '$d' -e 's/^# \?//'
}

# `eitri sidecar` opens no project or window either, and all of it lives in the installer, which holds
# the release key. A route with no eitri-setup takes its sidecar from its package.
if [[ "${1:-}" == sidecar ]]; then
	shift
	case "${1:-}" in
	status | check | update | use)
		verb="$1"
		shift
		if [[ ! -f "$LIBDIR/eitri-setup" ]]; then
			echo "eitri: this package updates its sidecar itself" >&2
			exit 1
		fi
		if [[ "$verb" == use && $# -eq 0 ]]; then
			echo "usage: eitri sidecar use bundled|VERSION" >&2
			exit 64
		fi
		exec sh "$LIBDIR/eitri-setup" "--sidecar-$verb" "$@"
		;;
	-h | --help)
		sidecar_help
		exit 0
		;;
	"")
		sidecar_help >&2
		exit 64
		;;
	*)
		echo "eitri sidecar: unknown command $1" >&2
		sidecar_help >&2
		exit 64
		;;
	esac
fi

# `eitri agents` opens no project or window and writes only its result to stdout, so a status line can
# read `eitri agents --watch`: it goes to the binary before the banner and the option loop, which would
# refuse its own flags.
if [[ "${1:-}" == agents ]]; then
	shift
	exec "$LIBDIR/shell" agents "$@"
fi

# `eitri remote` opens no project or window either, and its commands take options and --help of their
# own, which the option loop would refuse or answer with this file's help: it goes to the binary as typed.
if [[ "${1:-}" == remote ]]; then
	shift
	exec "$LIBDIR/shell" remote "$@"
fi

# Handle setup before flag parsing: it opens no project or window. Invoke the POSIX installer
# through sh so setup also works when the installed copy lacks its execute bit.
# Explicit modes and help pass through once, unchanged: adding --sidecar-only would conflict with
# --nvim-only or --uninstall. The mode list is checked against install.sh so new modes cannot be missed.
# With no mode, build the sidecar and run the installer's normal nvim offer. Unlike --nvim-only,
# that offer skips an adequate PATH/private nvim and only warns on download failure; an offline
# machine or an already adequate nvim must not make setup fail. --no-nvim skips the offer;
# --yes/--with-nvim are ignored by --sidecar-only and read by --nvim-offer itself.
if [[ "${1:-}" == setup ]]; then
	shift
	case "${1:-}" in
		lua-ls | lua_ls | luals) exec "$LIBDIR/shell" setup "$@" ;;
	esac
	for a in "$@"; do
		case "$a" in
		--nvim-only | --nvim-offer | --uninstall | --sidecar-only | --sidecar-status | --sidecar-check | --sidecar-update | --sidecar-use | --from-source | --build-sidecar-into | -h | --help)
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

# `eitri panel` is the agent panel as its own window beside a terminal nvim. It is parsed by the same
# loop as the one-window command line: what is shared (--account, --quiet, the project) behaves
# alike, and the options that only make sense for one of the two are refused in the other.
MODE=window
if [[ "${1:-}" == open ]]; then
	MODE=open
	shift
elif [[ "${1:-}" == panel ]]; then
	MODE=panel
	shift
elif [[ "${1:-}" == split ]]; then
	# `eitri split` is the editor and the panel as two windows. It takes the project and the shared
	# options and nothing else, like `panel`: the in-process editor's own flags do not apply.
	MODE=split
	shift
elif [[ "${1:-}" == page ]]; then
	MODE=page
	shift
fi

ACCOUNT="${VERDANDI_CLAUDE_ACCOUNT:-}"
ACCOUNT_FROM_FLAG=""
PROJECT=""
QUIET=""
NEW_WINDOW=""
FROM_DESKTOP=""
CHOOSE=""
FIRST=""
# Window mode keeps what was typed, in order: BEFORE is everything ahead of the user's `--` (a `+N`
# stays a line spec there), AFTER everything behind it (all paths). shell resolves them.
BEFORE=()
AFTER=()
SAW_DASHES=""
SHELL_ARGS=()
PANEL_ARGS=()
PAGE_POSITIONALS=()
OPEN_POSITIONALS=()
PAGE_LIST=""
# What an unknown option is reported as: `eitri: ...` for the window, `eitri panel: ...` for the panel.
CMD_NAME=eitri
[[ $MODE == open ]] && CMD_NAME="eitri open"
[[ $MODE == panel ]] && CMD_NAME="eitri panel"
[[ $MODE == split ]] && CMD_NAME="eitri split"
[[ $MODE == page ]] && CMD_NAME="eitri page"

while [[ $# -gt 0 ]]; do
	case "$1" in
		# shell decides whether this invocation selects a compiled-in legacy backend. Keep it in argv
		# so two launches of the same binary can choose differently.
		--legacy)  [[ $MODE == window ]] || { echo "$CMD_NAME: unknown option --legacy" >&2; exit 64; }
		           SHELL_ARGS+=(--legacy); shift ;;
		# Forwarded too: shell starts its nvim with --clean, the same as running it directly.
		--clean)   [[ $MODE == window ]] || { echo "$CMD_NAME: unknown option --clean" >&2; exit 64; }
		           SHELL_ARGS+=(--clean); shift ;;
		# The panel attaches to an nvim that is already running; there is nothing to attach to in the
		# one-window mode, which starts its own.
		--nvim)    [[ $MODE == panel || $MODE == page || $MODE == open ]] || { echo "$CMD_NAME: unknown option --nvim" >&2; exit 64; }
		           [[ -n "${2:-}" ]] || { echo "$CMD_NAME: --nvim needs an address" >&2; exit 64; }
		           PANEL_ARGS+=(--nvim "$2"); shift 2 ;;
		--list)    [[ $MODE == page ]] || { echo "$CMD_NAME: unknown option --list" >&2; exit 64; }
		           PAGE_LIST=1; shift ;;
		--version) [[ $MODE != page ]] || { echo "$CMD_NAME: unknown option --version" >&2; exit 64; }
		           exec "$LIBDIR/shell" --version ;;
		# Opening a file reuses a running window of its project; this opts out. `--account` implies it
		# below: reusing a window would drop the account the flag selects.
		--new-window) [[ $MODE == window ]] || { echo "$CMD_NAME: unknown option --new-window" >&2; exit 64; }
		           NEW_WINDOW=1; shift ;;
		# Only the desktop entry passes this, in front of its `%F` list: see the end of this file.
		--from-desktop) [[ $MODE == window ]] || { echo "$CMD_NAME: unknown option --from-desktop" >&2; exit 64; }
		           FROM_DESKTOP=1; shift ;;
		--account) [[ $MODE != page ]] || { echo "$CMD_NAME: unknown option --account" >&2; exit 64; }
		           ACCOUNT="${2:?--account needs a name}"; ACCOUNT_FROM_FLAG=1; shift 2 ;;
		--quiet)   [[ $MODE != page ]] || { echo "$CMD_NAME: unknown option --quiet" >&2; exit 64; }
		           QUIET=1; shift ;;
		-h|--help) sed -n '2,/^set -euo pipefail$/p' "$0" | sed -e '$d' -e 's/^# \?//'; exit 0 ;;
		--) SAW_DASHES=1; shift
		    if [[ $MODE == window ]]; then AFTER=("$@"); FIRST="${FIRST:-${1:-}}"
		    elif [[ $MODE == page ]]; then PAGE_POSITIONALS+=("$@")
		    elif [[ $MODE == open ]]; then OPEN_POSITIONALS+=("$@")
		    else PROJECT="${1:-}"; fi
		    break ;;
		-*) echo "$CMD_NAME: unknown option $1" >&2; exit 64 ;;
		*)  if [[ $MODE == window ]]; then
		        BEFORE+=("$1")
		        # The banner names the first thing to open, not a `+N` line spec.
		        [[ -z "$FIRST" && $1 != +* ]] && FIRST="$1"
		    elif [[ $MODE == page ]]; then PAGE_POSITIONALS+=("$1")
		    elif [[ $MODE == open ]]; then OPEN_POSITIONALS+=("$1")
		    else PROJECT="$1"; fi
		    shift ;;
	esac
done
if [[ $MODE == page ]]; then
	if [[ -n "$PAGE_LIST" ]]; then
		[[ ${#PAGE_POSITIONALS[@]} -eq 0 ]] || { echo 'eitri page: --list takes no plugin id or directory' >&2; exit 64; }
		exec "$LIBDIR/shell" page "${PANEL_ARGS[@]}" --list
	fi
	PAGE_ID="${PAGE_POSITIONALS[0]:-}"
	if [[ ${#PAGE_ID} -gt 40 || ! "$PAGE_ID" =~ ^[a-z0-9]([a-z0-9-]*[a-z0-9])?$ ]]; then
		echo 'eitri page: needs a plugin id of 1 to 40 lower-case letters, digits or internal dashes' >&2
		exit 64
	fi
	[[ ${#PAGE_POSITIONALS[@]} -le 2 ]] || { echo 'eitri page: takes a plugin id and an optional directory' >&2; exit 64; }
	PROJECT="${PAGE_POSITIONALS[1]:-}"
fi
# `open` without a directory leaves the choice to the nvim being taken over (its own working directory), so
# the launcher's $PWD is never filled in for it; a second directory is refused rather than dropped.
OPEN_DIR_GIVEN=""
if [[ $MODE == open ]]; then
	[[ ${#OPEN_POSITIONALS[@]} -le 1 ]] || { echo 'eitri open: takes at most one project directory' >&2; exit 64; }
	if [[ ${#OPEN_POSITIONALS[@]} -eq 1 ]]; then
		OPEN_DIR_GIVEN=1
		PROJECT="$(realpath -e -- "${OPEN_POSITIONALS[0]}" 2>/dev/null)" || { echo "eitri: no such directory" >&2; exit 1; }
	fi
fi
[[ $MODE == window && -n "$ACCOUNT_FROM_FLAG" ]] && NEW_WINDOW=1
if [[ -n "$FROM_DESKTOP" && ${#BEFORE[@]} -eq 0 && ${#AFTER[@]} -eq 0 ]]; then
	# The chooser needs no project yet, and must also start when the current directory is gone.
	CHOOSE=1
elif [[ $MODE == open ]]; then
	:
elif [[ $MODE != window || (${#BEFORE[@]} -eq 0 && ${#AFTER[@]} -eq 0) ]]; then
	# No positional (or panel/split, which take a directory only): the one directory, resolved here.
	PROJECT="$(realpath -e -- "${PROJECT:-$PWD}" 2>/dev/null)" || { echo "eitri: no such directory" >&2; exit 1; }
	[[ $MODE == window ]] && BEFORE=("$PROJECT") && SAW_DASHES=""
fi

if [[ $MODE == page ]]; then
	exec "$LIBDIR/shell" page "$PAGE_ID" "${PANEL_ARGS[@]}" -- "$PROJECT"
fi

# Use the same account for CLI authentication, transcript writes and resume discovery (agent::account),
# so a window inheriting a different CLAUDE_CONFIG_DIR still reads the selected account's history.
[[ -n "$ACCOUNT" ]] && export VERDANDI_CLAUDE_ACCOUNT="$ACCOUNT"

# This hint never selects a backend or exports a discovery result: shell resolves the sidecar.
# Mirror the executable override, selected update, packaged sibling and rev-keyed user path to warn
# before a failed first session. The development checkout override does not apply to installed release builds.
# Unset, empty or relative XDG_DATA_HOME means $HOME/.local/share; accepting a relative value here
# would disagree with the installer's sidecar location.
xdg_data_home() {
	case "${XDG_DATA_HOME:-}" in
		/*) printf '%s\n' "$XDG_DATA_HOME" ;;
		*)  printf '%s\n' "$HOME/.local/share" ;;
	esac
}

# version_newer A B, both already matched: the numbers first, then a release sorts above its own
# release candidates. 10# because a leading zero must not read as octal.
version_newer() {
	local a b i
	IFS=. read -r -a a <<<"${1%%-*}"
	IFS=. read -r -a b <<<"${2%%-*}"
	for i in 0 1 2; do
		((10#${a[i]} > 10#${b[i]})) && return 0
		((10#${a[i]} < 10#${b[i]})) && return 1
	done
	if [[ "$1" != *-rc.* ]]; then
		[[ "$2" == *-rc.* ]]
		return
	fi
	[[ "$2" == *-rc.* ]] || return 1
	((10#${1##*-rc.} > 10#${2##*-rc.}))
}

# The update a tab would start: eitri-setup beside the binary, `selected` naming a version strictly
# newer than the bundled one, and its directory holding the binary and BUILD. Older than bundled is
# passed over, also when the bundled one is not built. `selected` reads as a tab reads it: one line,
# with the x keeping the newlines $() would drop and a NUL becoming a character no version holds; a
# number has one spelling, and past 256 bytes the cut leaves no version: none is that long.
selected_update_found() {
	[[ -f "$LIBDIR/eitri-setup" && -f "$LIBDIR/RELEASE" ]] || return 1
	local number='(0|[1-9][0-9]{0,17})' version_re root selected version bundled
	version_re="$number\\.$number\\.$number(-rc\\.$number)?"
	root="$(xdg_data_home)/eitri/sidecar"
	[[ -f "$root/selected" ]] || return 1
	selected="$(head -c 256 -- "$root/selected" 2>/dev/null | tr '\000' '?'; printf x)"
	selected="${selected%x}"
	selected="${selected%$'\n'}"
	[[ "$selected" =~ ^version=($version_re)$ ]] || return 1
	version="${BASH_REMATCH[1]}"
	bundled="$(sed -n 's/^SIDECAR_VERSION=//p' "$LIBDIR/RELEASE" | head -n1)"
	[[ "$bundled" =~ ^$version_re$ ]] || return 1
	version_newer "$version" "$bundled" || return 1
	[[ -f "$root/updates/$version/verdandi-claude-sidecar" && -f "$root/updates/$version/BUILD" ]]
}

sidecar_found() {
	[[ -n "${EITRI_SIDECAR_BINARY:-}" && -x "$EITRI_SIDECAR_BINARY" ]] && return 0
	selected_update_found && return 0
	[[ -x "$LIBDIR/verdandi-claude-sidecar" ]] && return 0
	# Only this install's RELEASE can name its pinned revision. Without it, skip the user path
	# rather than guessing which sidecar build belongs to this install.
	if [[ -f "$LIBDIR/RELEASE" ]]; then
		local rev7
		rev7="$(sed -n 's/^VERDANDI_REV=\(.......\).*/\1/p' "$LIBDIR/RELEASE" | head -n1)"
		[[ -n "$rev7" && -x "$(xdg_data_home)/eitri/sidecar/$rev7/verdandi-claude-sidecar" ]] && return 0
	fi
	return 1
}

if [[ -z "$QUIET" ]] && ! sidecar_found; then
	# A missing backend still permits the window to open, and the panel
	# says the same thing this line does.
	echo 'eitri: no sidecar built yet -- run "eitri setup" to build one' >&2
fi

# A launch that names a file may end in a running window, which keeps the account it has, so here nothing has been
# picked yet. If it goes on to a window of its own, the binary says what it took.
MAY_HAND_OVER=""
if [[ $MODE == window && -z "$PROJECT" && -z "$NEW_WINDOW" && ${#SHELL_ARGS[@]} -eq 0 ]]; then
	[[ $((${#BEFORE[@]} + ${#AFTER[@]})) -eq 1 && -d "$FIRST" ]] || MAY_HAND_OVER=1
fi

if [[ -z "$QUIET" ]]; then
	{
		if [[ $MODE == open && -z "$OPEN_DIR_GIVEN" ]]; then
			echo "eitri    project (the editor's working directory)"
		elif [[ $MODE != window || -n "$PROJECT" ]]; then
			echo "eitri    project $PROJECT"
		elif [[ -n "$FIRST" ]]; then
			echo "eitri    open $FIRST"
		fi
		if [[ -n "$MAY_HAND_OVER" ]]; then
			:
		elif [[ -n "$ACCOUNT_FROM_FLAG" ]]; then
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

if [[ $MODE == split ]]; then
	exec "$LIBDIR/shell" split -- "$PROJECT"
fi
if [[ $MODE == open ]]; then
	if [[ -n "$OPEN_DIR_GIVEN" ]]; then
		exec "$LIBDIR/shell" open "${PANEL_ARGS[@]}" -- "$PROJECT"
	fi
	exec "$LIBDIR/shell" open "${PANEL_ARGS[@]}"
fi
if [[ $MODE == panel ]]; then
	exec "$LIBDIR/shell" panel "${PANEL_ARGS[@]}" -- "$PROJECT"
fi
[[ -n "$CHOOSE" ]] && exec "$LIBDIR/shell" choose-project "${SHELL_ARGS[@]}"
[[ -n "$NEW_WINDOW" ]] && SHELL_ARGS+=(--new-window)
# A file manager gives the desktop entry everything selected as one list, and a folder after the first path is a
# refusal no screen would show. So each folder gets a window of its own, as it would from one launch per item, and
# the files stay together so that they can share a window.
if [[ -n "$FROM_DESKTOP" ]]; then
	FOLDER_LATER=""
	for a in "${BEFORE[@]:1}"; do
		if [[ -d "$a" ]]; then FOLDER_LATER=1; fi
	done
	if [[ -n "$FOLDER_LATER" ]]; then
		FILES=()
		for a in "${BEFORE[@]}"; do
			if [[ -d "$a" ]]; then
				"$LIBDIR/shell" "${SHELL_ARGS[@]}" -- "$a" &
			else
				FILES+=("$a")
			fi
		done
		if [[ ${#FILES[@]} -gt 0 ]]; then
			"$LIBDIR/shell" "${SHELL_ARGS[@]}" -- "${FILES[@]}" &
		fi
		wait
		exit 0
	fi
fi
# shell reads its first argument as a subcommand before it reads any path. A directory or file typed
# with one of those names (`eitri --quiet panel` in a folder called panel) would run the subcommand
# instead of opening the path; spelling it ./name keeps it the path the user typed, relative to the
# same directory.
if [[ ${#SHELL_ARGS[@]} -eq 0 && ${#BEFORE[@]} -gt 0 ]]; then
	case "${BEFORE[0]}" in
		open | panel | split | page | agents | remote | run-guard | linked-window | choose-project) BEFORE[0]="./${BEFORE[0]}" ;;
	esac
fi
if [[ -n "$SAW_DASHES" ]]; then
	exec "$LIBDIR/shell" "${SHELL_ARGS[@]}" "${BEFORE[@]}" -- "${AFTER[@]}"
fi
exec "$LIBDIR/shell" "${SHELL_ARGS[@]}" "${BEFORE[@]}"
