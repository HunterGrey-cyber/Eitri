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

cat > "$BINDIR/neovibe" <<'LAUNCHER'
#!/usr/bin/env bash
# neovibe, installed by install.sh. Everything it decides is printed before the window opens.
set -euo pipefail

LIBDIR="$HOME/.local/lib/neovibe"
BACKEND="${NEOVIBE_AGENT_BACKEND:-sidecar}"
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

# The compiled-in default points at ~/src/verdandi, which tracks Verdandi's `main` --
# protocol major 1, which this client (major 3) refuses at the handshake. The symptom is a sidecar
# that exits before binding its socket, which reads as "it will not start" rather than as a version
# mismatch. So the checkout is stated here, not defaulted.
VERDANDI="${NEOVIBE_VERDANDI_CHECKOUT:-$HOME/src/verdandi-old-checkout}"

if [[ "$BACKEND" == sidecar ]]; then
	if [[ ! -d "$VERDANDI/apps/claude-sidecar" ]]; then
		echo "neovibe: the sidecar backend needs a Verdandi checkout, and $VERDANDI is not one." >&2
		echo "         Set NEOVIBE_VERDANDI_CHECKOUT, or run with --legacy." >&2
		exit 1
	fi
	export NEOVIBE_VERDANDI_CHECKOUT="$VERDANDI"
fi
export NEOVIBE_AGENT_BACKEND="$BACKEND"
[[ -n "$ACCOUNT" ]] && export VERDANDI_CLAUDE_ACCOUNT="$ACCOUNT"

if [[ -z "${QUIET:-}" ]]; then
	{
		echo "neovibe  project $PROJECT"
		echo "         backend $BACKEND"
		if [[ "$BACKEND" == sidecar ]]; then
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
echo "  launcher   $BINDIR/neovibe"
echo "  desktop    $APPDIR/neovibe.desktop   (also offered for folders in Files)"
echo
echo "runtime dependencies this does NOT contain:"
echo "  nvim, claude            on PATH"
echo "  a Verdandi checkout     for the sidecar backend -- currently ~/src/verdandi-old-checkout"
echo "                          (node + npm, built on first use)"
echo
echo "this is a snapshot: re-run ./install.sh after changing the source."
case ":$PATH:" in *":$BINDIR:"*) ;; *) echo "note: $BINDIR is not on your PATH";; esac
