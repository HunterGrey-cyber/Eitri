#!/usr/bin/env bash
# Installs neovibe for this machine's own user. Not a distribution package: it installs a snapshot
# of what is built right now, and it depends on things that live outside it (see below).
#
#   ./install.sh            build release, install, write a desktop entry
#   ./install.sh --no-build use whatever is already in target/release
set -euo pipefail
cd "$(dirname "$(realpath "$0")")"

LIBDIR="$HOME/.local/lib/neovibe"
BINDIR="$HOME/.local/bin"
APPDIR="$HOME/.local/share/applications"

# All four are located as SIBLINGS of the running executable (`current_exe()`'s directory), so they
# must land in one directory together. Installing only `shell` produces a build that silently loses
# pane switching, the supervisor, and the legacy backend's permission gate.
BINS=(shell agent-hook neovibe-supervisor neovibe-tmux-shim neovibe-claude-handoff)

# The sixth sibling, and the one that decides the backend. `agent::packaged_sidecar_available()`
# looks for a `verdandi-claude-sidecar` beside the running executable; with it here, an installed
# copy runs the sidecar with nothing to build and nothing to find. Without it the binary falls back
# to a Verdandi checkout, which still works on this machine but makes the install depend on a
# directory outside itself -- so this warns rather than refusing, which is the opposite of
# `publish.sh`'s rule. A package is distributed and its degradation is invisible; a local install is
# being run by the person reading this line.
SIDECAR_ARTIFACT="${NEOVIBE_SIDECAR_ARTIFACT:-}"
if [[ -z "${SIDECAR_ARTIFACT}" ]]; then
	VERDANDI_SRC="${NEOVIBE_VERDANDI_CHECKOUT:-$HOME/src/verdandi}"
	# Newest first and read whole -- `| head -1` would SIGPIPE `ls` and, under pipefail, fail here.
	mapfile -t _artifacts < <(ls -t "${VERDANDI_SRC}"/apps/claude-sidecar/dist-bin/verdandi-claude-sidecar-* 2>/dev/null)
	[[ ${#_artifacts[@]} -gt 0 ]] && SIDECAR_ARTIFACT="${_artifacts[0]}"
fi

if [[ "${1:-}" != "--no-build" ]]; then
	cargo build --release -p shell -p agent -p supervisor --bins
fi

for b in "${BINS[@]}"; do
	[[ -x "target/release/$b" ]] || { echo "install: target/release/$b is missing -- build first" >&2; exit 1; }
done

# The packaged build wins over this one, which is the opposite of what a developer wants. `/usr/bin`
# precedes `~/.local/bin` in this machine's PATH, so with the distro package installed, `neovibe`
# keeps running /usr/lib/neovibe/shell no matter what this script writes -- you would be testing the
# release while believing you were testing your change. Say so rather than let it be discovered.
if pacman -Q neovibe >/dev/null 2>&1; then
	installed="$(pacman -Q neovibe | awk '{print $2}')"
	cat >&2 <<WARN

warning: the distro package neovibe ${installed} is installed, and /usr/bin/neovibe takes
         precedence over ${BINDIR}/neovibe on this PATH. After this script finishes, typing
         'neovibe' will still run the PACKAGED build, not the one it just installed.

         To test this build:   sudo pacman -R neovibe     (then re-run this script)
         Or run it directly:   ${LIBDIR}/shell <project>

WARN
fi

mkdir -p "$LIBDIR" "$BINDIR" "$APPDIR"
install -m755 "${BINS[@]/#/target/release/}" "$LIBDIR/"
if [[ -n "${SIDECAR_ARTIFACT}" && -x "${SIDECAR_ARTIFACT}" ]]; then
	install -m755 "${SIDECAR_ARTIFACT}" "$LIBDIR/verdandi-claude-sidecar"
else
	cat >&2 <<'NOSIDECAR'

warning: no built sidecar artifact found, so this install has none beside its binaries. It will
         fall back to a Verdandi checkout, which means this install depends on a directory outside
         itself and its first start may run npm. To make it self-contained:

             npm run build:binary -w @verdandi/claude-sidecar   (in the Verdandi checkout)
             ./install.sh --no-build                            (re-run this)

NOSIDECAR
fi

cat > "$BINDIR/neovibe" <<'LAUNCHER'
#!/usr/bin/env bash
# neovibe, installed by install.sh. Everything it decides is printed before the window opens.
set -euo pipefail

LIBDIR="$HOME/.local/lib/neovibe"
# Empty means "not set": the binary decides, and says which and why on stderr. This script used to
# default it to `sidecar`, which is now the binary's own job
# (`neovibe_core::agent_backend::BackendKind::choose`) -- two places deciding one thing is how they
# drift.
BACKEND="${NEOVIBE_AGENT_BACKEND:-}"
ACCOUNT="${VERDANDI_CLAUDE_ACCOUNT:-}"
ACCOUNT_FROM_FLAG=""
PROJECT=""

while [[ $# -gt 0 ]]; do
	case "$1" in
		--legacy)  BACKEND=legacy;  shift ;;
		--sidecar) BACKEND=sidecar; shift ;;
		--account) ACCOUNT="${2:?--account needs a name}"; ACCOUNT_FROM_FLAG=1; shift 2 ;;
		--quiet)   QUIET=1; shift ;;
		--) shift; PROJECT="${1:-}"; break ;;
		-*) echo "neovibe: unknown option $1" >&2; exit 64 ;;
		*)  PROJECT="$1"; shift ;;
	esac
