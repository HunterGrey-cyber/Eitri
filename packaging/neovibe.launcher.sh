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

# LIBDIR is derived from where THIS script actually lives, never hardcoded: the one launcher ships
# in three shapes -- /usr/bin/neovibe -> /usr/lib/neovibe (.deb/.rpm), ~/.local/bin/neovibe ->
# ~/.local/lib/neovibe (the tarball installer, spec sec 6.5), and the tarball's own bin/neovibe ->
# lib/neovibe before it is installed anywhere.
#
# F5: resolve ONLY the launcher FILE's own symlink chain here (a package manager's alternatives
# system pointing `~/.local/bin/neovibe -> /usr/bin/neovibe`, or a plain
# `~/bin/neovibe -> ~/.local/bin/neovibe`) -- never a symlinked DIRECTORY along the way. The old
# `readlink -f "$0"` canonicalized every path component, including a symlinked `bin/` (e.g. GNU
# stow folding `~/.local/bin` into a dotfiles repo): the fully-resolved path then sits in the
# symlink's TARGET directory, whose sibling `../lib/neovibe` is not this install's lib at all, and
# every launch, `neovibe setup` included, died `cd: .../lib/neovibe: No such file or directory`.
#
# The `cd` below then gets `<the resolved file's directory>/../lib/neovibe` and resolves the `..`
# logically (its default): the `..` removes the directory before it as spelled, after checking that
# it names a directory (symlinks followed), so a symlinked bindir leads to ITS OWN sibling
# `lib/neovibe`. POSIX cd(1) does the same, so this holds under `POSIXLY_CORRECT=1` too. Outside
# POSIX mode, bash adds one thing: if the logical path does not exist, it retries physically. Only
# the unsupported inverse shape needs that retry: a symlinked bindir whose TARGET, not the bindir
# as named, has the sibling `lib/neovibe`. That shape therefore works only with `POSIXLY_CORRECT`
# unset, and F5 never asked for it.
#
# The parent is reached by appending `..`, never by a second `dirname`. An earlier version used
# `dirname`, and `dirname .` is `.`, not `..`, so it broke `./neovibe` run from inside an unpacked
# release's bin/ and `bin/./neovibe` run from its root. Appending `..` works for any spelling of the
# directory (`.`, `bin/.`, `..`).
#
# `CDPATH=''` on that `cd` is a separate fix: without it, an exported `CDPATH` (e.g. `.`) with an
# entry that matches the target makes bash's `cd` print the destination to stdout on top of `pwd`'s
# own line, corrupting LIBDIR into two lines glued together wherever that path is relative -- `$0`
# found through a relative `PATH` entry, or `bin/neovibe` run from an unpacked release directory
# (one of this launcher's three shapes, see above).
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

# `neovibe setup [args]` is a subcommand, handled before any flag parsing: it never opens a
# project or a window, so none of the flags below apply to it. `install.sh`/`neovibe-setup` is a
# POSIX `sh` script (spec sec 6.1), run with `sh` explicitly rather than executed directly, so this
# still works if it is ever installed without its execute bit.
#
# installer-claude-1 (+installer-codex-3): this used to force `--sidecar-only` onto every
# `neovibe setup` call, so `neovibe setup --nvim-only` (install.sh's own recovery hint) and
# `neovibe setup --uninstall` both died "--sidecar-only and X cannot be combined" --
# packaging/install.sh's own set_mode refuses a second mode. Every mode it has -- --nvim-only,
# --nvim-offer, --uninstall, --sidecar-only, --from-source, --build-sidecar-into -- and -h/--help
# (which needs no second call at all) passes straight through, unmodified, exactly once.
# packaging/test_launcher.sh reads that list of modes out of packaging/install.sh itself, so a mode
# added there and not here fails a test instead of dying "--sidecar-only and X" for a user (fix
# round 1 added --nvim-offer without it, and exactly that happened). Anything else
# is the plain case spec sec 6.1 describes ("build the sidecar (and offer nvim)"): build the sidecar
# first, then run the SAME nvim offer a normal install runs (packaging's own --nvim-offer: skipped
# when the nvim already on PATH is adequate, when the release's nvim is already installed privately,
# or when --no-nvim was given, and a download failure only warns) -- unlike --nvim-only above, which
# fetches even beside an adequate PATH nvim and dies on failure, `neovibe setup`
# without a mode flag must never turn "nvim was already fine" or "no network right now" into a
# failed setup. --yes/--with-nvim need no special handling to be "honoured" here: --sidecar-only
# ignores them already, and --nvim-offer reads them itself.
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
		# Forwarded, not decided here: which backend `--legacy` selects, and whether this build
		# has one to select, is `shell`'s own call (spec sec 10, D16) -- exactly the same
		# reasoning that already governs the sidecar's own discovery below. Unlike the old
		# `NEOVIBE_AGENT_BACKEND=legacy` export this replaced, this is plain argv: `shell` decides
		# per invocation, so two launches of the same installed binary can disagree.
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

# WHERE THE SIDECAR HINT LOOKS, and why it is not a decision.
#
# This only decides whether to print a hint before the window opens -- it never picks a backend and
# never exports anything. The real discovery (which sidecar, if any, `shell` actually uses) is
# `agent::providers::claude_sidecar::spawn::resolve_sidecar_program` (spec sec 5.2); this mirrors
# the two checks that matter for a "is there nothing at all" hint (an explicit override, and the
# per-user rev-keyed path Task 3's `user_sidecar_path` writes to) so a first run says so immediately
# rather than after a failed session start. A packaged sibling
# (`$LIBDIR/verdandi-claude-sidecar`, the private mirror's profile) is covered by the second check;
# the checkout-based discovery (`NEOVIBE_VERDANDI_CHECKOUT`, a dev override) is deliberately not
# mirrored here -- it never applies to an installed release build, which is the only thing this
# launcher ships for.
#
# <data> follows spec sec 6.4's XDG rule exactly (unset, empty or relative XDG_DATA_HOME means
# $HOME/.local/share) -- a plain `${XDG_DATA_HOME:-default}` would pass a relative value straight
# through and disagree with where the sidecar was actually built.
xdg_data_home() {
	case "${XDG_DATA_HOME:-}" in
		/*) printf '%s\n' "$XDG_DATA_HOME" ;;
		*)  printf '%s\n' "$HOME/.local/share" ;;
	esac
}

sidecar_found() {
	[[ -n "${NEOVIBE_SIDECAR_BINARY:-}" && -x "$NEOVIBE_SIDECAR_BINARY" ]] && return 0
	[[ -x "$LIBDIR/verdandi-claude-sidecar" ]] && return 0
	# The rev-keyed per-user path is keyed by the FULL install's own pinned revision, which only a
	# `RELEASE` beside this launcher's LIBDIR can name (spec sec 4.3); a development build with no
	# `RELEASE` at all just skips this third check rather than guessing a revision.
	if [[ -f "$LIBDIR/RELEASE" ]]; then
		local rev7
		rev7="$(sed -n 's/^VERDANDI_REV=\(.......\).*/\1/p' "$LIBDIR/RELEASE" | head -n1)"
		[[ -n "$rev7" && -x "$(xdg_data_home)/neovibe/sidecar/$rev7/verdandi-claude-sidecar" ]] && return 0
	fi
	return 1
}

if [[ -z "$QUIET" ]] && ! sidecar_found; then
	# Not an error (I2/I7): the window still opens with no agent backend to spawn, and the panel
	# says the same thing this line does.
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