done
PROJECT="$(realpath -e -- "${PROJECT:-$PWD}" 2>/dev/null)" || { echo "neovibe: no such directory" >&2; exit 1; }

# This paragraph used to say the default checkout tracked Verdandi's `main` at protocol major 1,
# which this client refuses at the handshake. That stopped being true on 2026-09-18: the pin moved
# to `8936a10`, which IS on Verdandi main, and a checkout there builds a protocol-3 sidecar.
VERDANDI="${NEOVIBE_VERDANDI_CHECKOUT:-$HOME/src/verdandi}"

# Only handed over when this install has no artifact of its own; otherwise a checkout in the
# operator's home directory would silently outrank the binary installed beside the product.
if [[ "$BACKEND" == sidecar && ! -x "$LIBDIR/verdandi-claude-sidecar" ]]; then
	if [[ ! -d "$VERDANDI/apps/claude-sidecar" ]]; then
		echo "neovibe: --sidecar needs either $LIBDIR/verdandi-claude-sidecar or a Verdandi" >&2
		echo "         checkout; $VERDANDI is not one. Set NEOVIBE_VERDANDI_CHECKOUT, or drop it." >&2
		exit 1
	fi
	export NEOVIBE_VERDANDI_CHECKOUT="$VERDANDI"
fi
[[ -n "$BACKEND" ]] && export NEOVIBE_AGENT_BACKEND="$BACKEND"
[[ -n "$ACCOUNT" ]] && export VERDANDI_CLAUDE_ACCOUNT="$ACCOUNT"

if [[ -z "${QUIET:-}" ]]; then
	{
		echo "neovibe  project $PROJECT"
		if [[ -n "$BACKEND" ]]; then
			echo "         backend $BACKEND (requested here)"
		elif [[ -x "$LIBDIR/verdandi-claude-sidecar" ]]; then
			echo "         backend decided by the binary; a sidecar artifact is installed beside it"
		else
			echo "         backend decided by the binary; no artifact here -- see its own line below"
		fi
		if [[ -x "$LIBDIR/verdandi-claude-sidecar" ]]; then
			echo "         sidecar $("$LIBDIR/verdandi-claude-sidecar" --version 2>/dev/null | head -1)"
		elif [[ -d "$VERDANDI/apps/claude-sidecar" ]]; then
			echo "         sidecar $VERDANDI @ $(git -C "$VERDANDI" rev-parse --short HEAD 2>/dev/null || echo '?')"
		fi
		if [[ -n "$ACCOUNT_FROM_FLAG" ]]; then
			echo "         account $ACCOUNT (--account)"
		elif [[ -n "$ACCOUNT" ]]; then
			# Distinguished from the flag on purpose: an account forced by the environment is still
			# a decision, but it was made somewhere else and this line is the only place the running
			# app says so.
			echo "         account $ACCOUNT (from VERDANDI_CLAUDE_ACCOUNT in the environment)"
		else
			echo "         account not pinned -- the sidecar will resolve its own; CLAUDE_PROFILE=${CLAUDE_PROFILE:-<unset>}"
		fi
	} >&2
fi

exec "$LIBDIR/shell" "$PROJECT"
LAUNCHER
chmod 755 "$BINDIR/neovibe"

cat > "$APPDIR/neovibe.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=neovibe
Comment=Neovim, embedded, with a Claude agent panel
Exec=$BINDIR/neovibe --quiet %f
Terminal=false
Categories=Development;TextEditor;
MimeType=inode/directory;
StartupNotify=true
DESKTOP
update-desktop-database "$APPDIR" 2>/dev/null || true

echo
echo "installed"
echo "  binaries   $LIBDIR/          ($(printf '%s ' "${BINS[@]}"))"
if [[ -x "$LIBDIR/verdandi-claude-sidecar" ]]; then
	echo "  sidecar    $LIBDIR/verdandi-claude-sidecar"
	echo "             $("$LIBDIR/verdandi-claude-sidecar" --version 2>/dev/null | head -1)"
fi
echo "  launcher   $BINDIR/neovibe"
echo "  desktop    $APPDIR/neovibe.desktop   (also offered for folders in Files)"
echo
echo "runtime dependencies this does NOT contain:"
echo "  nvim, claude            on PATH"
if [[ -x "$LIBDIR/verdandi-claude-sidecar" ]]; then
	echo "  (a Verdandi checkout is NOT one of them: the sidecar artifact above is self-contained)"
else
	echo "  a Verdandi checkout     for the sidecar backend -- currently ~/src/verdandi"
	echo "                          (node + npm, built on first use)"
fi
echo
echo "this is a snapshot: re-run ./install.sh after changing the source."
case ":$PATH:" in *":$BINDIR:"*) ;; *) echo "note: $BINDIR is not on your PATH";; esac
