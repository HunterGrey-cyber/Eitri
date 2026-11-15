#!/bin/sh
{
# Eitri's installer: download a release, verify it, install it for this user, upgrade it, or
# uninstall it. Nothing here runs sudo
# or a package manager: when something is missing it prints the command and stops.
#
#   curl --proto '=https' --proto-redir '=https' --tlsv1.2 -sSfL https://github.com/HunterGrey-cyber/eitri/releases/latest/download/install.sh | sh
#   curl --proto '=https' --proto-redir '=https' --tlsv1.2 -sSfL .../install.sh | sh -s -- --version 0.4.0 --yes
#   eitri setup        (this same file, installed as lib/eitri/eitri-setup)
#
# `sh install.sh --help` lists the options.
#
# Three rules plain `set -eu` does not give; packaging/tests/install/ holds each:
#
# 1. Nothing runs until the whole script arrives: the brace group opens on the second line and
#    ends with main "$@", and its closing brace is the last line. The shell parses that whole
#    command first, so a cut anywhere after the first line runs nothing (rm -rf, swaps and
#    uninstall included) and sh exits with a syntax error. Main outside the group would let a cut
#    after `}; main` invoke it without the intended arguments. A download that delivered no byte
#    leaves sh an empty script, which exits 0: only the pipeline's own status (pipefail) or a
#    download to a file can report that.
# 2. `set -e` is never relied on inside a function. POSIX (dash included) ignores it in any
#    function called from an `if`, `&&`, `||` or `!` context, where a failed `tar` or `mv` would
#    carry on. So every fallible command ends in `|| die "..."` (or `|| return 1`), and every
#    function is called only as a plain statement or a plain assignment `x=$(f)` -- predicates
#    report through a variable, never through their exit status. `shellcheck -s sh -o
#    check-set-e-suppressed` (SC2310/SC2311) holds this. One case it cannot see: bash outside
#    POSIX mode (`bash install.sh`, `curl | bash`) turns errexit off inside $(...), so a function
#    that runs inside $(...) never itself assigns `x=$(g)` from a function g that can die -- g's
#    die would be lost there. Such a function sets a variable instead and runs as a plain
#    statement (rev_of_release, lib_dirs_for).
# 3. stdin is the script under `curl | sh`, so it is never read: `main` runs with stdin from
#    /dev/null, and a prompt may only ever read /dev/tty (`read ans </dev/tty`), never decide
#    whether to prompt by testing if stdin is a terminal (it is a pipe for every curl user).
set -eu

NV_DEFAULT_BASE_URL='https://github.com/HunterGrey-cyber/eitri'
NV_LAUNCHER_MARKER='# eitri-launcher v1'
# The launcher signature written by the root install.sh is recognized as Eitri's,
# so an upgrade can replace it instead of treating it as a foreign launcher.
NV_OLD_LAUNCHER_LINE2='# neovibe, installed by install.sh. Everything it decides is printed before the window opens.'
NV_SIGNER_IDENTITY='release@eitri'
NV_SIGNATURE_NAMESPACE='eitri-release'
# The same key signs sidecar releases under their own namespace, so a signature made for one purpose
# cannot be replayed as the other. Sidecar releases live in their own public repository.
NV_SIDECAR_NAMESPACE='eitri-sidecar-release'
NV_SIDECAR_REPO_BASE=https://github.com/HunterGrey-cyber/verdandi
NV_GTK_FLOOR_MINOR=14
NV_GLIBC_FLOOR='2.39'
NV_NVIM_FLOOR='0.11.0'
# Retired companions are removed from staged archives before the install is swapped in.
NV_OLD_BINARIES='eitri-supervisor'
NV_BINARIES='shell eitri-tmux-shim eitri-claude-handoff eitri-run-guard eitri-editor-bridge eitri-server'
# First release line requiring the server helper, including that line's candidates.
NV_SERVER_SINCE=0.3.2
# First release line requiring the editor bridge, including that line's candidates.
NV_BRIDGE_SINCE=0.3.1
# The oldest release line whose layout this installer lays out, including that line's candidates. From
# 0.3.1 on, a release carries the page window's desktop entry, the nvim plugin's health check
# (lua/eitri/health.lua) and the agents-strips, agents-top and agents-pass plugins; 0.3.0 carries none of
# them. An older version is refused before its tarball is fetched (refuse_older_layout), with the way to
# install it with its own installer.
NV_LAYOUT_SINCE=0.3.1
NV_LICENCE_FILES='LICENSE THIRD-PARTY-LICENSES SOURCE'
# The desktop entry is named by the application id (the compositor matches a window to <id>.desktop;
# the icon theme's icon name and the window's class, shell/src/main.rs's APP_ID, are the same string).
# Releases before the app icon installed the entry as eitri.desktop; NV_OLD_DESKTOP is what an
# install or an uninstall removes of it, and only when it is byte for byte what they wrote.
#
# NV_DESKTOP is every entry the release installs (a space-separated list: nothing in a name has a space);
# NV_MAIN_DESKTOP is the one that is the application, named in messages. The other is the companion
# panel's own entry (`eitri panel`, its own window class), hidden from menus.
NV_MAIN_DESKTOP=cn.huntergrey.eitri.desktop
NV_DESKTOP='cn.huntergrey.eitri.desktop cn.huntergrey.eitri.Panel.desktop cn.huntergrey.eitri.Page.desktop'
NV_OLD_DESKTOP=eitri.desktop
# The nvim plugin that adds :EitriPanel: in the release tarball at share/eitri/eitri.nvim/<this path>, and
# installed to $XDG_DATA_HOME/eitri/eitri.nvim/<this path>. Not under eitri/nvim/: that directory holds
# the private nvim versions this installer scans and removes. A test holds this list to the committed
# files.
NV_PLUGIN_FILES='plugin/eitri.lua lua/eitri/init.lua lua/eitri/health.lua doc/eitri.txt'
# The GNOME Shell extension a companion panel on GNOME uses to move focus to and from its editor: in the release
# tarball at share/gnome-shell/extensions/<NV_EXT_ID>/<file>, and installed to
# $XDG_DATA_HOME/gnome-shell/extensions/<NV_EXT_ID>/<file>, where the shell looks for a user's extensions.
# Only the four files the shell loads: the extension's testing.js and test/ never ship. This installer puts
# the files in place and never runs `gnome-extensions`: enabling is the user's own step. A test holds this
# list to the committed files.
NV_EXT_ID='eitri@huntergrey.cn'
NV_EXT_FILES='metadata.json extension.js direction.js policy.js'
# The bundled Markdown preview and agents faces: in the release tarball at
# lib/eitri/plugins/<name>/ (its eitri-plugin.toml and its built site under web/dist/), so they are installed, upgraded
# and removed with the binaries and the running binary finds them beside itself; never in the user's data directory,
# whose own plugins the window looks at first. A from-source install stages them from the tree it built. A test holds
# this list to the harness's.
NV_SHIPPED_PLUGINS='markdown agents-strips agents-top agents-pass'
# The icon, one file per size under the icon theme's tree: each is in the release tarball at
# share/icons/<this path> and goes to $XDG_DATA_HOME/icons/<this path> (packaging/release_check.py's
# role table holds the tarball to exactly this set, and a test holds the two lists equal).
NV_ICON_FILES='hicolor/16x16/apps/cn.huntergrey.eitri.png hicolor/24x24/apps/cn.huntergrey.eitri.png hicolor/32x32/apps/cn.huntergrey.eitri.png hicolor/48x48/apps/cn.huntergrey.eitri.png hicolor/64x64/apps/cn.huntergrey.eitri.png hicolor/128x128/apps/cn.huntergrey.eitri.png hicolor/256x256/apps/cn.huntergrey.eitri.png hicolor/512x512/apps/cn.huntergrey.eitri.png hicolor/scalable/apps/cn.huntergrey.eitri.svg'
NV_VERSION_ERE='[0-9]+\.[0-9]+\.[0-9]+(-rc\.[0-9]+)?'
# A sidecar version, as a tab reads it. It names a directory, so a number has one spelling, and 18 digits
# are what every reader can compare. Nothing that parses here is refused by a tab.
NV_SIDECAR_NUMBER_ERE='(0|[1-9][0-9]{0,17})'
NV_SIDECAR_VERSION_ERE="$NV_SIDECAR_NUMBER_ERE\\.$NV_SIDECAR_NUMBER_ERE\\.$NV_SIDECAR_NUMBER_ERE(-rc\\.$NV_SIDECAR_NUMBER_ERE)?"
NV_ISSUES='https://github.com/HunterGrey-cyber/eitri/issues'
# Reads a sidecar's --version --json on stdin and prints what a sidecar update compares with its signed
# manifest, one KEY=value line each. Run by the pinned Node; POSIX sh has no JSON parser.
NV_SIDECAR_FACTS_JS='
const r = JSON.parse(require("fs").readFileSync(0, "utf8"));
const p = r.protocol || {};
console.log("sidecar_version=" + r.sidecar_version);
console.log("protocol=" + p.major + "." + p.minor);
console.log("node=" + r.node);
console.log("sdk_version=" + r.sdk_version);
console.log("build_stamp=" + r.build_stamp);
console.log("executables=" + JSON.stringify(r.executables));
'
# The sidecar build. The protocol major matches Verdandi's own
# apps/claude-sidecar/src/runtimeServiceImpl.ts PROTOCOL_MAJOR, checked against a built artifact's
# own --version rather than assumed. Node's dist base is fixed in production; test mode replaces it
# (node_dist_base) so a sandboxed run can never reach the real nodejs.org by accident.
NV_SIDECAR_PROTOCOL_MAJOR=3
NV_NODE_DIST_BASE=https://nodejs.org/dist
# Where a sidecar update takes the protoc that grpc-tools' install script would otherwise fetch with
# no hash. Fixed in production; test mode replaces it (grpc_dist_base), as it does Node's.
NV_GRPC_DIST_BASE=https://node-precompiled-binaries.grpc.io
NV_SIDECAR_BUILD_MIN_KIB=1468006
NV_SIDECAR_UPDATE_MIN_KIB=1258291
# The nvim offer. Never Eitri's own release server -- this is neovim's own GitHub
# releases. nvim_dist_base's own test-mode default (a refused loopback port) keeps every test that
# forgets to redirect it from reaching the real host, the same discipline as node_dist_base.
NV_NVIM_DIST_BASE=https://github.com/neovim/neovim/releases/download
# --from-source: the public repositories it clones. Overridable only in test mode
# (EITRI_INSTALL_TEST_REPO_URL / EITRI_INSTALL_TEST_VERDANDI_REPO_URL), which the
# EITRI_INSTALL_TEST_* pattern in run-in-env.sh's allowlist already covers -- no new entry there.
NV_EITRI_REPO_URL=https://github.com/HunterGrey-cyber/eitri.git
NV_VERDANDI_REPO_URL=https://github.com/HunterGrey-cyber/verdandi.git
NV_MIN_RUSTC=1.96.0
NV_NL='
'
# The AppArmor user-namespace restriction (apparmor_note, below): where the kernel says whether it
# is on, and where a profile is installed. Test mode replaces both (set_paths).
NV_PROC=/proc
NV_APPARMOR_D=/etc/apparmor.d

# Test mode (packaging/tests/install/ only). With EITRI_INSTALL_TEST=1 -- and only then -- three
# more variables are read, each standing in for a file of the host the harness must not depend on:
#   EITRI_INSTALL_TEST_LIBDIRS          colon-separated directories searched for the GTK and
#                                         WebKit sonames instead of `ldconfig -p` and the usual lib
#                                         directories; a directory's `ldconfig-p.txt` is read as
#                                         fake `ldconfig -p` output;
#   EITRI_INSTALL_TEST_SYSTEM_RELEASE   read instead of /usr/lib/eitri/RELEASE;
#   EITRI_INSTALL_TEST_OS_RELEASE       read instead of /etc/os-release;
#   EITRI_INSTALL_TEST_PROC             a directory read instead of /proc (only its
#                                         sys/kernel/apparmor_restrict_unprivileged_userns);
#   EITRI_INSTALL_TEST_APPARMOR_D       a directory standing in for /etc/apparmor.d;
#   EITRI_INSTALL_TEST_NET_DELAY        seconds before the first retry of a download (default 0,
#                                         doubling up to eight times this, like NV_NET_DELAY);
#   EITRI_INSTALL_TEST_NET_STALL        seconds under 1 KiB/s before an attempt is given up.
# PROC and APPARMOR_D default, in test mode, to directories that do not exist, so a test that does not set
# them never reads this host's own restriction or profiles.
# None of them touches the trust anchor: the release key is never taken from the environment (only
# --release-signers replaces it, with a warning). At worst they make a system check pass or keep
# one more sidecar. The URL overrides further down (EITRI_INSTALL_TEST_*_BASE_URL, *_REPO_URL) are
# refused unless they name a loopback http server or an absolute local path (set_paths).
NV_TEST_MODE=0

OPT_MODE=install
OPT_HELP=0
OPT_VERSION=
OPT_BASE_URL=
OPT_SIGNERS=
OPT_TARBALL=
OPT_SUMS=
OPT_SIG=
OPT_PURGE=0
OPT_DRY_RUN=0
OPT_ALLOW_ROOT=0
OPT_NVIM=
OPT_RELEASE_FILE=
OPT_CHECKOUT=
OPT_VERDANDI_CHECKOUT=
OPT_KEEP_BUILD=0
OPT_NODE_TARBALL=
OPT_VERDANDI_SOURCE_TARBALL=
OPT_BUILD_SIDECAR_INTO=
OPT_SIDECAR_USE=
OPT_YES=0
OPT_ALLOW_VERDANDI_REV_MISMATCH=0
OPT_INSECURE_SKIP_SIGNATURE=0

LOCK_HELD=0
TAKEOVER_HELD=0
NV_LOCK=
NV_NEW_CREATED=0
NV_CACHE_CREATED=0
# Every download's bounds: refused past 1 GiB (the release tarball and the Node, nvim and Skia
# archives are tens of MB); an attempt is given up when it moves under 1 KiB/s for a minute or cannot
# connect within 30 seconds, so a server that never ends or stalls a transfer cannot hold a run for
# good. A broken or stalled attempt is tried again (net_get), resuming the partial file, until
# NV_NET_TRIES (8) attempts in a row have added nothing, about five minutes on a link that times out
# every connection, or NV_NET_MAX_TRIES (20) were made in all, waiting 2, 4, 8, then 16 seconds between
# attempts; a slow link that keeps moving still finishes.
NV_MAX_DOWNLOAD_BYTES=1073741824
NV_NET_CONNECT=30
NV_NET_STALL=60
NV_NET_TRIES=8
NV_NET_MAX_TRIES=20
NV_NET_DELAY=2
# A sidecar release's own files are small, and each is held to what it can reasonably be: its
# SHA256SUMS, .sig and SIDECAR-RELEASE, then its source and protoc archives.
NV_SIDECAR_META_MAX_BYTES=65536
NV_SIDECAR_ARCHIVE_MAX_BYTES=67108864
NV_SYS_REV7=
NV_SIDECAR_WORKDIR=
NV_SIDECAR_TMP=
NV_NVIM_TMP_DEST=
NV_VERDANDI_SOURCE_TARBALL_OVERRIDE=
NV_SIDECAR_ACTUAL_VERDANDI_REV=
# Build inputs from either RELEASE or SIDECAR-RELEASE. Only legacy RELEASE files may
# leave both empty and let npm fetch the compiler itself.
SB_NPM_CI_FLAGS=
SB_PROTOC_TARBALL=
# A sidecar update leaves $NV_LIB and what surrounds it alone: on a .deb that path is not this
# install's, and a repair of it would belong to a run that never touched it.
NV_LEAVE_LIB=0
# What on_exit removes or puts back after a sidecar update or check.
NV_CACHE_NV_CREATED=0
NV_SIDECAR_CHECK_DIR=
NV_UPDATES_CREATED=0
NV_UPDATE_TMP=
NV_UPDATE_ASIDE=
NV_UPDATE_DEST=
NV_SELECTED_TMP=

say() { printf 'eitri: %s\n' "$*"; }
warn() { printf 'eitri: warning: %s\n' "$*" >&2; }
die() {
	printf 'eitri: error: %s\n' "$*" >&2
	exit 1
}

usage() {
	cat <<'EOF'
Eitri installer -- installs Eitri for the user running it, into ~/.local.

usage: sh install.sh [options]

  (no option)                  install or upgrade to the latest release
  --version X                  a specific release, e.g. 0.2.0 (also the only way to install an
                               older version than the one installed)
  --yes, --no-nvim, --with-nvim
                               non-interactive answers to the prompt offering to install a private
                               copy of nvim; --yes is the generic affirmative, --no-nvim/--with-nvim
                               answer it by name
  --nvim-only                  the nvim offer alone, for an already-installed Eitri (reads RELEASE
                               beside this script, or --release-file FILE); always fetches unless
                               that exact nvim is already installed privately, and a download
                               failure is fatal
  --nvim-offer                 the same nvim offer a normal install runs, for an already-installed
                               Eitri -- skipped when the nvim already on PATH is new enough, when
                               the release's nvim is already installed privately, or with --no-nvim;
                               a download failure only warns
  --from-source [--checkout DIR] [--verdandi-checkout DIR [--allow-verdandi-rev-mismatch]]
                               build Eitri from source, then install exactly like a prebuilt
                               release. Without --checkout: clone the public repo at the chosen
                               version's tag and refuse unless its HEAD matches the verified
                               release's EITRI_COMMIT. With --checkout DIR: build DIR as it is,
                               synthesizing a RELEASE from its own version, HEAD, the root
                               Cargo.toml's workspace Verdandi rev and packaging/pins.env
                               (a local development build).
                               --verdandi-checkout DIR builds the sidecar's source from a local
                               Verdandi checkout instead of the pinned public source; refused unless
                               it is at the pinned revision and clean, unless
                               --allow-verdandi-rev-mismatch
  --base-url URL               a mirror of the release assets, laid out as GitHub's
                               (URL/releases/latest/download/..., URL/releases/download/vX/...).
                               https:// only; plain http only for http://127.0.0.1[:port] or
                               http://localhost[:port]
  --tarball FILE --sums FILE [--sig FILE]
                               install from files already downloaded
  --release-signers FILE       check SHA256SUMS.sig against FILE instead of the key built into this
                               installer (release candidates, tests); warns every time
  --insecure-skip-signature    on a machine without ssh-keygen, install without checking the
                               release signature (refused otherwise). Only the checksums are then
                               checked: they catch a corrupt download, not a release someone else
                               built. Where ssh-keygen is installed the signature is checked anyway
  --allow-root                 run as root anyway (containers)
  --uninstall [--purge]        remove Eitri; --purge also removes its settings, $XDG_CONFIG_HOME/eitri
                               (else ~/.config/eitri; never $EITRI_CONFIG_DIR), and its state
  --dry-run                    print every action without doing it
  --keep-build                 keep the sidecar build directory
  --sidecar-only [--release-file FILE]
                               build the sidecar alone, for an already-installed Eitri; reads
                               RELEASE from beside this script, or FILE. `eitri setup` with no
                               mode flag runs this, then --nvim-offer above
  --sidecar-check [--version X]
                               ask for the newest sidecar release and say whether this Eitri can
                               take it; changes nothing. `eitri sidecar check` runs this
  --sidecar-update [--version X]
                               fetch a signed sidecar release, build it on this machine, try it
                               and select it for new tabs; the bundled sidecar stays as the fallback.
                               --dry-run, --insecure-skip-signature, --tarball, --sums and --sig do
                               not apply to either. `eitri sidecar update` runs this
  --sidecar-status             say what new tabs run, what is bundled and what is kept; reads files
                               and takes no lock. `eitri sidecar status` runs this
  --sidecar-use bundled|VERSION
                               go back to the bundled sidecar, or select an update already built
                               that this Eitri accepts. `eitri sidecar use` runs this
  --build-sidecar-into DIR --node TARBALL --verdandi-source TARBALL [--release-file FILE]
                               build the sidecar from files already on disk and install it into DIR
                               instead of the per-user path (the eitri-bin AUR package's build())
  -h, --help                   this text
EOF
}

set_mode() {
	if [ "$OPT_MODE" != install ] && [ "$OPT_MODE" != "$1" ]; then
		die "--$OPT_MODE and --$1 cannot be combined"
	fi
	OPT_MODE=$1
}

parse_args() {
	while [ $# -gt 0 ]; do
		case $1 in
		--version | --base-url | --release-signers | --tarball | --sums | --sig | --release-file | \
			--checkout | --verdandi-checkout | --node | --verdandi-source | --build-sidecar-into | --sidecar-use)
			if [ $# -lt 2 ]; then die "$1 needs a value (see --help)"; fi
			case $1 in
			--version) OPT_VERSION=${2#v} ;;
			--base-url) OPT_BASE_URL=$2 ;;
			--release-signers) OPT_SIGNERS=$2 ;;
			--tarball) OPT_TARBALL=$2 ;;
			--sums) OPT_SUMS=$2 ;;
			--sig) OPT_SIG=$2 ;;
			--release-file) OPT_RELEASE_FILE=$2 ;;
			--checkout) OPT_CHECKOUT=$2 ;;
			--verdandi-checkout) OPT_VERDANDI_CHECKOUT=$2 ;;
			--node) OPT_NODE_TARBALL=$2 ;;
			--verdandi-source) OPT_VERDANDI_SOURCE_TARBALL=$2 ;;
			--build-sidecar-into)
				OPT_BUILD_SIDECAR_INTO=$2
				set_mode build-sidecar-into
				;;
			--sidecar-use)
				OPT_SIDECAR_USE=$2
				set_mode sidecar-use
				;;
			esac
			shift
			;;
		-h | --help) OPT_HELP=1 ;;
		# The nvim offer's own non-interactive "yes": --with-nvim/--no-nvim answer the
		# offer by name, --yes is the generic affirmative this installer's only prompt also honours.
		--yes) OPT_YES=1 ;;
		--keep-build) OPT_KEEP_BUILD=1 ;;
		--no-nvim) OPT_NVIM=no ;;
		--with-nvim) OPT_NVIM=yes ;;
		--uninstall) set_mode uninstall ;;
		--sidecar-only) set_mode sidecar-only ;;
		--sidecar-update) set_mode sidecar-update ;;
		--sidecar-check) set_mode sidecar-check ;;
		--sidecar-status) set_mode sidecar-status ;;
		--nvim-only) set_mode nvim-only ;;
		--nvim-offer) set_mode nvim-offer ;;
		--from-source) set_mode from-source ;;
		--allow-verdandi-rev-mismatch) OPT_ALLOW_VERDANDI_REV_MISMATCH=1 ;;
		--purge) OPT_PURGE=1 ;;
		--allow-root) OPT_ALLOW_ROOT=1 ;;
		--insecure-skip-signature) OPT_INSECURE_SKIP_SIGNATURE=1 ;;
		--dry-run) OPT_DRY_RUN=1 ;;
		*) die "unknown option: $1 (see --help)" ;;
		esac
		shift
	done
	# An update is optional, so a machine that cannot check one goes without: none of the ways past
	# a signature an install has applies here. A dry run skips the check too.
	case $OPT_MODE in
	sidecar-update | sidecar-check | sidecar-use | sidecar-status)
		if [ "$OPT_INSECURE_SKIP_SIGNATURE" = 1 ]; then
			die "--insecure-skip-signature does not apply to a sidecar release: an update is optional, so a machine that cannot check one goes without"
		fi
		if [ "$OPT_DRY_RUN" = 1 ]; then
			die "--dry-run does not apply to eitri sidecar: a dry run would skip a release's signature, and status and check change nothing"
		fi
		if [ -n "$OPT_TARBALL$OPT_SUMS$OPT_SIG" ]; then
			die "--tarball, --sums and --sig do not apply to a sidecar release: an update fetches what it checks"
		fi
		;;
	esac
	case $OPT_MODE in
	sidecar-use | sidecar-status)
		if [ -n "$OPT_VERSION" ]; then die "--version goes with eitri sidecar update and check"; fi
		;;
	esac
	if [ "$OPT_PURGE" = 1 ] && [ "$OPT_MODE" != uninstall ]; then
		die "--purge only goes with --uninstall"
	fi
	if [ -n "$OPT_VERSION" ] && ! printf '%s\n' "$OPT_VERSION" | grep -Eqx "$NV_VERSION_ERE"; then
		die "--version $OPT_VERSION: expected a release version such as 0.2.0 or 0.2.0-rc.1"
	fi
	if [ -n "$OPT_TARBALL$OPT_SUMS$OPT_SIG" ]; then
		if [ -z "$OPT_TARBALL" ] || [ -z "$OPT_SUMS" ]; then
			die "--tarball and --sums go together (and --sig with them): see --help"
		fi
		if [ -n "$OPT_BASE_URL" ]; then die "--tarball installs from files, so --base-url does not apply"; fi
	fi
	if [ -n "$OPT_RELEASE_FILE$OPT_CHECKOUT$OPT_VERDANDI_CHECKOUT" ] && [ "$OPT_MODE" = install ]; then
		die "--release-file, --checkout and --verdandi-checkout go with --sidecar-only or --from-source"
	fi
	if [ "$OPT_MODE" = build-sidecar-into ]; then
		if [ -z "$OPT_NODE_TARBALL" ] || [ -z "$OPT_VERDANDI_SOURCE_TARBALL" ]; then
			die "--build-sidecar-into needs --node FILE and --verdandi-source FILE too: see --help"
		fi
	elif [ -n "$OPT_NODE_TARBALL$OPT_VERDANDI_SOURCE_TARBALL" ]; then
		die "--node and --verdandi-source go with --build-sidecar-into: see --help"
	fi
	if [ "$OPT_ALLOW_VERDANDI_REV_MISMATCH" = 1 ] && [ -z "$OPT_VERDANDI_CHECKOUT" ]; then
		die "--allow-verdandi-rev-mismatch only means something with --verdandi-checkout DIR: see --help"
	fi
	if [ -n "$OPT_VERDANDI_CHECKOUT" ] && [ "$OPT_MODE" != from-source ]; then
		die "--verdandi-checkout goes with --from-source: see --help"
	fi
	if [ "$OPT_MODE" = from-source ] && [ -n "$OPT_TARBALL$OPT_SUMS$OPT_SIG" ]; then
		die "--tarball/--sums/--sig install from a downloaded tarball; --from-source builds one instead. Use one or the other"
	fi
}

# ---------------------------------------------------------------------------------------------
# Paths

check_home() {
	case ${HOME-} in
	/*) ;;
	*) die "HOME is unset or not an absolute path: set HOME to your home directory and re-run" ;;
	esac
	case $HOME in *"$NV_NL"*) die "HOME contains a newline: set HOME to your home directory and re-run" ;; esac
	NV_HOME=$HOME
	while :; do
		case $NV_HOME in
		?*/) NV_HOME=${NV_HOME%/} ;;
		*) break ;;
		esac
	done
	# After the trim, so `//` is refused as `/` is; and a `.` or `..` component could name / (or
	# anywhere) too. Every path this installer writes or removes starts with $HOME.
	if [ "$NV_HOME" = / ]; then die "HOME is $HOME, the root directory: set HOME to your own home directory and re-run"; fi
	case /$NV_HOME/ in
	*/./* | */../*) die "HOME ($HOME) has a . or .. component: set HOME to your home directory's plain absolute path and re-run" ;;
	esac
}

# xdg_dir data|cache|state: sets XD_DIR to the absolute non-empty XDG value, else
# $HOME/.local/share, $HOME/.cache or $HOME/.local/state respectively, matching Rust so setup and
# eitri agree on sidecar locations. ${VAR:-default} accepts relative
# values; quoted ~ does not expand, so defaults use $HOME.
# Refuse newlines: paths containing one can split the uninstall target list and remove unrelated
# directories such as ~/Documents. Falling back instead would disagree with Rust's absolute-path
# handling. Set a variable and call as a plain statement because failures must propagate (rule 2).
xdg_dir() {
	case $1 in
	data) _xd_n=XDG_DATA_HOME _xd_v=${XDG_DATA_HOME-} _xd_d=$NV_HOME/.local/share ;;
	cache) _xd_n=XDG_CACHE_HOME _xd_v=${XDG_CACHE_HOME-} _xd_d=$NV_HOME/.cache ;;
	state) _xd_n=XDG_STATE_HOME _xd_v=${XDG_STATE_HOME-} _xd_d=$NV_HOME/.local/state ;;
	*) die "xdg_dir: unknown kind $1" ;;
	esac
	case $_xd_v in
	*"$NV_NL"*) die "$_xd_n contains a newline, so no path built from it can be trusted: set $_xd_n to one absolute directory (or unset it) and re-run" ;;
	/*) XD_DIR=$_xd_v ;;
	*) XD_DIR=$_xd_d ;;
	esac
}

# settings_dir sets NV_CONFIG by Linux's settings rule: absolute XDG_CONFIG_HOME/eitri,
# else $HOME/.config/eitri. Ignore EITRI_CONFIG_DIR because it may name another program's settings;
# nothing here may remove that directory. Only uninstall removes settings, so it refuses newlines
# there; install may still name the directory in a message. Trim trailing slashes, as for HOME,
# so purge compares equal paths instead of removing a directory while claiming it was kept.
# A value made only of slashes denotes root and produces /eitri.
settings_dir() {
	case ${XDG_CONFIG_HOME-} in
	/*)
		_sd_x=$XDG_CONFIG_HOME
		while :; do
			case $_sd_x in
			?*/) _sd_x=${_sd_x%/} ;;
			*) break ;;
			esac
		done
		NV_CONFIG=${_sd_x%/}/eitri
		;;
	*) NV_CONFIG=$NV_HOME/.config/eitri ;;
	esac
}

set_paths() {
	NV_LIBROOT=$NV_HOME/.local/lib
	NV_LIB=$NV_LIBROOT/eitri
	NV_BINDIR=$NV_HOME/.local/bin
	xdg_dir data
	NV_DATA=$XD_DIR
	xdg_dir cache
	NV_CACHE=$XD_DIR
	xdg_dir state
	NV_STATE=$XD_DIR
	settings_dir
	NV_CACHE_NV=$NV_CACHE/eitri
	NV_DL=$NV_CACHE_NV/download
	NV_STAGE=$NV_CACHE_NV/unpack
	NV_SIDECAR_ROOT=$NV_DATA/eitri/sidecar
	NV_SIDECAR_UPDATES=$NV_SIDECAR_ROOT/updates
	NV_SIDECAR_SELECTED=$NV_SIDECAR_ROOT/selected
	NV_SYSTEM_RELEASE=/usr/lib/eitri/RELEASE
	# What rev_of_release tells the user to do about a system RELEASE it cannot use: that file is a
	# package's, which this installer never touches.
	NV_SYSTEM_REMEDY='reinstall or remove the Eitri package that owns it (this installer never changes it)'
	NV_OS_RELEASE=/etc/os-release
	NV_TEST_LIBDIRS=
	if [ "${EITRI_INSTALL_TEST-}" = 1 ]; then
		NV_TEST_MODE=1
		NV_SYSTEM_RELEASE=${EITRI_INSTALL_TEST_SYSTEM_RELEASE-$NV_SYSTEM_RELEASE}
		NV_OS_RELEASE=${EITRI_INSTALL_TEST_OS_RELEASE-$NV_OS_RELEASE}
		NV_TEST_LIBDIRS=${EITRI_INSTALL_TEST_LIBDIRS-}
		NV_PROC=${EITRI_INSTALL_TEST_PROC-/nonexistent/eitri-test-proc}
		NV_APPARMOR_D=${EITRI_INSTALL_TEST_APPARMOR_D-/nonexistent/eitri-test-apparmor.d}
		# A test waits no time between download attempts unless it asks to, and may shorten the
		# stall bound so a stalling fixture server is given up in seconds.
		NV_NET_DELAY=${EITRI_INSTALL_TEST_NET_DELAY-0}
		NV_NET_STALL=${EITRI_INSTALL_TEST_NET_STALL-$NV_NET_STALL}
		for _sp_v in "$NV_NET_DELAY" "$NV_NET_STALL"; do
			case $_sp_v in
			0 | [1-9] | [1-9]*[0-9]) case $_sp_v in *[!0-9]*) _sp_v=bad ;; esac ;;
			*) _sp_v=bad ;;
			esac
			if [ "$_sp_v" = bad ]; then die "EITRI_INSTALL_TEST_NET_DELAY and EITRI_INSTALL_TEST_NET_STALL are whole seconds without a leading zero"; fi
		done
		# The download overrides lead only to a loopback server and the repository overrides only to a
		# local path: a test mode left switched on in an environment can then send no download, clone
		# or build anywhere else.
		for _sp_v in NODE NVIM SKIA GRPC; do
			eval "_sp_u=\${EITRI_INSTALL_TEST_${_sp_v}_BASE_URL-}"
			if [ -n "$_sp_u" ]; then
				loopback_http "$_sp_u"
				if [ "$LH" != 1 ]; then die "EITRI_INSTALL_TEST_${_sp_v}_BASE_URL=$_sp_u: test mode downloads only from http://127.0.0.1[:port] or http://localhost[:port]"; fi
			fi
		done
		for _sp_v in REPO VERDANDI_REPO; do
			eval "_sp_u=\${EITRI_INSTALL_TEST_${_sp_v}_URL-}"
			case $_sp_u in
			'' | /*) ;;
			*) die "EITRI_INSTALL_TEST_${_sp_v}_URL=$_sp_u: test mode clones only from an absolute local path" ;;
			esac
		done
	fi
}

# loopback_http URL: sets LH=1 when URL is plain http to exactly 127.0.0.1 or localhost, with an
# optional numeric port and no user part, else LH=0 (check_base_url's rule for --base-url).
loopback_http() {
	LH=0
	case $1 in http://*) ;; *) return 0 ;; esac
	_lh_auth=${1#http://}
	_lh_auth=${_lh_auth%%/*}
	case $_lh_auth in
	127.0.0.1 | localhost) LH=1 ;;
	127.0.0.1:?* | localhost:?*)
		case ${_lh_auth#*:} in
		*[!0-9]*) ;;
		*) LH=1 ;;
		esac
		;;
	esac
}

# Refuse XDG_DATA_HOME outside HOME before writing under NV_DATA: under_home refuses every
# uninstall target as a batch if any lies outside HOME, leaving even in-HOME parts uninstallable.
# Uninstall remains exempt so it can name the paths requiring manual removal for such an install.
check_data_home() {
	_cdh_ok=$(under_home "$NV_DATA")
	if [ "$_cdh_ok" != 1 ]; then
		die "XDG_DATA_HOME resolves to $NV_DATA, which is not inside $NV_HOME: Eitri writes its desktop entry, icon, licences, private nvim and sidecar there, and --uninstall can only ever remove paths inside \$HOME. Set XDG_DATA_HOME to a directory under \$HOME (or unset it) and re-run"
	fi
}

# ---------------------------------------------------------------------------------------------
# Small pure helpers. Each prints its answer; none changes anything.

# kv_get FILE KEY: the value of the first KEY=value line. KEY=value files (RELEASE, BUILD,
# os-release) are read by a case loop, never sourced or eval'd.
kv_get() {
	_kv_text=$(cat -- "$1") || die "cannot open $1"
	kv_value "$_kv_text" "$2"
}

# kv_value TEXT KEY: kv_get, for text already read.
kv_value() {
	_kv_out=
	_kv_ifs=$IFS
	IFS=$NV_NL
	set -f
	for _kv_line in $1; do
		case $_kv_line in
		"$2="*)
			_kv_out=${_kv_line#*=}
			break
			;;
		esac
	done
	set +f
	IFS=$_kv_ifs
	printf '%s\n' "$_kv_out"
}

# dotted_cmp A B: -1, 0 or 1, comparing up to three numeric dot-separated fields (missing = 0).
dotted_cmp() {
	_dc_a=$1.0.0.0 _dc_b=$2.0.0.0
	for _dc_i in 1 2 3; do
		_dc_x=${_dc_a%%.*} _dc_y=${_dc_b%%.*}
		_dc_a=${_dc_a#*.} _dc_b=${_dc_b#*.}
		case $_dc_x in '' | *[!0-9]*) _dc_x=0 ;; esac
		case $_dc_y in '' | *[!0-9]*) _dc_y=0 ;; esac
		if [ "$_dc_x" -lt "$_dc_y" ]; then
			printf '%s\n' -1
			return 0
		fi
		if [ "$_dc_x" -gt "$_dc_y" ]; then
			printf '%s\n' 1
			return 0
		fi
	done
	printf '%s\n' 0
}

# semver_cmp A B: -1, 0 or 1 for two release versions (X.Y.Z or X.Y.Z-rc.N, already validated):
# the numbers first, then a final release sorts above any of its own release candidates.
semver_cmp() {
	_sc_c=$(dotted_cmp "${1%%-*}" "${2%%-*}")
	if [ "$_sc_c" != 0 ]; then
		printf '%s\n' "$_sc_c"
		return 0
	fi
	_sc_a='' _sc_b=''
	case $1 in *-rc.*) _sc_a=${1##*-rc.} ;; esac
	case $2 in *-rc.*) _sc_b=${2##*-rc.} ;; esac
	if [ -z "$_sc_a" ] && [ -z "$_sc_b" ]; then
		printf '%s\n' 0
	elif [ -z "$_sc_a" ]; then
		printf '%s\n' 1
	elif [ -z "$_sc_b" ]; then
		printf '%s\n' -1
	else
		dotted_cmp "$_sc_a" "$_sc_b"
	fi
}

# first7 STRING
first7() {
	printf '%s\n' "${1%"${1#???????}"}"
}

# file_sha256 FILE. The file goes in on a redirect, not as an argument: GNU sha256sum escapes a name
# holding a backslash or newline and then prefixes its output line with `\`, which would corrupt the
# hash parsed from it (a HOME like /home/a\b is enough).
file_sha256() {
	if command -v sha256sum >/dev/null 2>&1; then
		_fs_out=$(sha256sum <"$1") || die "sha256sum could not open $1"
	elif command -v shasum >/dev/null 2>&1; then
		_fs_out=$(shasum -a 256 <"$1") || die "shasum could not open $1"
	else
		die "neither sha256sum nor shasum was found: install coreutils (or perl's shasum) and re-run"
	fi
	printf '%s\n' "${_fs_out%% *}"
}

# text_sha256 TEXT: file_sha256 for text held in memory, byte for byte (a dry run hashes the
# RELEASE it fetched without writing it anywhere).
text_sha256() {
	if command -v sha256sum >/dev/null 2>&1; then
		_ts_out=$(printf '%s' "$1" | sha256sum) || die "sha256sum failed"
	elif command -v shasum >/dev/null 2>&1; then
		_ts_out=$(printf '%s' "$1" | shasum -a 256) || die "shasum failed"
	else
		die "neither sha256sum nor shasum was found: install coreutils (or perl's shasum) and re-run"
	fi
	printf '%s\n' "${_ts_out%% *}"
}

# rev_of_release FILE REMEDY sets RR_REV7 to the 7-hex prefix of a bare lowercase-hex VERDANDI_REV,
# or nothing if FILE is absent. An unreadable file or missing/malformed field stops the run:
# without knowing which sidecar an install uses, pruning or uninstalling one is unsafe.
# REMEDY tells the user how to repair it. Release files use bare values, so any other spelling
# is damaged. Set a variable and call as a plain statement (rule 2): nested $(...) under bash
# can lose kv_get's fatal error and let uninstall remove that install's sidecar.
rev_of_release() {
	RR_REV7=
	if [ ! -f "$1" ]; then return 0; fi
	if [ ! -r "$1" ]; then
		die "$1 cannot be read, so which sidecar its install uses cannot be told, and none is removed on a guess: make it readable, or $2, then re-run"
	fi
	_rr=$(kv_get "$1" VERDANDI_REV)
	if ! printf '%s\n' "$_rr" | grep -Eqx '[0-9a-f]{7,40}'; then
		die "$1 names no valid VERDANDI_REV (it reads [$_rr]), so which sidecar its install uses cannot be told, and none is removed on a guess: $2, then re-run"
	fi
	RR_REV7=$(first7 "$_rr")
}

# ---------------------------------------------------------------------------------------------
# Reading a RELEASE

# parse_release FILE: sets REL_VERSION, REL_VERDANDI_REV, REL_REV7 and the sidecar-build fields
# below; dies naming the file when any of them is missing or malformed.
parse_release() {
	_pl_text=$(cat -- "$1") || die "cannot open $1"
	parse_release_text "$_pl_text" "$1"
}

# parse_release_text TEXT NAME: parse_release, for a RELEASE already read; NAME is what messages
# call it. Also sets the fields the sidecar build reads: REL_VERDANDI_SOURCE
# (checked against REL_REV7's own prefix, since a mismatch there means the release was assembled
# wrong), REL_VERDANDI_SOURCE_SHA256, REL_NODE_VERSION, REL_NODE_SHA256_LINUX_X64 and
# REL_NODE_SHA256_LINUX_ARM64 -- copied from packaging/pins.env into every real RELEASE (release.sh) -- and the
# fields of the from-source and nvim paths: REL_EITRI_COMMIT, REL_NEOVIDE_FORK_COMMIT (the
# from-source clone check reads the first), REL_NVIM_VERSION and REL_NVIM_SHA256_LINUX_X86_64 (the
# nvim offer). A RELEASE missing any of them is as damaged as one missing VERDANDI_REV.
parse_release_text() {
	REL_VERSION=$(kv_value "$1" EITRI_VERSION)
	REL_EITRI_COMMIT=$(kv_value "$1" EITRI_COMMIT)
	REL_NEOVIDE_FORK_COMMIT=$(kv_value "$1" NEOVIDE_FORK_COMMIT)
	REL_VERDANDI_REV=$(kv_value "$1" VERDANDI_REV)
	if ! printf '%s\n' "$REL_VERSION" | grep -Eqx "$NV_VERSION_ERE"; then
		die "$2 names no valid EITRI_VERSION: the release is damaged. Report it at $NV_ISSUES"
	fi
	if ! printf '%s\n' "$REL_EITRI_COMMIT" | grep -Eqx '[0-9a-f]{40}'; then
		die "$2 names no valid EITRI_COMMIT (40 hex): the release is damaged. Report it at $NV_ISSUES"
	fi
	if ! printf '%s\n' "$REL_NEOVIDE_FORK_COMMIT" | grep -Eqx '[0-9a-f]{40}'; then
		die "$2 names no valid NEOVIDE_FORK_COMMIT (40 hex): the release is damaged. Report it at $NV_ISSUES"
	fi
	if ! printf '%s\n' "$REL_VERDANDI_REV" | grep -Eqx '[0-9a-f]{40}'; then
		die "$2 names no valid VERDANDI_REV (40 hex): the release is damaged. Report it at $NV_ISSUES"
	fi
	REL_REV7=$(first7 "$REL_VERDANDI_REV")
	REL_VERDANDI_SOURCE=$(kv_value "$1" VERDANDI_SOURCE)
	if [ "$REL_VERDANDI_SOURCE" != "verdandi-$REL_REV7-source.tar.gz" ]; then
		die "$2 names VERDANDI_SOURCE=$REL_VERDANDI_SOURCE, not verdandi-$REL_REV7-source.tar.gz (its own VERDANDI_REV's prefix): the release is damaged. Report it at $NV_ISSUES"
	fi
	REL_VERDANDI_SOURCE_SHA256=$(kv_value "$1" VERDANDI_SOURCE_SHA256)
	if ! printf '%s\n' "$REL_VERDANDI_SOURCE_SHA256" | grep -Eqx '[0-9a-f]{64}'; then
		die "$2 names no valid VERDANDI_SOURCE_SHA256 (64 hex): the release is damaged. Report it at $NV_ISSUES"
	fi
	REL_NODE_VERSION=$(kv_value "$1" NODE_VERSION)
	if ! printf '%s\n' "$REL_NODE_VERSION" | grep -Eqx 'v[0-9]+\.[0-9]+\.[0-9]+'; then
		die "$2 names no valid NODE_VERSION (vX.Y.Z): the release is damaged. Report it at $NV_ISSUES"
	fi
	REL_NODE_SHA256_LINUX_X64=$(kv_value "$1" NODE_SHA256_linux_x64)
	if ! printf '%s\n' "$REL_NODE_SHA256_LINUX_X64" | grep -Eqx '[0-9a-f]{64}'; then
		die "$2 names no valid NODE_SHA256_linux_x64 (64 hex): the release is damaged. Report it at $NV_ISSUES"
	fi
	REL_NODE_SHA256_LINUX_ARM64=$(kv_value "$1" NODE_SHA256_linux_arm64)
	if ! printf '%s\n' "$REL_NODE_SHA256_LINUX_ARM64" | grep -Eqx '[0-9a-f]{64}'; then
		die "$2 names no valid NODE_SHA256_linux_arm64 (64 hex): the release is damaged. Report it at $NV_ISSUES"
	fi
	REL_NVIM_VERSION=$(kv_value "$1" NVIM_VERSION)
	if ! printf '%s\n' "$REL_NVIM_VERSION" | grep -Eqx '[0-9]+\.[0-9]+\.[0-9]+'; then
		die "$2 names no valid NVIM_VERSION (X.Y.Z, no leading v): the release is damaged. Report it at $NV_ISSUES"
	fi
	REL_NVIM_SHA256_LINUX_X86_64=$(kv_value "$1" NVIM_SHA256_linux_x86_64)
	if ! printf '%s\n' "$REL_NVIM_SHA256_LINUX_X86_64" | grep -Eqx '[0-9a-f]{64}'; then
		die "$2 names no valid NVIM_SHA256_linux_x86_64 (64 hex): the release is damaged. Report it at $NV_ISSUES"
	fi
	# Optional and absent from downloaded RELEASE files. synthesize_release_from_checkout marks
	# its git-archive hash so a separate setup run can explain why it cannot fetch an equivalent
	# release asset, rather than report an expected byte difference as checksum tampering.
	REL_BUILT_FROM_CHECKOUT=$(kv_value "$1" EITRI_BUILT_FROM_CHECKOUT)
	# Additive fields: old installers ignore them; builds validate the complete pin before npm.
	REL_GRPC_TOOLS_PRESENT=0
	if printf '%s\n' "$1" | grep -Eq '^GRPC_TOOLS_(VERSION|SHA256_linux_(x64|arm64))='; then
		REL_GRPC_TOOLS_PRESENT=1
	fi
	REL_GRPC_TOOLS_VERSION=$(kv_value "$1" GRPC_TOOLS_VERSION)
	REL_GRPC_TOOLS_SHA256_LINUX_X64=$(kv_value "$1" GRPC_TOOLS_SHA256_linux_x64)
	REL_GRPC_TOOLS_SHA256_LINUX_ARM64=$(kv_value "$1" GRPC_TOOLS_SHA256_linux_arm64)
	# Optional too: what this tree expects of a sidecar. A RELEASE from before sidecar updates has
	# none and still installs; only the commands that take an update require them
	# (require_sidecar_contract).
	REL_SIDECAR_VERSION=$(kv_value "$1" SIDECAR_VERSION)
	REL_SIDECAR_PROTOCOL_MAJOR=$(kv_value "$1" SIDECAR_PROTOCOL_MAJOR)
	REL_SIDECAR_REQUIRED_CAPABILITIES=$(kv_value "$1" SIDECAR_REQUIRED_CAPABILITIES)
}

# ---------------------------------------------------------------------------------------------
# System checks. Each dies with the command to run, never runs it.

read_os_release() {
	OS_ID='' OS_ID_LIKE='' OS_VERSION_ID='' OS_NAME=''
	if [ -f "$NV_OS_RELEASE" ]; then
		OS_ID=$(kv_get "$NV_OS_RELEASE" ID)
		OS_ID_LIKE=$(kv_get "$NV_OS_RELEASE" ID_LIKE)
		OS_VERSION_ID=$(kv_get "$NV_OS_RELEASE" VERSION_ID)
		OS_NAME=$(kv_get "$NV_OS_RELEASE" NAME)
	fi
	# os-release values may be quoted; they are only ever compared and printed.
	OS_ID=$(printf '%s' "$OS_ID" | tr -d '"'"'")
	OS_ID_LIKE=$(printf '%s' "$OS_ID_LIKE" | tr -d '"'"'")
	OS_VERSION_ID=$(printf '%s' "$OS_VERSION_ID" | tr -d '"'"'")
	OS_NAME=$(printf '%s' "$OS_NAME" | tr -d '"'"'")
	OS_FAMILY=other
	for _or_w in $OS_ID $OS_ID_LIKE; do
		case $_or_w in
		debian | ubuntu) OS_FAMILY=debian ;;
		fedora | rhel | centos) OS_FAMILY=fedora ;;
		arch) OS_FAMILY=arch ;;
		opensuse* | suse | sles) OS_FAMILY=opensuse ;;
		*) continue ;;
		esac
		break
	done
}

# runtime_hint: how to get GTK 4 and WebKitGTK 6.0 on this distribution.
runtime_hint() {
	case $OS_FAMILY in
	debian) printf '%s\n' "install them with: sudo apt install libgtk-4-1 libwebkitgtk-6.0-4" ;;
	fedora) printf '%s\n' "install them with: sudo dnf install gtk4 webkitgtk6.0 (on RHEL 10, from EPEL)" ;;
	arch) printf '%s\n' "install them with: sudo pacman -S gtk4 webkitgtk-6.0 (or use the AUR package eitri-bin)" ;;
	opensuse) printf '%s\n' "install them with: sudo zypper install gtk4 libwebkitgtk-6_0-4 (package name unverified)" ;;
	*) printf '%s\n' "install GTK 4 (4.14 or newer, libgtk-4.so.1) and WebKitGTK 6.0 (libwebkitgtk-6.0.so.4) from your distribution" ;;
	esac
}

# gtk_too_old_hint VERSION: which release of this distribution has GTK 4.14.
gtk_too_old_hint() {
	case $OS_ID in
	ubuntu) printf '%s\n' "Ubuntu ${OS_VERSION_ID:-?} ships GTK $1; Ubuntu 24.04 or newer has 4.14: upgrade to it" ;;
	debian) printf '%s\n' "Debian ${OS_VERSION_ID:-?} ships GTK $1; Debian 13 (trixie) or newer has it: upgrade to it" ;;
	fedora) printf '%s\n' "Fedora 40 or newer has GTK 4.14: upgrade to it" ;;
	*) printf '%s\n' "a distribution release with GTK 4.14 or newer is needed (for example Ubuntu 24.04, Debian 13, Fedora 40)" ;;
	esac
}

check_root() {
	_cr_uid=$(id -u) || die "id -u failed"
	if [ "$_cr_uid" != 0 ]; then return 0; fi
	if [ "$OPT_ALLOW_ROOT" = 1 ]; then
		warn "running as root (--allow-root): everything is installed into root's own home, $NV_HOME"
		return 0
	fi
	die "do not run this as root: run it as the user who will run Eitri (the sidecar is built per user into \$XDG_DATA_HOME, and as root npm would run every dependency's install script as root). In a container, pass --allow-root."
}

check_platform() {
	_cp=$(uname -sm) || die "uname failed"
	case $_cp in
	'Linux x86_64') ;;
	# Only an x86_64 Skia archive is pinned, so --from-source cannot rescue another architecture;
	# refuse it here rather than direct the user to a build that must fail at the Skia step.
	*) die "prebuilt Eitri releases are for x86_64 Linux only, and this is $_cp. v1 builds x86_64 only: there is no working install path for another architecture yet" ;;
	esac
}

check_glibc() {
	_cg=$(getconf GNU_LIBC_VERSION 2>/dev/null) || _cg=
	case $_cg in
	glibc\ *) _cg=${_cg#glibc } ;;
	*)
		_cg=$(ldd --version 2>&1 | head -n 1) || _cg=
		case $_cg in
		*GNU*libc* | *GLIBC*) _cg=${_cg##* } ;;
		*) die "Eitri's prebuilt binaries need glibc $NV_GLIBC_FLOOR or newer, and this system does not appear to use glibc: build from source instead (sh install.sh --from-source)" ;;
		esac
		;;
	esac
	_cg_c=$(dotted_cmp "$_cg" "$NV_GLIBC_FLOOR")
	if [ "$_cg_c" = -1 ]; then
		die "Eitri's prebuilt binaries need glibc $NV_GLIBC_FLOOR or newer (they are built on Ubuntu 24.04), and this system has $_cg. Releases that have it: Ubuntu 24.04, Debian 13, Fedora 40, RHEL 10, Arch. Upgrade, or build from source (sh install.sh --from-source)."
	fi
}

# ldconfig_output: `ldconfig -p`, or in test mode the fake outputs the harness wrote.
ldconfig_output() {
	if [ "$NV_TEST_MODE" = 1 ] && [ -n "$NV_TEST_LIBDIRS" ]; then
		_lo_ifs=$IFS
		IFS=:
		set -f
		for _lo_d in $NV_TEST_LIBDIRS; do
			if [ -f "$_lo_d/ldconfig-p.txt" ]; then cat -- "$_lo_d/ldconfig-p.txt" || die "cannot open $_lo_d/ldconfig-p.txt"; fi
		done
		set +f
		IFS=$_lo_ifs
		return 0
	fi
	for _lo_c in ldconfig /sbin/ldconfig /usr/sbin/ldconfig; do
		if command -v "$_lo_c" >/dev/null 2>&1; then
			"$_lo_c" -p 2>/dev/null || :
			return 0
		fi
	done
}

# lib_dirs_for SONAME sets LD_DIRS, one directory per line: ldconfig -p's architecture-specific
# entries first, then the usual library directories. Set a variable and call as a plain
# statement because ldconfig_output can die (rule 2). Keep the AArch64 marker and directories
# correct for a future pinned aarch64 Skia archive; platform checks currently prevent reaching
# check_gtk_webkit on aarch64 through either prebuilt or from-source installs.
lib_dirs_for() {
	_ld_arch=$(uname -m) || die "uname failed"
	case $_ld_arch in
	aarch64 | arm64)
		_ld_marker=AArch64
		_ld_deb_dir=aarch64-linux-gnu
		;;
	*)
		_ld_marker=x86-64
		_ld_deb_dir=x86_64-linux-gnu
		;;
	esac
	_ld_ldc=$(ldconfig_output)
	LD_DIRS=$(printf '%s\n' "$_ld_ldc" | NV_SO=$1 NV_MARK=$_ld_marker awk \
		'$1 == ENVIRON["NV_SO"] && index($0, ENVIRON["NV_MARK"]) { p = $NF; sub(/\/[^\/]*$/, "", p); print p }')
	if [ "$NV_TEST_MODE" = 1 ] && [ -n "$NV_TEST_LIBDIRS" ]; then
		_ld_list=$NV_TEST_LIBDIRS
	else
		_ld_list=/usr/lib/$_ld_deb_dir:/usr/lib64:/usr/lib:/lib/$_ld_deb_dir:/lib64:/lib
	fi
	_ld_ifs=$IFS
	IFS=:
	set -f
	for _ld_d in $_ld_list; do
		if [ -e "$_ld_d/$1" ]; then LD_DIRS=${LD_DIRS:+$LD_DIRS$NV_NL}$_ld_d; fi
	done
	set +f
	IFS=$_ld_ifs
}

# gtk_version_in DIR: "4.<minor>.<micro>" from the soname target in DIR (libgtk-4.so.1.1405.5 is
# 4.14.5; verified on Arch: libgtk-4.so.1.2200.5 is 4.22.5), or nothing.
gtk_version_in() {
	_gv_best=
	_gv_bestn=-1
	for _gv_f in "$1"/libgtk-4.so.1.[0-9]*; do
		if [ ! -e "$_gv_f" ]; then continue; fi
		_gv_t=${_gv_f##*/libgtk-4.so.1.}
		_gv_n=${_gv_t%%.*}
		_gv_micro=${_gv_t#*.}
		_gv_micro=${_gv_micro%%.*}
		case $_gv_n in '' | *[!0-9]*) continue ;; esac
		case $_gv_micro in '' | *[!0-9]*) _gv_micro=0 ;; esac
		if [ "$_gv_n" -gt "$_gv_bestn" ]; then
			_gv_bestn=$_gv_n
			_gv_best=4.$((_gv_n / 100)).$_gv_micro
		fi
	done
	printf '%s\n' "$_gv_best"
}

check_gtk_webkit() {
	read_os_release
	lib_dirs_for libgtk-4.so.1
	_gw_dirs=$LD_DIRS
	_gw_ver=
	# Split into this function's own positional parameters, then loop with globbing back on:
	# gtk_version_in globs, and `set -f` would still be in force inside it.
	_gw_ifs=$IFS
	IFS=$NV_NL
	set -f
	# shellcheck disable=SC2086 # split on newlines only, with globbing off
	set -- $_gw_dirs
	set +f
	IFS=$_gw_ifs
	for _gw_d do
		_gw_ver=$(gtk_version_in "$_gw_d")
		if [ -n "$_gw_ver" ]; then break; fi
	done
	_gw_hint=$(runtime_hint)
	if [ -z "$_gw_ver" ]; then
		die "GTK 4 (libgtk-4.so.1) was not found; Eitri needs GTK 4.14 or newer and WebKitGTK 6.0: $_gw_hint"
	fi
	_gw_minor=${_gw_ver#4.}
	_gw_minor=${_gw_minor%%.*}
	if [ "$_gw_minor" -lt "$NV_GTK_FLOOR_MINOR" ]; then
		_gw_old=$(gtk_too_old_hint "$_gw_ver")
		die "this system has GTK $_gw_ver, and Eitri needs GTK 4.$NV_GTK_FLOOR_MINOR or newer: its binaries are built against 4.$NV_GTK_FLOOR_MINOR's API on Ubuntu 24.04. $_gw_old."
	fi
	lib_dirs_for libwebkitgtk-6.0.so.4
	_gw_wk=$LD_DIRS
	if [ -z "$_gw_wk" ]; then
		die "WebKitGTK 6.0 (libwebkitgtk-6.0.so.4) was not found; Eitri's agent panel needs it: $_gw_hint"
	fi
	say "GTK $_gw_ver and WebKitGTK 6.0: ok"
}

# nvim_check: reports which nvim is on PATH and why, and sets NV_NVIM_OK=1 when it
# already satisfies NV_NVIM_FLOOR (0.11, the floor the editor gives the fork). maybe_offer_nvim reads NV_NVIM_OK to
# decide whether the nvim offer runs at all -- an adequate PATH nvim always wins, whatever flag was
# given, so --with-nvim never *replaces* a perfectly good system nvim.
nvim_check() {
	NV_NVIM_OK=0
	if ! command -v nvim >/dev/null 2>&1; then
		warn "nvim was not found on PATH. Eitri needs nvim $NV_NVIM_FLOOR or newer: install it from your distribution or https://github.com/neovim/neovim/releases"
		return 0
	fi
	_rn=$(nvim --version 2>/dev/null | head -n 1) || _rn=
	_rn_v=${_rn#NVIM v}
	_rn_v=${_rn_v%%[!0-9.]*}
	if [ -z "$_rn_v" ]; then
		warn "could not parse nvim's version ($_rn); Eitri needs nvim $NV_NVIM_FLOOR or newer"
		return 0
	fi
	_rn_c=$(dotted_cmp "$_rn_v" "$NV_NVIM_FLOOR")
	if [ "$_rn_c" = -1 ]; then
		warn "nvim $_rn_v is older than $NV_NVIM_FLOOR, which Eitri needs (its editor refuses older ones): install a newer nvim from https://github.com/neovim/neovim/releases"
	else
		say "nvim $_rn_v: ok"
		NV_NVIM_OK=1
	fi
}

# nvim_dist_base: where the pinned nvim release tarball is fetched from -- neovim's own GitHub
# releases, never Eitri's release server. Fixed in production; test mode redirects it to
# a fixture server, and -- deliberately, the same discipline as node_dist_base -- an unset override
# does NOT fall back to the real github.com: a test that forgets to set it must fail fast against a
# refused loopback port, never spend real time downloading a real nvim release.
nvim_dist_base() {
	if [ "$NV_TEST_MODE" = 1 ]; then
		printf '%s\n' "${EITRI_INSTALL_TEST_NVIM_BASE_URL:-http://127.0.0.1:1}"
	else
		printf '%s\n' "$NV_NVIM_DIST_BASE"
	fi
}

# nvim_private_state VERSION: sets NV_NVIM_PRIVATE_PRESENT=1 when
# $XDG_DATA_HOME/eitri/nvim/VERSION/bin/nvim already exists and is executable (the layout contract
# shared with Eitri's own nvim resolver) -- --nvim-only and a re-run of the offer are both
# idempotent against an already-fetched version.
nvim_private_state() {
	NV_NVIM_PRIVATE_PRESENT=0
	NV_NVIM_PRIVATE_BIN=$NV_DATA/eitri/nvim/$1/bin/nvim
	if [ -n "$1" ] && [ -x "$NV_NVIM_PRIVATE_BIN" ]; then NV_NVIM_PRIVATE_PRESENT=1; fi
}

# archive_names_inside TAR-LIST-OPTION ARCHIVE WHAT: refuses ARCHIVE when a member's name is absolute
# or has a `..` component, before anything is unpacked. Every archive here is checked against a hash
# first; this holds whatever the tar in use would make of such a name (GNU tar and bsdtar strip or
# skip them, others need not). `tar -t` prints one name per line, a newline inside a name escaped by
# GNU tar or split by others -- either way a part that is absolute or climbs is still seen. A plain
# statement (rule 2 above).
archive_names_inside() {
	_ani_list=$(tar "$1" "$2") || die "could not unpack $3: tar cannot even list it, so it is not a valid archive. Nothing was changed; report it at $NV_ISSUES"
	_ani_bad=$(printf '%s\n' "$_ani_list" | awk '
		/^\// { print; exit }
		{ n = split($0, part, "/"); for (i = 1; i <= n; i++) if (part[i] == "..") { print; exit } }') || _ani_bad=
	if [ -n "$_ani_bad" ]; then
		die "$3 holds a member named $_ani_bad, outside the directory it is unpacked into: refusing to unpack it. Report it at $NV_ISSUES"
	fi
}

# links_stay_inside DIR WHAT: refuses what was unpacked into DIR (from WHAT) when a symlink in it is
# absolute, climbs out of DIR, or has a `..` after any other component. Its leading `..` components
# are counted from where the link sits, and that count is exact: find does not descend through links,
# so every directory above a link it reports is a real one. A `..` after another component is refused
# outright, because that component may be a link leading anywhere inside DIR (`s -> .` turns
# `l -> s/..` into DIR's parent). Without one, a target only goes up through real directories and then
# down, and every link it goes down through is held to the same rule, so it ends inside DIR. `tar -t`
# shows no link targets, so this reads the links themselves once unpacked; tar itself writes nothing
# through a link it unpacked that could lead out (GNU tar unpacks a link with an absolute or `..`
# target only at the end, bsdtar refuses to write through one), so nothing has reached outside DIR
# yet. Names are passed to the checking shell as arguments, so any character in one is fine. A plain
# statement (rule 2 above).
links_stay_inside() {
	# shellcheck disable=SC2016 # the script is for the inner shell
	_lsi_bad=$(find "$1" -type l -exec sh -c '
		root=$1
		shift
		for l do
			t=$(readlink -- "$l") || { printf "%s\n" "$l"; exit 1; }
			case $t in /*) printf "%s -> %s\n" "${l#"$root"/}" "$t"; exit 1 ;; esac
			r=${l#"$root"/}
			d=0
			while :; do
				case $r in */*) d=$((d + 1)); r=${r#*/} ;; *) break ;; esac
			done
			rest=$t
			down=0
			while [ -n "$rest" ]; do
				c=${rest%%/*}
				case $rest in */*) rest=${rest#*/} ;; *) rest= ;; esac
				case $c in
				"" | .) ;;
				..)
					d=$((d - 1))
					if [ "$down" = 1 ] || [ "$d" -lt 0 ]; then printf "%s -> %s\n" "${l#"$root"/}" "$t"; exit 1; fi
					;;
				*) down=1 ;;
				esac
			done
		done' sh "$1" {} + 2>/dev/null) || _lsi_bad=${_lsi_bad:-"a link that could not be read"}
	if [ -n "$_lsi_bad" ]; then
		die "$2 holds a symlink leading outside the directory it was unpacked into ($_lsi_bad): refusing to use it. Report it at $NV_ISSUES"
	fi
}

# install_nvim_version VERSION SHA256 TOLERANT: download the official
# nvim-linux-x86_64.tar.gz of VERSION, verify it against SHA256 (which the caller always reads off a
# checked RELEASE, never re-derives here), and extract it with --strip-components=1 (the tarball's
# own top directory is nvim-linux-x86_64/) into $XDG_DATA_HOME/eitri/nvim/VERSION/. Never placed on
# PATH, never linked into ~/.local/bin, and nothing named nvim/vim/vi anywhere else is created,
# replaced or removed. TOLERANT=1 (the install path's own best-effort offer) warns and returns on a
# network failure; TOLERANT=0 (--nvim-only, the explicit ask) dies. A plain statement always (rule 2:
# this can die, so it is never called from if/&&/||).
install_nvim_version() {
	_inv_version=$1
	_inv_sha=$2
	_inv_tolerant=$3
	_inv_workdir=$NV_CACHE_NV/nvim-build
	rm -rf -- "$_inv_workdir" || die "cannot remove the stale nvim build directory $_inv_workdir"
	mkdir -p -- "$_inv_workdir" || die "cannot create $_inv_workdir"
	_inv_tar=$_inv_workdir/nvim-linux-x86_64.tar.gz
	curl_get "$(nvim_dist_base)/v$_inv_version/nvim-linux-x86_64.tar.gz" "$_inv_tar.part" "$NV_MAX_DOWNLOAD_BYTES"
	if [ "$CG_OK" != 1 ]; then
		if [ "$_inv_tolerant" = 1 ]; then
			warn "could not download nvim $_inv_version ($CG_ERR): Eitri is installed without a private copy; run \"eitri setup --nvim-only\" once you have network access"
			return 0
		fi
		die "could not download nvim $_inv_version: $CG_ERR"
	fi
	_inv_have=$(file_sha256 "$_inv_tar.part")
	if [ "$_inv_have" != "$_inv_sha" ]; then
		rm -f -- "$_inv_tar.part" || :
		die "checksum mismatch for nvim-linux-x86_64.tar.gz: expected $_inv_sha, got $_inv_have. Refusing to install an unverified nvim; re-run, and report it at $NV_ISSUES if it persists"
	fi
	mv -- "$_inv_tar.part" "$_inv_tar" || die "cannot rename $_inv_tar.part"
	_inv_extract=$_inv_workdir/extract
	mkdir -p -- "$_inv_extract" || die "cannot create $_inv_extract"
	archive_names_inside -tzf "$_inv_tar" nvim-linux-x86_64.tar.gz
	tar -xzf "$_inv_tar" --no-same-owner --strip-components=1 -C "$_inv_extract" || die "could not unpack nvim-linux-x86_64.tar.gz: report it at $NV_ISSUES"
	links_stay_inside "$_inv_extract" nvim-linux-x86_64.tar.gz
	if [ ! -x "$_inv_extract/bin/nvim" ]; then
		die "nvim-linux-x86_64.tar.gz did not extract to bin/nvim: report it at $NV_ISSUES"
	fi
	mkdir -p -- "$NV_DATA/eitri/nvim" || die "cannot create $NV_DATA/eitri/nvim"
	_inv_dest=$NV_DATA/eitri/nvim/$_inv_version
	_inv_tmp=$_inv_dest.tmp.$$
	NV_NVIM_TMP_DEST=$_inv_tmp
	rm -rf -- "$_inv_tmp" || die "cannot remove the stale $_inv_tmp"
	mv -- "$_inv_extract" "$_inv_tmp" || die "cannot move the extracted nvim into place"
	rm -rf -- "$_inv_dest" || die "cannot remove the stale $_inv_dest"
	mv -- "$_inv_tmp" "$_inv_dest" || die "cannot move the extracted nvim into $_inv_dest"
	NV_NVIM_TMP_DEST=
	rm -rf -- "$_inv_workdir" || :
	say "installed nvim $_inv_version into $_inv_dest (never on PATH; Eitri finds it on its own)"
}

# maybe_offer_nvim runs after RELEASE is parsed: unlike nvim_check, it needs the pinned version
# and checksum. Skip with an adequate PATH nvim or --no-nvim. --with-nvim or --yes answers without
# asking; otherwise try /dev/tty, never stdin (which carries the script). Without a tty, report only.
maybe_offer_nvim() {
	if [ "$NV_NVIM_OK" = 1 ]; then return 0; fi
	if [ "$OPT_NVIM" = no ]; then return 0; fi
	# The pinned nvim tarball is x86_64-only. --nvim-only and --from-source check architecture
	# beforehand, but --nvim-offer can reach here on another architecture. Keep this guard so the
	# offer cannot install a wrong-architecture binary that Eitri would prefer over the system nvim.
	_mon_uname=$(uname -m) || _mon_uname=
	if [ "$_mon_uname" != x86_64 ]; then
		say "no prebuilt nvim for $_mon_uname (only x86_64 is offered): install nvim $NV_NVIM_FLOOR or newer yourself"
		return 0
	fi
	# Skip a pinned version already installed privately: Eitri's resolver prefers it to the
	# inadequate PATH nvim, so downloading it again or asking again would break idempotence.
	# A dry run without a readable new RELEASE has no pinned version to inspect.
	if [ -n "$NV_NEW_REV7" ]; then
		nvim_private_state "$REL_NVIM_VERSION"
		if [ "$NV_NVIM_PRIVATE_PRESENT" = 1 ]; then
			say "nvim $REL_NVIM_VERSION: already installed at $NV_NVIM_PRIVATE_BIN"
			return 0
		fi
	fi
	_mon_answer=0
	if [ "$OPT_NVIM" = yes ] || [ "$OPT_YES" = 1 ]; then
		_mon_answer=1
	# Use a real subshell: without a controlling terminal, dash 0.5.12 treats opening /dev/tty
	# as fatal to the current shell, even under elif and with 2>/dev/null. A brace group would
	# end the installer; a subshell confines the failure to the status elif receives.
	# Reproduce with `docker run ... eitri-install-dash` without a controlling terminal.
	elif ( : </dev/tty ) 2>/dev/null; then
		printf 'eitri: fetch the pinned nvim release into XDG_DATA_HOME/eitri/nvim/ (never put on PATH)? [y/N] ' >&2
		_mon_ans=
		read -r _mon_ans </dev/tty || _mon_ans=
		case $_mon_ans in
		y | Y | yes | YES | Yes) _mon_answer=1 ;;
		esac
	else
		say "no terminal to ask on: not fetching a private nvim (pass --with-nvim to fetch it non-interactively, or --no-nvim to silence this)"
	fi
	if [ "$_mon_answer" != 1 ]; then return 0; fi
	# A dry run that could not read the new RELEASE at all (dry_run_new_rev's own early return)
	# leaves REL_NVIM_VERSION unset under `set -eu`; NV_NEW_REV7 is always defined by then (empty or
	# real) and is the same guard ensure_sidecar reads for the identical reason.
	if [ -z "$NV_NEW_REV7" ]; then
		say "which nvim to fetch is not known to this dry run"
		return 0
	fi
	if [ "$OPT_DRY_RUN" = 1 ]; then
		say "would install nvim $REL_NVIM_VERSION into $NV_DATA/eitri/nvim/$REL_NVIM_VERSION"
		return 0
	fi
	install_nvim_version "$REL_NVIM_VERSION" "$REL_NVIM_SHA256_LINUX_X86_64" 1
}

# report_claude: the claude CLI is never installed here, only reported, and compared with the range
# the present sidecar says it supports.
report_claude() {
	if ! command -v claude >/dev/null 2>&1; then
		warn "the claude CLI was not found on PATH; Eitri's agent panel runs it. Get Claude Code at https://code.claude.com/docs/en/quickstart"
		return 0
	fi
	_rc=$(claude --version 2>/dev/null | head -n 1) || _rc=
	_rc_v=${_rc%% *}
	say "claude ${_rc_v:-(version unknown)}"
	if [ -z "${1-}" ] || [ -z "$_rc_v" ]; then return 0; fi
	_rc_range=$("$1" --version 2>/dev/null | sed -n 's/^supported claude code CLI: //p' | head -n 1) || _rc_range=
	_rc_lo='' _rc_hi=''
	for _rc_w in $_rc_range; do
		case $_rc_w in
		'>='*) _rc_lo=${_rc_w#>=} ;;
		'<'*) _rc_hi=${_rc_w#<} ;;
		esac
	done
	_rc_bad=0
	if [ -n "$_rc_lo" ]; then
		_rc_c=$(dotted_cmp "$_rc_v" "$_rc_lo")
		if [ "$_rc_c" = -1 ]; then _rc_bad=1; fi
	fi
	if [ -n "$_rc_hi" ]; then
		_rc_c=$(dotted_cmp "$_rc_v" "$_rc_hi")
		if [ "$_rc_c" != -1 ]; then _rc_bad=1; fi
	fi
	if [ "$_rc_bad" = 1 ]; then
		warn "claude $_rc_v is outside the range this sidecar supports ($_rc_range): update it with \`claude update\`, or install a supported version"
	fi
}

# ---------------------------------------------------------------------------------------------
# Downloads

# check_base_url URL: sets NV_BASE. https only; plain http only for exactly
# 127.0.0.1 or localhost with an optional numeric port, and no `@` anywhere in the authority -- so
# http://localhost@<host>/ and http://127.0.0.1.<host>/ are both refused.
check_base_url() {
	_bu=$1
	while :; do
		case $_bu in
		*/) _bu=${_bu%/} ;;
		*) break ;;
		esac
	done
	case $_bu in
	https://?*) ;;
	http://*)
		_bu_auth=${_bu#http://}
		_bu_auth=${_bu_auth%%/*}
		case $_bu_auth in
		*@*) die "--base-url $1: an http:// URL may not carry a user part ('@'); use https://" ;;
		127.0.0.1 | localhost) ;;
		127.0.0.1:* | localhost:*)
			case ${_bu_auth#*:} in
			'' | *[!0-9]*) die "--base-url $1: plain http is accepted only for 127.0.0.1 or localhost with a numeric port; use https://" ;;
			esac
			;;
		*) die "--base-url $1: plain http is accepted only for http://127.0.0.1[:port] or http://localhost[:port]; use https://" ;;
		esac
		;;
	*) die "--base-url $1: the URL must start with https://" ;;
	esac
	NV_BASE=$_bu
}

# require_curl: every download goes through curl. The design also calls for `wget --https-only` for a
# system without curl, but wget's manual scopes --https-only to recursive downloads, and measured
# (GNU Wget 1.25.0) it follows an https -> http redirect -- and fetches a plain
# http:// URL -- with exit 0. curl's --proto-redir holds every hop to HTTPS, and GitHub serves every
# release asset through a redirect, so wget is not used at all (an Eitri-only deviation from the design).
require_curl() {
	if command -v curl >/dev/null 2>&1; then return 0; fi
	read_os_release
	case $OS_FAMILY in
	debian) _rq=' (sudo apt install curl)' ;;
	fedora) _rq=' (sudo dnf install curl)' ;;
	arch) _rq=' (sudo pacman -S curl)' ;;
	opensuse) _rq=' (sudo zypper install curl)' ;;
	*) _rq= ;;
	esac
	die "curl was not found: this installer downloads only with curl, which it can hold to HTTPS across every redirect (wget cannot be). Install curl$_rq and re-run"
}

# size_within FILE LIMIT sets SW_OK=0, and removes FILE, when it holds more than LIMIT bytes.
# --max-filesize cannot refuse in advance a response that declares no length, so a download is
# measured again once it is on disk. Rule 2: a variable, and a plain statement.
size_within() {
	SW_OK=1
	if [ ! -f "$1" ]; then return 0; fi
	_sw_size=$(wc -c <"$1") || die "cannot measure $1"
	if [ "$_sw_size" -gt "$2" ]; then
		SW_OK=0
		rm -f -- "$1" || :
	fi
}

# net_retryable RC MESSAGE: NR_RETRY=1 when curl's exit status RC (with its MESSAGE) is a failure
# another attempt can cure: the name lookup, connection, TLS handshake or transfer broke or stalled,
# or the server said 408, 429 or 5xx. A 404, a refused redirect, a file over its limit, a certificate
# problem or a local write error is an answer, and is not asked again. Rule 2: a variable.
net_retryable() {
	NR_RETRY=0
	case $1 in
	5 | 6 | 7 | 16 | 18 | 28 | 35 | 52 | 55 | 56 | 92) NR_RETRY=1 ;;
	22)
		case $2 in
		*'returned error: 408'* | *'returned error: 429'* | *'returned error: 5'[0-9][0-9]*) NR_RETRY=1 ;;
		esac
		;;
	esac
}

# net_get URL DEST LIMIT: download URL into the file DEST, or with DEST '-' into NG_TEXT (byte for
# byte), refusing more than LIMIT bytes. HTTPS-only for every non-loopback URL, on every redirect
# too (--proto/--proto-redir); a loopback URL reads no curl config and follows no redirect. An
# attempt the network broke is made again (NV_NET_* above); a file download resumes its partial
# file (curl -C -) and starts over when the server cannot resume it. Every caller checks the whole
# file against a hash or a signature afterwards, so a resumed file that is not the file fails that
# check like any corrupt download. Sets NG_OK=1, or NG_OK=0 with NG_ERR (curl's last message, and
# what was tried when it was the network) and NG_HINT (what the user can change, or empty when the
# server answered). Rule 2: a plain statement; it dies only on a URL no caller may pass.
net_get() {
	NG_OK=0
	NG_ERR=
	NG_HINT=
	NG_TEXT=
	_ng_url=$1
	_ng_dest=$2
	_ng_lim=$3
	case $_ng_url in
	https://*) _ng_lb=0 ;;
	http://*)
		loopback_http "$_ng_url"
		if [ "$LH" != 1 ]; then die "$_ng_url: plain http is only for a loopback test server"; fi
		_ng_lb=1
		;;
	*) die "$_ng_url is not an http(s) URL" ;;
	esac
	_ng_host=${_ng_url#*://}
	_ng_host=${_ng_host%%/*}
	_ng_name=${_ng_url##*/}
	_ng_via=
	_ng_at=$_ng_host
	_ng_n=0
	_ng_dry=0
	_ng_resume=0
	_ng_resumed=0
	_ng_wait=$NV_NET_DELAY
	_ng_t0=$(date +%s) || _ng_t0=0
	# A partial file a run before this one left is not this run's progress.
	if [ "$_ng_dest" != - ]; then rm -f -- "$_ng_dest" || die "cannot remove $_ng_dest"; fi
	while :; do
		_ng_n=$((_ng_n + 1))
		_ng_before=0
		_ng_max=$_ng_lim
		if [ "$_ng_resume" = 1 ]; then
			_ng_before=$(wc -c <"$_ng_dest") || _ng_before=0
			# A resumed attempt may add only what the limit leaves: --max-filesize sees one response.
			if [ "$_ng_before" -ge "$_ng_lim" ]; then
				rm -f -- "$_ng_dest" || :
				NG_ERR="it reached the $_ng_lim bytes it can be over $((_ng_n - 1)) attempts: refusing it"
				return 0
			fi
			_ng_max=$((_ng_lim - _ng_before))
			set -- -C -
		else
			set --
		fi
		# For a file, one capture holds curl's own messages and the address and HTTP status it ended
		# at; for text, the capture is the body alone (curl's messages, and any a user's curl config
		# asks for, go to stderr as they are). The exit status is printed only when curl failed, so
		# the substitution itself always succeeds. The x keeps the text's own trailing newlines.
		if [ "$_ng_dest" = - ]; then
			if [ "$_ng_lb" = 1 ]; then
				_ng_out=$(
					curl -q --max-redirs 0 --proto '=http' --connect-timeout "$NV_NET_CONNECT" --speed-limit 1024 --speed-time "$NV_NET_STALL" --max-filesize "$_ng_lim" -fsS -o - -- "$_ng_url" || printf '\nNV-RC %s' "$?"
					printf x
				)
			else
				_ng_out=$(
					curl --proto '=https' --proto-redir '=https' --tlsv1.2 --connect-timeout "$NV_NET_CONNECT" --speed-limit 1024 --speed-time "$NV_NET_STALL" --max-filesize "$_ng_lim" -fsSL -o - -- "$_ng_url" || printf '\nNV-RC %s' "$?"
					printf x
				)
			fi
		elif [ "$_ng_lb" = 1 ]; then
			_ng_out=$(
				curl -q --max-redirs 0 --proto '=http' --connect-timeout "$NV_NET_CONNECT" --speed-limit 1024 --speed-time "$NV_NET_STALL" --max-filesize "$_ng_max" -fsS -w 'NV-URL %{url_effective}\nNV-CODE %{http_code}\n' "$@" -o "$_ng_dest" -- "$_ng_url" 2>&1 || printf '\nNV-RC %s' "$?"
				printf x
			)
		else
			_ng_out=$(
				curl --proto '=https' --proto-redir '=https' --tlsv1.2 --connect-timeout "$NV_NET_CONNECT" --speed-limit 1024 --speed-time "$NV_NET_STALL" --max-filesize "$_ng_max" -fsSL -w 'NV-URL %{url_effective}\nNV-CODE %{http_code}\n' "$@" -o "$_ng_dest" -- "$_ng_url" 2>&1 || printf '\nNV-RC %s' "$?"
				printf x
			)
		fi
		_ng_out=${_ng_out%x}
		_ng_rc=0
		case $_ng_out in
		*"${NV_NL}NV-RC "*)
			_ng_rc=${_ng_out##*"${NV_NL}NV-RC "}
			# The marker is printed only for a failure, so "NV-RC 0" at the end is the body's own text.
			if [ "$_ng_rc" = 0 ]; then
				_ng_rc=0
			else
				_ng_out=${_ng_out%"${NV_NL}NV-RC "*}
			fi
			;;
		esac
		_ng_code=
		case $_ng_out in
		*'NV-CODE '*)
			_ng_code=${_ng_out##*NV-CODE }
			_ng_code=${_ng_code%%"$NV_NL"*}
			;;
		esac
		# curl takes a 416 to a resumption as the file being complete and exits 0, with the partial
		# file as it was: when that file is not the whole one, the check after would refuse it, so the
		# file is downloaded again from the start instead.
		if [ "$_ng_rc" = 0 ] && [ "$_ng_resume" = 1 ] && [ "$_ng_code" = 416 ]; then
			_ng_rc=416
		fi
		if [ "$_ng_rc" = 0 ]; then
			if [ "$_ng_dest" = - ]; then NG_TEXT=$_ng_out; fi
			NG_OK=1
			return 0
		fi
		_ng_msg=
		case $_ng_out in
		*'curl: ('*)
			_ng_msg="curl: (${_ng_out##*curl: (}"
			_ng_msg=${_ng_msg%%"$NV_NL"*}
			;;
		esac
		if [ "$_ng_rc" = 416 ]; then
			_ng_msg="the server answered 416 to the resumption"
		elif [ -z "$_ng_msg" ] && [ "$_ng_dest" = - ]; then
			_ng_msg="curl exited with status $_ng_rc (its message is above)"
		elif [ -z "$_ng_msg" ]; then
			_ng_msg="curl exited with status $_ng_rc"
		fi
		# The host this attempt ended at: a redirect seen once does not mean every later failure was there.
		_ng_at=$_ng_host
		case $_ng_out in
		*'NV-URL '*)
			_ng_eff=${_ng_out##*NV-URL }
			_ng_eff=${_ng_eff%%"$NV_NL"*}
			_ng_eff=${_ng_eff#*://}
			_ng_eff=${_ng_eff%%/*}
			if [ -n "$_ng_eff" ]; then _ng_at=$_ng_eff; fi
			if [ -n "$_ng_eff" ] && [ "$_ng_eff" != "$_ng_host" ]; then _ng_via=$_ng_eff; fi
			;;
		esac
		# A resumed attempt the server cannot serve (no ranges, or the range is past the end): start
		# the file again, as an attempt of its own.
		if [ "$_ng_resume" = 1 ]; then
			case $_ng_rc:$_ng_msg in
			33:* | 416:* | 22:*'returned error: 416'*)
				rm -f -- "$_ng_dest" || :
				_ng_resume=0
				if [ "$_ng_n" -ge "$NV_NET_MAX_TRIES" ]; then break; fi
				printf 'eitri: %s from %s: the server cannot resume it (%s); downloading it again from the start\n' "$_ng_name" "$_ng_host" "$_ng_msg" >&2
				continue
				;;
			esac
		fi
		net_retryable "$_ng_rc" "$_ng_msg"
		if [ "$NR_RETRY" != 1 ]; then
			NG_ERR=$_ng_msg
			return 0
		fi
		_ng_after=0
		if [ "$_ng_dest" != - ] && [ -f "$_ng_dest" ]; then _ng_after=$(wc -c <"$_ng_dest") || _ng_after=0; fi
		# Each attempt is bounded by --max-filesize, but resumed attempts add up: the file as a whole
		# is held to the limit after every one.
		if [ "$_ng_after" -gt "$_ng_lim" ]; then
			rm -f -- "$_ng_dest" || :
			NG_ERR="it grew past the $_ng_lim bytes it can be over $_ng_n attempts: refusing it"
			return 0
		fi
		if [ "$_ng_after" -gt "$_ng_before" ]; then
			_ng_dry=0
			_ng_wait=$NV_NET_DELAY
		else
			_ng_dry=$((_ng_dry + 1))
		fi
		if [ "$_ng_dry" -ge "$NV_NET_TRIES" ] || [ "$_ng_n" -ge "$NV_NET_MAX_TRIES" ]; then break; fi
		_ng_resume=0
		_ng_from=
		if [ "$_ng_after" -gt 0 ]; then
			_ng_resume=1
			_ng_resumed=1
			_ng_from=", resuming at $((_ng_after / 1024)) KiB"
		fi
		printf 'eitri: %s from %s: %s; trying again in %s s (attempt %s)%s\n' "$_ng_name" "$_ng_at" "$_ng_msg" "$_ng_wait" "$((_ng_n + 1))" "$_ng_from" >&2
		sleep "$_ng_wait" || :
		_ng_wait=$((_ng_wait * 2))
		if [ "$_ng_wait" -gt $((NV_NET_DELAY * 8)) ]; then _ng_wait=$((NV_NET_DELAY * 8)); fi
	done
	_ng_t1=$(date +%s) || _ng_t1=$_ng_t0
	NG_ERR="$_ng_msg; $_ng_n attempts over $((_ng_t1 - _ng_t0)) s to $_ng_host"
	if [ -n "$_ng_via" ]; then NG_ERR="$NG_ERR (redirected to $_ng_via)"; fi
	if [ "$_ng_resumed" = 1 ]; then NG_ERR="$NG_ERR, resuming the partial file"; fi
	if [ "$_ng_lb" = 1 ]; then
		NG_HINT="; check that the server at $_ng_host is running"
		return 0
	fi
	NG_HINT=". If this network reaches $_ng_at only through a proxy, set https_proxy (for example https_proxy=http://127.0.0.1:7890) and re-run"
	case $_ng_url in
	"${NV_BASE-}"/*)
		if [ -n "${NV_BASE-}" ]; then NG_HINT="$NG_HINT; --base-url URL downloads the release files from a mirror of the release page instead"; fi
		;;
	esac
}

# fetch URL DEST WHAT LIMIT: download URL to DEST ('-' = stdout, byte for byte) through net_get, and
# die naming WHAT when that fails.
fetch() {
	net_get "$1" "$2" "$4"
	if [ "$NG_OK" != 1 ]; then die "could not download $3 from $1: $NG_ERR$NG_HINT"; fi
	if [ "$2" = - ]; then
		printf '%s' "$NG_TEXT"
		return 0
	fi
	size_within "$2" "$4"
	if [ "$SW_OK" != 1 ]; then die "$3 from $1 is more than the $4 bytes it can be: refusing it"; fi
}

# sums_hash NAME: the hash of the unique SHA256SUMS line whose second field equals NAME.
# Do not run sha256sum -c over the whole list: it fails on files not downloaded.
sums_hash() {
	_sh_out=$(printf '%s\n' "$NV_SUMS_TEXT" | NV_F=$1 awk '
		$2 == ENVIRON["NV_F"] { n++; h = $1 }
		END { if (n == 1) print h; else print "COUNT " n + 0 }')
	case $_sh_out in
	'COUNT 0') die "SHA256SUMS does not list $1, so it cannot be checked: refusing to install it. Report it at $NV_ISSUES" ;;
	COUNT*) die "SHA256SUMS lists $1 ${_sh_out#COUNT } times, and exactly one line is required: refusing to install it. Report it at $NV_ISSUES" ;;
	esac
	if ! printf '%s\n' "$_sh_out" | grep -Eqx '[0-9a-f]{64}'; then
		die "SHA256SUMS's line for $1 carries no valid sha256: refusing to install it. Report it at $NV_ISSUES"
	fi
	printf '%s\n' "$_sh_out"
}

# tarball_version: the version named by the one tarball line of SHA256SUMS. The
# version comes from here, not from any API call.
tarball_version() {
	_tv=$(printf '%s\n' "$NV_SUMS_TEXT" | grep -E "^[0-9a-f]{64}  eitri-$NV_VERSION_ERE-x86_64-linux\\.tar\\.gz\$") || _tv=
	case $_tv in
	'') die "SHA256SUMS lists no eitri-<version>-x86_64-linux.tar.gz: this is not an Eitri release (check --base-url or --version)" ;;
	*"$NV_NL"*) die "SHA256SUMS lists more than one Eitri tarball, so which version it is cannot be told: refusing. Report it at $NV_ISSUES" ;;
	esac
	_tv=${_tv#*  eitri-}
	printf '%s\n' "${_tv%-x86_64-linux.tar.gz}"
}

# verify_file PATH NAME: PATH's sha256 must equal SHA256SUMS's line for NAME.
verify_file() {
	_vf_want=$(sums_hash "$2")
	_vf_have=$(file_sha256 "$1")
	if [ "$_vf_want" != "$_vf_have" ]; then
		rm -f -- "$1" || :
		die "checksum mismatch for $2: SHA256SUMS says $_vf_want, the download is $_vf_have. The file is corrupt or was altered, so nothing was installed; re-run later, and report it at $NV_ISSUES if it persists"
	fi
}

# download_verified NAME LIMIT: fetch NAME from the release into NAME.part, check it, and only then
# rename it. Nothing is unpacked before this has passed.
download_verified() {
	fetch "$NV_REL_URL/$1" "$NV_DL/$1.part" "$1" "$2"
	verify_file "$NV_DL/$1.part" "$1"
	mv -- "$NV_DL/$1.part" "$NV_DL/$1" || die "cannot rename $NV_DL/$1.part"
}

# curl_get URL DEST LIMIT: like fetch, but reports failure through CG_OK/CG_ERR instead of dying (rule 2:
# a predicate reports through a variable), because the sidecar build's own first download (Node) is
# the one place in this installer where "the network is not reachable" is not fatal (a
# release with no sidecar still installs; the panel says how to get one). The same net_get, so the
# same retries, resumption and HTTPS rules as fetch.
curl_get() {
	CG_OK=0
	CG_ERR=
	net_get "$1" "$2" "$3"
	if [ "$NG_OK" != 1 ]; then
		CG_ERR=$NG_ERR$NG_HINT
		return 0
	fi
	CG_OK=1
	size_within "$2" "$3"
	if [ "$SW_OK" != 1 ]; then
		CG_OK=0
		CG_ERR="it is more than the $3 bytes it can be"
	fi
}

# node_arch: sets NV_NODE_ARCH from uname -m, matching how packaging/pins.env and buildBinary.mjs
# name the two Node platforms Eitri pins (x64/arm64, not uname's own x86_64/aarch64).
node_arch() {
	NV_NODE_ARCH=
	_na=$(uname -m) || die "uname failed"
	case $_na in
	x86_64) NV_NODE_ARCH=x64 ;;
	aarch64 | arm64) NV_NODE_ARCH=arm64 ;;
	*) die "no pinned Node build for $_na: the sidecar can only be built on x86_64 or aarch64 Linux" ;;
	esac
}

# node_dist_base: where the pinned Node tarball is fetched from. Fixed in production
# (NV_NODE_DIST_BASE); in test mode, EITRI_INSTALL_TEST_NODE_BASE_URL redirects it to a fixture
# server, and -- deliberately -- an unset override does NOT fall back to the real nodejs.org: this
# host (like the harness's own) can reach the real internet, and a test suite that forgets to set
# the override must fail fast against a refused loopback port, never spend real time downloading a
# real Node tarball from every test that reaches this point.
node_dist_base() {
	if [ "$NV_TEST_MODE" = 1 ]; then
		printf '%s\n' "${EITRI_INSTALL_TEST_NODE_BASE_URL:-http://127.0.0.1:1}"
	else
		printf '%s\n' "$NV_NODE_DIST_BASE"
	fi
}

# grpc_dist_base: where the pinned protoc tarball is fetched from, on the terms of node_dist_base.
grpc_dist_base() {
	if [ "$NV_TEST_MODE" = 1 ]; then
		printf '%s\n' "${EITRI_INSTALL_TEST_GRPC_BASE_URL:-http://127.0.0.1:1}"
	else
		printf '%s\n' "$NV_GRPC_DIST_BASE"
	fi
}

# check_free_space DIR MIN_KIB NEEDS: MIN_KIB free in DIR's filesystem before a build starts; NEEDS says
# what wants it. A df that cannot be read degrades rather than blocking a build over it.
check_free_space() {
	_cfs_kib=$(df -Pk "$1" 2>/dev/null | awk 'NR==2 { print $4 }') || _cfs_kib=
	case $_cfs_kib in
	'' | *[!0-9]*) return 0 ;;
	esac
	if [ "$_cfs_kib" -lt "$2" ]; then
		die "only $((_cfs_kib / 1024)) MiB free in $1's filesystem; $3: free some space and re-run"
	fi
}

# check_sidecar_build_space DIR: about 1.4 GiB free before starting. Three first builds, each in a private
# home, peaked at 1.2 to 1.25 GiB in the work directory and at 1.3 GiB once the 106 MiB artifact was copied
# out while the work directory still stood. A sidecar update, which runs npm ci without install scripts,
# measured 1.15 GiB (NV_SIDECAR_UPDATE_MIN_KIB).
check_sidecar_build_space() {
	check_free_space "$1" "$NV_SIDECAR_BUILD_MIN_KIB" "building the sidecar needs about 1.4 GiB (a measured first build peaked near 1.3 GiB)"
}

# ---------------------------------------------------------------------------------------------
# Release signature verification key.

# embedded_release_signers: packaging/release-signers, verbatim, between the two marker lines
# (the heredoc's own). packaging/tests/install/ asserts the block is byte-equal to that file.
embedded_release_signers() {
	cat <<'EITRI_RELEASE_SIGNERS'
# Eitri release signers.
#
# ssh-keygen's allowed_signers format. packaging/install.sh embeds this file byte for byte (a test
# holds the two equal) and checks a release's SHA256SUMS.sig with exactly:
#
#   ssh-keygen -Y verify -f <this file> -I release@eitri -n eitri-release -s SHA256SUMS.sig < SHA256SUMS
#
# and a sidecar release's with -n eitri-sidecar-release in its place. A key line looks like this, with
# no comment field after the key:
#
#   release@eitri namespaces="eitri-release,eitri-sidecar-release" ssh-ed25519 AAAA...
#
# The release key is listed below: the dedicated release key's public half, never an
# everyday login key, and release.sh refuses one that matches any ~/.ssh/id_*.pub. Every installer
# built from this tree embeds it and refuses a release whose SHA256SUMS.sig is missing or does not
# verify. The installer's --release-signers option replaces it, with a warning, for a
# release candidate or a test.
release@eitri namespaces="eitri-release,eitri-sidecar-release" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICUZrObfSHuE2F8LBZj6YG8pgph6d5v/JEqb8RClBujR
EITRI_RELEASE_SIGNERS
}

# prepare_signers sets NV_SIGNERS and NV_SIGNERS_HAVE_KEY. An unreadable signers file is a
# fatal error, never "no key": treating grep exit 2 as no match would disable verification
# even with a built-in key or an explicit --sig. Refuse an empty --release-signers too,
# since replacing the trust anchor must not silently turn signature checking off.
prepare_signers() {
	if [ -n "$OPT_SIGNERS" ]; then
		warn "checking signatures against $OPT_SIGNERS (--release-signers) instead of the key built into this installer: do this only for a release candidate or a test"
		if [ ! -f "$OPT_SIGNERS" ]; then die "--release-signers $OPT_SIGNERS: no such file"; fi
		if [ ! -r "$OPT_SIGNERS" ]; then
			die "--release-signers $OPT_SIGNERS cannot be read, so no signature could be checked against it: make it readable (or leave the option out) and re-run"
		fi
		NV_SIGNERS=$OPT_SIGNERS
	else
		NV_SIGNERS=$NV_DL/allowed_signers
		if [ "$OPT_DRY_RUN" = 1 ]; then
			NV_SIGNERS_HAVE_KEY=0
			_ps_text=$(embedded_release_signers)
			if printf '%s\n' "$_ps_text" | grep -Eq '^[[:space:]]*[^#[:space:]]'; then NV_SIGNERS_HAVE_KEY=1; fi
			return 0
		fi
		_ps_text=$(embedded_release_signers)
		printf '%s\n' "$_ps_text" >"$NV_SIGNERS" || die "cannot write $NV_SIGNERS"
	fi
	_ps_rc=0
	grep -Eq '^[[:space:]]*[^#[:space:]]' "$NV_SIGNERS" || _ps_rc=$?
	case $_ps_rc in
	0) NV_SIGNERS_HAVE_KEY=1 ;;
	1) NV_SIGNERS_HAVE_KEY=0 ;;
	*) die "grep could not scan $NV_SIGNERS (it exited $_ps_rc), so the release signature cannot be checked: refusing. Make the file readable and re-run" ;;
	esac
	if [ -n "$OPT_SIGNERS" ] && [ "$NV_SIGNERS_HAVE_KEY" != 1 ]; then
		die "--release-signers $OPT_SIGNERS lists no key, so it would switch signature checking off: give a file holding the release key's line (release@eitri namespaces=\"eitri-release,eitri-sidecar-release\" ssh-ed25519 ...), or leave the option out"
	fi
}

# require_ssh_keygen: with a key to check against, a machine without ssh-keygen is refused. Going on
# with checksums alone would install whatever the assets' source served, and the cases the key is
# there for are exactly the ones where that source is not the one this installer came from: a saved
# installer re-run to upgrade, a --base-url mirror, files handed over with --tarball/--sums/--sig.
# --insecure-skip-signature lets such a run go on, and says what that gives up. Sets RSK=1 when
# ssh-keygen is there (the signature is checked), RSK=0 when the user chose to go on without it.
# A variable, not an exit status, and a plain statement: rule 2 above.
require_ssh_keygen() {
	RSK=1
	if command -v ssh-keygen >/dev/null 2>&1; then return 0; fi
	RSK=0
	if [ "$OPT_INSECURE_SKIP_SIGNATURE" = 1 ]; then
		warn "ssh-keygen was not found, so the release signature cannot be checked: installing without checking the release signature (--insecure-skip-signature). The checksums only detect corruption, not who made the release: a mirror or files handed to you could carry a release someone else built, and nothing in this run would notice. Install OpenSSH's client and re-run without the option to have it checked"
		return 0
	fi
	openssh_hint
	die "ssh-keygen was not found, so the release signature cannot be checked, and this installer carries a release key to check it against: refusing, because without that check a release someone else built would install unnoticed. Install OpenSSH's client$OH_HINT and re-run; or, to install with only the checksums checked, re-run with --insecure-skip-signature"
}

# openssh_hint sets OH_HINT to this distribution's command for OpenSSH's client, or nothing.
openssh_hint() {
	read_os_release
	case $OS_FAMILY in
	debian) OH_HINT=' (sudo apt install openssh-client)' ;;
	fedora) OH_HINT=' (sudo dnf install openssh-clients)' ;;
	arch) OH_HINT=' (sudo pacman -S openssh)' ;;
	opensuse) OH_HINT=' (sudo zypper install openssh-clients)' ;;
	*) OH_HINT= ;;
	esac
}

# verify_signature SUMS SIG NAMESPACE: with a key listed, a missing or unfetchable SIG refuses and a
# bad one refuses, and so does a missing ssh-keygen unless --insecure-skip-signature
# (require_ssh_keygen). The command is the documented verify form; ssh-keygen's no-validate check mode is
# never used (it accepts a signature by any key). NAMESPACE is one of the two constants above.
verify_signature() {
	if [ "$NV_SIGNERS_HAVE_KEY" != 1 ]; then
		warn "this installer carries no release key: checksums only detect corruption, they do not show who made the release"
		return 0
	fi
	if [ ! -f "$2" ]; then
		die "this release has no SHA256SUMS.sig, but this installer carries a release key and so requires one: refusing, because an unsigned release could have been altered. Report it at $NV_ISSUES"
	fi
	require_ssh_keygen
	if [ "$RSK" != 1 ]; then return 0; fi
	if [ "$OPT_DRY_RUN" = 1 ] && [ -z "$OPT_SIGNERS" ]; then
		# prepare_signers writes the embedded key to $NV_SIGNERS only in a real run, and a dry run
		# writes nothing: ssh-keygen would find no file and report a good signature as a bad one.
		# Said as the network dry run says it.
		say "would check $2 with ssh-keygen -Y verify against the release key built into this installer (not checked in a dry run)"
		return 0
	fi
	if ! _vs_out=$(ssh-keygen -Y verify -f "$NV_SIGNERS" -I "$NV_SIGNER_IDENTITY" -n "$3" -s "$2" <"$1" 2>&1); then
		die "the signature on SHA256SUMS does not verify against the release key ($_vs_out): refusing to install. Do not use this download; report it at $NV_ISSUES"
	fi
	say "signature on SHA256SUMS: good ($NV_SIGNER_IDENTITY)"
}

# ---------------------------------------------------------------------------------------------
# Choosing and fetching the release

# obtain_release: sets NV_VERSION, NV_TARBALL_NAME and NV_SUMS_TEXT, from the network or
# from --tarball/--sums/--sig. With a key embedded, the signature has been checked when it returns.
obtain_release() {
	prepare_signers
	if [ -n "$OPT_TARBALL" ]; then
		for _or_f in "$OPT_TARBALL" "$OPT_SUMS"; do
			if [ ! -f "$_or_f" ]; then die "$_or_f: no such file"; fi
		done
		if [ -n "$OPT_SIG" ] && [ ! -f "$OPT_SIG" ]; then die "--sig $OPT_SIG: no such file"; fi
		if [ "$NV_SIGNERS_HAVE_KEY" = 1 ] && [ -z "$OPT_SIG" ]; then
			die "this installer carries a release key, so --tarball needs --sig SHA256SUMS.sig too: refusing without it"
		fi
		if [ "$OPT_DRY_RUN" = 1 ]; then
			# A dry run installs nothing, so it reads the files where they lie.
			_or_sums=$OPT_SUMS
			_or_sig=${OPT_SIG:-/nonexistent}
		else
			# recover_interrupted makes this run's download directory empty and 0700. Check signatures
			# and hashes using the same private copies of --sums and --sig. Reopening
			# the originals in a shared directory or network mount could read different SHA256SUMS bytes
			# for verification and installation. do_install copies the tarball for the same reason.
			_or_sums=$NV_DL/SHA256SUMS
			_or_sig=$NV_DL/SHA256SUMS.sig
			cp -- "$OPT_SUMS" "$_or_sums" || die "cannot copy $OPT_SUMS into $NV_DL: check that it is readable and the disk is not full, then re-run"
			if [ -n "$OPT_SIG" ]; then
				cp -- "$OPT_SIG" "$_or_sig" || die "cannot copy $OPT_SIG into $NV_DL: check that it is readable and the disk is not full, then re-run"
			fi
		fi
		verify_signature "$_or_sums" "$_or_sig" "$NV_SIGNATURE_NAMESPACE"
		NV_SUMS_TEXT=$(cat -- "$_or_sums") || die "cannot open $_or_sums"
		NV_VERSION=$(tarball_version)
		if [ -n "$OPT_VERSION" ] && [ "$OPT_VERSION" != "$NV_VERSION" ]; then
			die "--version $OPT_VERSION, but $OPT_SUMS is for $NV_VERSION"
		fi
		refuse_older_layout "$NV_VERSION"
		NV_TARBALL_NAME=eitri-$NV_VERSION-x86_64-linux.tar.gz
		if [ "${OPT_TARBALL##*/}" != "$NV_TARBALL_NAME" ]; then
			die "--tarball must be the file named $NV_TARBALL_NAME that SHA256SUMS lists (it is ${OPT_TARBALL##*/})"
		fi
		# Set NV_REL_URL for the sidecar's source-download fallback under set -u. --base-url and
		# --tarball are mutually exclusive, so the fallback uses GitHub. do_install first checks for
		# the source tarball beside --tarball after reading RELEASE; an offline install with that
		# file present never reaches this URL.
		NV_REL_URL=$NV_DEFAULT_BASE_URL/releases/download/v$NV_VERSION
		return 0
	fi

	# A version named on the command line is judged before anything is downloaded; the latest one as soon as
	# SHA256SUMS names it, before the signature, the tarball or anything else is fetched.
	if [ -n "$OPT_VERSION" ]; then
		refuse_older_layout "$OPT_VERSION"
		_or_dir=releases/download/v$OPT_VERSION
	else
		_or_dir=releases/latest/download
	fi
	if [ "$OPT_DRY_RUN" = 1 ]; then
		NV_SUMS_TEXT=$(fetch "$NV_BASE/$_or_dir/SHA256SUMS" - SHA256SUMS "$NV_MAX_DOWNLOAD_BYTES")
	else
		fetch "$NV_BASE/$_or_dir/SHA256SUMS" "$NV_DL/SHA256SUMS.part" SHA256SUMS "$NV_MAX_DOWNLOAD_BYTES"
		NV_SUMS_TEXT=$(cat -- "$NV_DL/SHA256SUMS.part") || die "cannot open $NV_DL/SHA256SUMS.part"
	fi
	NV_VERSION=$(tarball_version)
	if [ -n "$OPT_VERSION" ] && [ "$OPT_VERSION" != "$NV_VERSION" ]; then
		die "asked for $OPT_VERSION, but the release's SHA256SUMS is for $NV_VERSION: check --version and --base-url"
	fi
	refuse_older_layout "$NV_VERSION"
	NV_TARBALL_NAME=eitri-$NV_VERSION-x86_64-linux.tar.gz
	# From here on every file comes from releases/download/v<X>/, so a release published between
	# two requests cannot mix files.
	NV_REL_URL=$NV_BASE/releases/download/v$NV_VERSION
	if [ "$OPT_DRY_RUN" = 1 ]; then
		if [ -z "$OPT_VERSION" ]; then
			NV_SUMS_TEXT=$(fetch "$NV_REL_URL/SHA256SUMS" - SHA256SUMS "$NV_MAX_DOWNLOAD_BYTES")
			_or_v=$(tarball_version)
			if [ "$_or_v" != "$NV_VERSION" ]; then die "the release changed while it was being fetched: re-run"; fi
		fi
		if [ "$NV_SIGNERS_HAVE_KEY" = 1 ]; then
			# A dry run reports the refusal a real run would make.
			require_ssh_keygen
			if [ "$RSK" = 1 ]; then
				say "would download $NV_REL_URL/SHA256SUMS.sig and check it with ssh-keygen -Y verify (not checked in a dry run)"
			fi
		else
			verify_signature - - "$NV_SIGNATURE_NAMESPACE"
		fi
		return 0
	fi
	if [ -z "$OPT_VERSION" ]; then
		rm -f -- "$NV_DL/SHA256SUMS.part" || die "cannot remove $NV_DL/SHA256SUMS.part"
		fetch "$NV_REL_URL/SHA256SUMS" "$NV_DL/SHA256SUMS.part" SHA256SUMS "$NV_MAX_DOWNLOAD_BYTES"
		NV_SUMS_TEXT=$(cat -- "$NV_DL/SHA256SUMS.part") || die "cannot open $NV_DL/SHA256SUMS.part"
		_or_v=$(tarball_version)
		if [ "$_or_v" != "$NV_VERSION" ]; then die "the release changed while it was being fetched (latest was $NV_VERSION, v$NV_VERSION's SHA256SUMS names $_or_v): re-run"; fi
	fi
	mv -- "$NV_DL/SHA256SUMS.part" "$NV_DL/SHA256SUMS" || die "cannot rename $NV_DL/SHA256SUMS.part"
	if [ "$NV_SIGNERS_HAVE_KEY" = 1 ]; then
		fetch "$NV_REL_URL/SHA256SUMS.sig" "$NV_DL/SHA256SUMS.sig" "SHA256SUMS.sig (this installer carries a release key, so a release without a signature is refused)" "$NV_MAX_DOWNLOAD_BYTES"
	fi
	verify_signature "$NV_DL/SHA256SUMS" "$NV_DL/SHA256SUMS.sig" "$NV_SIGNATURE_NAMESPACE"
	# Every hash used from here on comes from the file just verified, not from the read before it
	# (the download directory is this run's own, but a check and a use of two reads are two files).
	NV_SUMS_TEXT=$(cat -- "$NV_DL/SHA256SUMS") || die "cannot open $NV_DL/SHA256SUMS"
	_or_v=$(tarball_version)
	if [ "$_or_v" != "$NV_VERSION" ]; then die "$NV_DL/SHA256SUMS changed while it was being checked: re-run"; fi
}

# ---------------------------------------------------------------------------------------------
# State changes. Every one goes through `run` (or checks OPT_DRY_RUN itself), so --dry-run prints
# every action and does none.

run() {
	if [ "$OPT_DRY_RUN" = 1 ]; then
		printf 'eitri: would run:'
		printf " '%s'" "$@"
		printf '\n'
		return 0
	fi
	"$@" || die "this failed: $* (see the message above). Nothing after it was done; re-run to finish"
}

# run_in DIR CMD...: run, with the command's cwd set to DIR (the sidecar build's npm calls, which
# must run from the extracted Verdandi source's workspace root). A subshell, not a persistent `cd`:
# this installer's own cwd is never changed.
run_in() {
	_ri_dir=$1
	shift
	if [ "$OPT_DRY_RUN" = 1 ]; then
		printf 'eitri: would run (in %s):' "$_ri_dir"
		printf " '%s'" "$@"
		printf '\n'
		return 0
	fi
	(cd "$_ri_dir" && exec "$@") ||
		die "this failed in $_ri_dir: $* (see the message above). Nothing after it was done; re-run to finish"
}

pid_alive() {
	_pa=0
	case $1 in
	'' | *[!0-9]*) ;;
	*)
		if kill -0 "$1" 2>/dev/null || [ -d "/proc/$1" ]; then _pa=1; fi
		;;
	esac
	printf '%s\n' "$_pa"
}

# acquire_lock: a mkdir lock in <cache>/eitri/, so two runs never share
# eitri.new, the download directory or a sidecar build. A lock left by a dead pid is taken over.
# own_private_group DIR: sets OPG=1 when DIR's group is this user's own user-private group -- the
# user's primary group, named after the user, with no other members: the USERGROUPS_ENAB convention
# of Debian, Ubuntu and Fedora, whose umask 002 routinely leaves a self-owned ~/.cache at 0775.
# There, the group-write bit gives nobody else write access, so the group-write checks below accept
# it. Anything uncertain -- no getent, a failed lookup, a group named otherwise, another listed
# member -- leaves OPG=0, and those checks refuse exactly as before. Called as a plain statement
# (rule 2 above). A second account whose PRIMARY group is this one is not a member in getent's group
# entry either, so the account list is searched for one too; a directory with an extended ACL, or a
# GID two groups share, never qualifies. What this cannot see: accounts an NSS source does not
# enumerate (a directory service with enumeration off) whose primary or supplementary group has
# this GID -- a deliberate, unusual setup, stated in INSTALL.md.
own_private_group() {
	OPG=0
	_opg_gid=$(id -g 2>/dev/null) || return 0
	_opg_user=$(id -un 2>/dev/null) || return 0
	_opg_group=$(id -gn 2>/dev/null) || return 0
	if [ -z "$_opg_user" ] || [ "$_opg_user" != "$_opg_group" ]; then return 0; fi
	[ -n "$(find "$1" -maxdepth 0 -type d -gid "$_opg_gid" -print 2>/dev/null)" ] || return 0
	# An extended ACL can give a named user or group write access that the mode bits do not show (the
	# group bits then report the ACL mask), so a directory with one never gets the allowance. GNU ls
	# marks it with a '+' after the mode (of the directory itself: every caller passes a name already
	# checked to be a directory, not a link, as the find above checks again); getfacl, where it is
	# installed, lists the entries themselves.
	_opg_ls=$(ls -ld -- "$1" 2>/dev/null) || return 0
	case $_opg_ls in ??????????+*) return 0 ;; esac
	if command -v getfacl >/dev/null 2>&1; then
		_opg_acl=$(getfacl -cp -- "$1" 2>/dev/null) || return 0
		if printf '%s\n' "$_opg_acl" | grep -Eq '^(user|group):[^:]+:|^mask::'; then return 0; fi
	fi
	command -v getent >/dev/null 2>&1 || return 0
	_opg_entry=$(getent group "$_opg_group" 2>/dev/null) || return 0
	[ "$(printf '%s\n' "$_opg_entry" | cut -d: -f1)" = "$_opg_group" ] || return 0
	[ "$(printf '%s\n' "$_opg_entry" | cut -d: -f3)" = "$_opg_gid" ] || return 0
	case $(printf '%s\n' "$_opg_entry" | cut -d: -f4) in
	'' | "$_opg_user") ;;
	*) return 0 ;;
	esac
	# A second group with the same GID (groupadd --non-unique) gives its members the same access
	# without being named here: the whole group list must hold exactly one group with this GID.
	# Captured first: in a pipeline getent's own failure (a list cut short) would be lost.
	_opg_groups=$(getent group 2>/dev/null) || return 0
	_opg_gcount=$(printf '%s\n' "$_opg_groups" | awk -F: -v g="$_opg_gid" '$3 == g { n++ } END { print n + 0 }') || return 0
	[ "$_opg_gcount" = 1 ] || return 0
	# A second account whose PRIMARY group is this one is never listed as a member, so look for one
	# in the account list itself -- which must at least list this user, or it is not a list this
	# check can rely on (enumeration off in some NSS setups), and the answer stays no.
	_opg_passwd=$(getent passwd 2>/dev/null) || return 0
	_opg_verdict=$(printf '%s\n' "$_opg_passwd" | awk -F: -v g="$_opg_gid" -v u="$_opg_user" '
		$1 == u { self = 1 }
		$4 == g && $1 != u { other = 1 }
		END { print (self && !other) ? "ok" : "no" }') || return 0
	if [ "$_opg_verdict" = ok ]; then OPG=1; fi
}

# foreign_cache_link NAME UID: sets FCL=1 when NAME is itself a symlink owned by neither UID nor
# root, else FCL=0. find without -H or -L looks at the link, not at what it leads to. Called as a
# plain statement (rule 2 above).
foreign_cache_link() {
	FCL=0
	if [ ! -L "$1" ]; then return 0; fi
	_fcl_hit=$(find "$1" -maxdepth 0 -type l ! -uid "$2" ! -uid 0 -print 2>/dev/null) || _fcl_hit=
	if [ -n "$_fcl_hit" ]; then FCL=1; fi
}

# check_cache_root: everything acquire_lock checks before it takes the lock. NV_CACHE and the paths
# under it are the resolved ones afterwards, and <cache>/eitri exists, unless this is a dry run, which
# stops after the refusals that only look at names.
check_cache_root() {
	NV_LOCK=$NV_CACHE_NV/lock
	# A cache root that is itself a symlink is followed: the path is resolved once below, only the
	# resolved path is used from then on, and every directory on it is checked without following a
	# link, so repointing the link afterwards changes nothing (a ~/.cache linked to another disk is
	# common). One owned by another user (not you, and not root) is refused all the same: it would
	# let them choose where this run creates its cache directory, before anything is resolved. The
	# test is on the name without trailing slashes, which would make it look at the link's target.
	_al_root=$NV_CACHE
	while :; do
		case $_al_root in
		?*/) _al_root=${_al_root%/} ;;
		*) break ;;
		esac
	done
	_al_uid=$(id -u) || die "id -u failed"
	_al_symlink_msg="$_al_root is a symlink owned by another user, who could point it at a directory of their own, so this installer refuses to keep downloads behind it. Set XDG_CACHE_HOME to a directory of your own and re-run"
	foreign_cache_link "$_al_root" "$_al_uid"
	if [ "$FCL" = 1 ]; then die "$_al_symlink_msg"; fi
	# Refuse a symlinked <cache>/eitri before mkdir -p, which silently accepts a link to an
	# existing directory. Another user in a shared sticky directory could plant one and repoint
	# it after checks that only inspected its target's permissions.
	if [ -L "$NV_CACHE_NV" ]; then
		die "$NV_CACHE_NV is a symlink: a download checked there could be replaced by repointing it, and this installer refuses to use it. Remove it, or set XDG_CACHE_HOME to a directory of your own, and re-run"
	fi
	# A dry run stops here, after the two refusals above: they only look at names and change
	# nothing, so it reports them as a real run would. Everything below creates or depends on what
	# creating would leave.
	if [ "$OPT_DRY_RUN" = 1 ]; then return 0; fi
	# A cache directory this run creates is removed again with its contents (on_exit), so a run
	# that installs nothing leaves nothing.
	if [ ! -d "$NV_CACHE" ]; then NV_CACHE_CREATED=1; fi
	if [ ! -d "$NV_CACHE_NV" ]; then NV_CACHE_NV_CREATED=1; fi
	# 0700 when this run creates it; the cache directory above keeps the user's umask.
	# shellcheck disable=SC2174 # -m for the deepest directory only, deliberately
	mkdir -p -m 0700 -- "$NV_CACHE_NV" || die "cannot create $NV_CACHE_NV: check that $NV_CACHE is writable"
	# Again, now that it exists: a link could have been planted between the test above and mkdir.
	foreign_cache_link "$_al_root" "$_al_uid"
	if [ "$FCL" = 1 ]; then die "$_al_symlink_msg"; fi
	# Resolved once, here, and only the resolved path is used from now on: every check below is on
	# the directories that path names, and a symlink further up (a cache moved to another disk and
	# linked from where it was) can no longer be repointed under a check that has already passed.
	_al_real=$(cd -P -- "$NV_CACHE" && pwd -P) || die "cannot enter $NV_CACHE: check that it is a directory you can enter, and re-run"
	case $_al_real in
	*"$NV_NL"*) die "$NV_CACHE resolves to a path holding a newline: set XDG_CACHE_HOME to a plain directory of your own and re-run" ;;
	/*) ;;
	*) die "$NV_CACHE did not resolve to an absolute path ($_al_real): set XDG_CACHE_HOME to a directory of your own and re-run" ;;
	esac
	NV_CACHE=$_al_real
	NV_CACHE_NV=${NV_CACHE%/}/eitri
	NV_DL=$NV_CACHE_NV/download
	NV_STAGE=$NV_CACHE_NV/unpack
	NV_LOCK=$NV_CACHE_NV/lock
	if [ -L "$NV_CACHE_NV" ]; then
		die "$NV_CACHE_NV is a symlink: a download checked there could be replaced by repointing it, and this installer refuses to use it. Remove it, or set XDG_CACHE_HOME to a directory of your own, and re-run"
	fi
	# mkdir -p does not check an existing directory's owner. A planted NV_CACHE_NV lets its
	# owner read or replace downloads regardless of its write bits. Root must require root
	# ownership too: sudo -E can retain another user's HOME and allow that user to replace a
	# checked download before root unpacks it.
	_al_nv_owner_bad=$(find "$NV_CACHE_NV" -maxdepth 0 ! -uid "$_al_uid" -print 2>/dev/null) || _al_nv_owner_bad=
	if [ -n "$_al_nv_owner_bad" ]; then
		die "$NV_CACHE_NV is owned by another user, so a download checked there could already be under their control: remove it (or set XDG_CACHE_HOME to a directory of your own) and re-run"
	fi
	# Other users must not be able to swap the download directory between checking and use.
	# Check group-write as well as other-write: a group-writable 0770 directory permits that swap.
	_al_priv=$(find "$NV_CACHE_NV" -prune -type d ! -perm -0002 ! -perm -0020 -print 2>/dev/null) || _al_priv=
	if [ -z "$_al_priv" ]; then
		# Group-write alone, on this user's own private group (own_private_group), lets no one else in.
		_al_upg=$(find "$NV_CACHE_NV" -prune -type d ! -perm -0002 -perm -0020 -print 2>/dev/null) || _al_upg=
		OPG=0
		if [ -n "$_al_upg" ]; then own_private_group "$NV_CACHE_NV"; fi
		if [ "$OPG" = 1 ]; then _al_priv=$_al_upg; fi
	fi
	if [ -z "$_al_priv" ]; then
		die "other users can write to $NV_CACHE_NV, so a download checked there could be replaced before it is used: run chmod go-w '$NV_CACHE_NV' (or set XDG_CACHE_HOME to a directory of your own) and re-run"
	fi
	# Check ownership and write bits independently on <cache> and every resolved ancestor up
	# to /. A writable non-sticky parent lets another user rename the checked subtree away;
	# a sticky parent owned by another user also permits its owner to rename it. Group-write
	# needs the same scrutiny as other-write, except for an own_private_group.
	# A resolved path alone is insufficient: a writable ancestor can be swapped for a link
	# after resolution. Never follow links in these checks (find without -H or -L), and require
	# each entry to remain a directory owned by this user or root. Combined ancestor checks
	# ensure that only this user or root can replace any entry after it has passed.
	_al_d=$NV_CACHE
	while :; do
		# For uid 0 the two tests are one: owned by root.
		_al_parent_owner_bad=$(find "$_al_d" -maxdepth 0 ! -uid "$_al_uid" ! -uid 0 -print 2>/dev/null) || _al_parent_owner_bad=
		if [ -n "$_al_parent_owner_bad" ]; then
			die "$_al_d is owned by another user (not you, and not root), so its owner could replace $NV_CACHE_NV between checks no matter its permissions: set XDG_CACHE_HOME to a directory of your own and re-run"
		fi
		_al_parent_dir=$(find "$_al_d" -maxdepth 0 -type d -print 2>/dev/null) || _al_parent_dir=
		if [ -z "$_al_parent_dir" ]; then
			die "$_al_d is no longer a directory (a symlink now, or gone) although it was one when the cache path was resolved, so something on that path is being changed while this installer checks it: set XDG_CACHE_HOME to a directory of your own and re-run"
		fi
		_al_parent_bad=$(find "$_al_d" -maxdepth 0 \( -perm -0002 -o -perm -0020 \) ! -perm -1000 -print 2>/dev/null) || _al_parent_bad=
		if [ -n "$_al_parent_bad" ]; then
			# A default Ubuntu ~/.cache can be 0775. Group-write alone on this user's own private
			# group lets no one else in (own_private_group), so it need not prevent setup.
			_al_parent_upg=$(find "$_al_d" -maxdepth 0 -type d ! -perm -0002 -perm -0020 -print 2>/dev/null) || _al_parent_upg=
			OPG=0
			if [ -n "$_al_parent_upg" ]; then own_private_group "$_al_d"; fi
			if [ "$OPG" = 1 ]; then _al_parent_bad=; fi
		fi
		if [ -n "$_al_parent_bad" ]; then
			# The ownership check already limits this parent to us or root. For a self-owned parent,
			# removing unnecessary group/other-write bits is a remedy; chmod +t is also appropriate
			# for a shared directory. An ordinary umask 002 can leave a self-owned ~/.cache at 0775.
			die "$_al_d is writable by its group or by anyone, and not sticky, so another user could replace $NV_CACHE_NV between checks: if you own $_al_d and do not need to share write access to it, run chmod g-w '$_al_d'; if it is meant to be shared, run chmod +t '$_al_d' instead (or set XDG_CACHE_HOME to a directory of your own) and re-run"
		fi
		if [ "$_al_d" = / ]; then break; fi
		_al_d=${_al_d%/*}
		if [ -z "$_al_d" ]; then _al_d=/; fi
	done
}

acquire_lock() {
	check_cache_root
	if [ "$OPT_DRY_RUN" = 1 ]; then
		say "would take the lock $NV_LOCK"
		return 0
	fi
	if ! mkdir -- "$NV_LOCK" 2>/dev/null; then
		# Serialize stale-lock takeover with a mkdir-based sub-lock. Otherwise two runs observing
		# the same dead pid can race: one creates a fresh pid-less NV_LOCK, another renames it,
		# and a third claims the vacated path. Restoring the renamed lock with mv then nests it
		# inside the third run's directory and leaves two runs believing they hold the lock.
		_al_takeover=$NV_LOCK.takeover
		if ! mkdir -- "$_al_takeover" 2>/dev/null; then
			_al_tk_pid=$(cat -- "$_al_takeover/pid" 2>/dev/null) || _al_tk_pid=
			if [ -z "$_al_tk_pid" ]; then
				# The holder may have made the takeover directory but not yet written its pid.
				# Wait as for NV_LOCK so ordinary contention is not reported as permanently stuck.
				sleep 1
				_al_tk_pid=$(cat -- "$_al_takeover/pid" 2>/dev/null) || _al_tk_pid=
			fi
			if [ -z "$_al_tk_pid" ]; then
				# A holder can die between mkdir and writing its pid. Name the pid-less sub-lock itself
				# in the remedy, since waiting cannot recover it and the user must remove it.
				die "$_al_takeover holds no pid, so whether another Eitri installer is using or recovering the lock $NV_LOCK cannot be told: if none is running, remove $_al_takeover and re-run"
			fi
			_al_tk_dead=0
			_al_tk_live=$(pid_alive "$_al_tk_pid")
			if [ "$_al_tk_live" != 1 ]; then _al_tk_dead=1; fi
			if [ "$_al_tk_dead" != 1 ]; then
				# Contention for the takeover sub-lock does not prove NV_LOCK is stale:
				# its pid has not been checked yet, so the message must cover live locks too.
				die "another Eitri installer (pid $_al_tk_pid) is already checking the lock $NV_LOCK: wait a moment and re-run"
			fi
			# Recovering this sub-lock by removal would let two contenders replace each other's lock.
			die "$_al_takeover has a dead pid ($_al_tk_pid), so no installer is using it: if none is running, remove $_al_takeover by hand and re-run"
		fi
		printf '%s\n' "$$" >"$_al_takeover/pid" 2>/dev/null || :
		# Confirm the pid was written before touching the main lock.
		_al_tk_owner=$(cat -- "$_al_takeover/pid" 2>/dev/null) || _al_tk_owner=
		if [ "$_al_tk_owner" != "$$" ]; then
			rm -rf -- "$_al_takeover" || :
			die "cannot write $_al_takeover/pid: check that $NV_CACHE_NV is writable and re-run"
		fi
		TAKEOVER_HELD=1
		# From here to the fresh mkdir below runs under that sub-lock: the pid is read fresh (never
		# an outer read a concurrent run's own takeover might have raced against) and NV_LOCK is
		# never moved back onto a path anything else could since have claimed -- a lock found to
		# belong to someone else, alive or with no pid at all, is left completely untouched, never
		# renamed anywhere.
		_al_pid=$(cat -- "$NV_LOCK/pid" 2>/dev/null) || _al_pid=
		if [ -z "$_al_pid" ]; then
			# The holder may have made the directory and not yet written its pid.
			sleep 1
			_al_pid=$(cat -- "$NV_LOCK/pid" 2>/dev/null) || _al_pid=
		fi
		if [ -z "$_al_pid" ]; then
			rm -rf -- "$_al_takeover" || :
			TAKEOVER_HELD=0
			die "the lock $NV_LOCK holds no pid, so whether another Eitri installer is using it cannot be told: if none is running, remove $NV_LOCK and re-run"
		fi
		_al_live=$(pid_alive "$_al_pid")
		if [ "$_al_live" = 1 ]; then
			rm -rf -- "$_al_takeover" || :
			TAKEOVER_HELD=0
			die "another Eitri installer (pid $_al_pid) is running: wait for it to finish and re-run. If none is running, remove $NV_LOCK and re-run"
		fi
		warn "taking over a lock left by an installer that is no longer running (pid $_al_pid)"
		# Removed directly, never renamed aside first: nothing else can be modifying NV_LOCK while
		# this run holds the takeover sub-lock (every other path that would touch a stale-looking
		# NV_LOCK needs that same sub-lock first), so there is nothing left to protect a rename
		# against.
		if ! rm -rf -- "$NV_LOCK"; then
			rm -rf -- "$_al_takeover" || :
			TAKEOVER_HELD=0
			die "cannot remove the stale lock $NV_LOCK: remove it by hand and re-run"
		fi
		if ! mkdir -- "$NV_LOCK" 2>/dev/null; then
			# The one race this cannot close: an entirely separate, fresh run's own very first
			# `mkdir NV_LOCK` (which needs no sub-lock at all, since to it NV_LOCK simply did not
			# exist) landing in the instant between the rm -rf above and this mkdir. Reported the
			# same way an ordinary lock collision always is, never treated as corruption.
			rm -rf -- "$_al_takeover" || :
			TAKEOVER_HELD=0
			die "another Eitri installer took the lock $NV_LOCK just now: wait for it to finish and re-run"
		fi
		rm -rf -- "$_al_takeover" || :
		TAKEOVER_HELD=0
	fi
	# Mark the lock held only after writing its pid. If the write fails after creating the
	# pid file (for example, a full filesystem), remove the file before rmdir; otherwise the
	# nonempty lock directory would remain stuck. A failed acquisition removes only its own lock.
	if ! printf '%s\n' "$$" >"$NV_LOCK/pid"; then
		rm -f -- "$NV_LOCK/pid" 2>/dev/null || :
		rmdir -- "$NV_LOCK" 2>/dev/null || :
		die "cannot write $NV_LOCK/pid: check that $NV_CACHE_NV is writable and re-run"
	fi
	LOCK_HELD=1
}

# on_exit: releases this run's takeover sub-lock even before the main lock is held.
# A failed run puts back whatever it had moved (the same recovery a later run would do) and never
# leaves its downloads or lock behind. A sidecar check holds no lock, so its own directory goes in
# a branch of its own.
on_exit() {
	_oe_rc=$?
	if [ "$TAKEOVER_HELD" = 1 ]; then
		_oe_tk_owner=$(cat -- "$NV_LOCK.takeover/pid" 2>/dev/null) || _oe_tk_owner=
		if [ "$_oe_tk_owner" = "$$" ]; then rm -rf -- "$NV_LOCK.takeover" 2>/dev/null || :; fi
		TAKEOVER_HELD=0
	fi
	if [ -n "$NV_SIDECAR_CHECK_DIR" ]; then
		rm -rf -- "$NV_SIDECAR_CHECK_DIR" 2>/dev/null || :
		if [ "$NV_CACHE_NV_CREATED" = 1 ]; then rmdir -- "$NV_CACHE_NV" 2>/dev/null || :; fi
		if [ "$NV_CACHE_CREATED" = 1 ]; then rmdir -- "$NV_CACHE" 2>/dev/null || :; fi
	fi
	if [ "$LOCK_HELD" = 1 ]; then
		if [ "$_oe_rc" != 0 ] && [ "$NV_LEAVE_LIB" != 1 ]; then
			if [ "$NV_NEW_CREATED" = 1 ]; then rm -rf -- "$NV_LIB.new" 2>/dev/null || :; fi
			if [ ! -e "$NV_LIB" ] && [ -d "$NV_LIB.old" ]; then mv -- "$NV_LIB.old" "$NV_LIB" 2>/dev/null || :; fi
			# unpack_new can create NV_LIBROOT before a later fatal step. rmdir removes it only while
			# empty, preserving any install or eitri.old already there.
			rmdir -- "$NV_LIBROOT" 2>/dev/null || :
		fi
		# The temporary names a write goes through before its `mv` (none is left on success).
		for _oe_t in "$NV_BINDIR/.eitri.tmp.$$" \
			"$NV_DATA/licenses/eitri/.LICENSE.tmp.$$" "$NV_DATA/licenses/eitri/.NOTICE.tmp.$$" \
			"$NV_DATA/licenses/eitri/.THIRD-PARTY-LICENSES.tmp.$$" \
			"$NV_DATA/licenses/eitri/.SOURCE.tmp.$$" "$NV_DATA/licenses/eitri/.installed-version.tmp.$$"; do
			rm -f -- "$_oe_t" 2>/dev/null || :
		done
		for _oe_i in $NV_ICON_FILES; do
			rm -f -- "$NV_DATA/icons/${_oe_i%/*}/.${_oe_i##*/}.tmp.$$" 2>/dev/null || :
		done
		for _oe_i in $NV_DESKTOP; do
			rm -f -- "$NV_DATA/applications/.$_oe_i.tmp.$$" 2>/dev/null || :
		done
		for _oe_i in $NV_PLUGIN_FILES; do
			rm -f -- "$NV_DATA/eitri/eitri.nvim/${_oe_i%/*}/.${_oe_i##*/}.tmp.$$" 2>/dev/null || :
		done
		for _oe_i in $NV_EXT_FILES; do
			rm -f -- "$NV_DATA/gnome-shell/extensions/$NV_EXT_ID/.$_oe_i.tmp.$$" 2>/dev/null || :
		done
		if [ -n "${NV_SIDECAR_TMP-}" ]; then rm -f -- "$NV_SIDECAR_TMP" 2>/dev/null || :; fi
		if [ -n "${NV_NVIM_TMP_DEST-}" ]; then rm -rf -- "$NV_NVIM_TMP_DEST" 2>/dev/null || :; fi
		# A sidecar update stopped part-way: its unfinished copy goes, and whatever it set aside comes
		# back unless the new directory has already taken its place.
		if [ -n "$NV_UPDATE_TMP" ]; then rm -rf -- "$NV_UPDATE_TMP" 2>/dev/null || :; fi
		if [ -n "$NV_SELECTED_TMP" ]; then rm -f -- "$NV_SELECTED_TMP" 2>/dev/null || :; fi
		if [ -n "$NV_UPDATE_ASIDE" ]; then
			if [ -e "$NV_UPDATE_DEST" ] || [ -L "$NV_UPDATE_DEST" ]; then
				rm -rf -- "$NV_UPDATE_ASIDE" 2>/dev/null || :
			else
				mv -- "$NV_UPDATE_ASIDE" "$NV_UPDATE_DEST" 2>/dev/null || :
			fi
		fi
		if [ "$NV_UPDATES_CREATED" = 1 ]; then rmdir -- "$NV_SIDECAR_UPDATES" 2>/dev/null || :; fi
		rm -rf -- "$NV_DL" "$NV_STAGE" 2>/dev/null || :
		# install_nvim_version's own workdir (that function removes it itself when it succeeds, whatever
		# --keep-build says) and --from-source's own scratch (the cloned Eitri source, the Verdandi source
		# it archived or cloned, the downloaded Node and Skia archives -- Node alone is about 200 MB): kept
		# with --keep-build, the same knob the sidecar build honours, and otherwise removed here on every
		# exit path, a death part-way through finish_install included. The RELEASE a --checkout run
		# synthesizes is this run's own, named by its own pid, and goes with them.
		if [ "$OPT_KEEP_BUILD" != 1 ]; then
			rm -rf -- "$NV_CACHE_NV/nvim-build" "$NV_CACHE_NV/src" "$NV_CACHE_NV/verdandi-src" \
				"$NV_CACHE_NV/verdandi-src-build" "$NV_CACHE_NV/from-source-node" \
				"$NV_CACHE_NV/from-source-skia" 2>/dev/null || :
			rm -f -- "$NV_CACHE_NV/from-source-RELEASE.$$" 2>/dev/null || :
		fi
		# The sidecar build's own work directory (removed after success, kept with
		# --keep-build). Set only by install_sidecar_for_rev/do_build_sidecar_into, which run under
		# this same lock, so nothing else's work directory is ever named here. Its parent
		# (sidecar-build/, made by this same run's mkdir -p) is removed too, but only if it is now
		# empty -- never disturbing a sibling rev directory a --keep-build run of a different rev
		# left behind.
		if [ -n "${NV_SIDECAR_WORKDIR-}" ] && [ "$OPT_KEEP_BUILD" != 1 ]; then
			rm -rf -- "$NV_SIDECAR_WORKDIR" 2>/dev/null || :
			rmdir -- "$NV_CACHE_NV/sidecar-build" 2>/dev/null || :
		fi
		rm -rf -- "$NV_LOCK" 2>/dev/null || :
		rmdir -- "$NV_CACHE_NV" 2>/dev/null || :
		if [ "$NV_CACHE_CREATED" = 1 ]; then rmdir -- "$NV_CACHE" 2>/dev/null || :; fi
		LOCK_HELD=0
	fi
	exit "$_oe_rc"
}

# recover_interrupted: a stale eitri.new goes; a missing eitri with a
# eitri.old comes back; with both present, eitri.old goes -- otherwise the swap's
# `mv eitri eitri.old` would move the tree inside it.
recover_interrupted() {
	if [ -e "$NV_LIB.new" ] || [ -L "$NV_LIB.new" ]; then
		say "removing $NV_LIB.new, left by an interrupted run"
		run rm -rf -- "$NV_LIB.new"
	fi
	if [ ! -e "$NV_LIB" ] && [ ! -L "$NV_LIB" ] && [ -d "$NV_LIB.old" ]; then
		say "restoring $NV_LIB from $NV_LIB.old, left by an interrupted upgrade"
		run mv -- "$NV_LIB.old" "$NV_LIB"
	fi
	if { [ -e "$NV_LIB" ] || [ -L "$NV_LIB" ]; } && { [ -e "$NV_LIB.old" ] || [ -L "$NV_LIB.old" ]; }; then
		say "removing $NV_LIB.old, left by an interrupted upgrade"
		run rm -rf -- "$NV_LIB.old"
	fi
	for _ri in "$NV_DL" "$NV_STAGE"; do
		if [ -e "$_ri" ] || [ -L "$_ri" ]; then run rm -rf -- "$_ri"; fi
	done
	# Made by this run, empty, and readable only by this user: the offline files are copied in here
	# and the downloads land here, each checked and then used (obtain_release).
	run mkdir -m 0700 -- "$NV_DL"
}

# sidecar_state REV7: NV_SIDECAR_PRESENT=1 when that rev's sidecar is *present*:
# the binary and BUILD both exist, and the binary's first --version line equals BUILD's
# SIDECAR_VERSION_LINE.
sidecar_state() {
	sidecar_dir_state "$NV_SIDECAR_ROOT/$1" "$1"
}

# sidecar_dir_state DIR NAME: sidecar_state, for the sidecar in DIR; an empty NAME is never present.
sidecar_dir_state() {
	NV_SIDECAR_PRESENT=0
	NV_SIDECAR_BIN=$1/verdandi-claude-sidecar
	_ss_build=$1/BUILD
	if [ -z "$2" ] || [ ! -x "$NV_SIDECAR_BIN" ] || [ ! -f "$_ss_build" ]; then return 0; fi
	_ss_want=$(kv_get "$_ss_build" SIDECAR_VERSION_LINE)
	_ss_have=$("$NV_SIDECAR_BIN" --version 2>/dev/null | head -n 1) || _ss_have=
	if [ -n "$_ss_want" ] && [ "$_ss_have" = "$_ss_want" ]; then NV_SIDECAR_PRESENT=1; fi
}

# ---------------------------------------------------------------------------------------------
# Build the sidecar before swapping, so a failed build preserves the installed release.
# Only Node's initial download may be tolerant, and only when no working sidecar is being
# replaced: warn and let the runtime tell the user to run eitri setup. On an upgrade with
# a working sidecar, that failure is fatal because swapping would break the agent panel.
# After the verified Node download, every source-checksum, npm or artifact failure is fatal
# on first installs and upgrades alike: a partial build must not silently reach the swap.
# Explicit --sidecar-only and --build-sidecar-into requests tolerate no download failure,
# since producing the sidecar is their entire job.

# sidecar_download_node WORKDIR TOLERANT: the Node download. Sets SDN_TARBALL and SDN_OK (rule 2:
# a predicate reports through a variable; this function is always called as a plain statement, never
# from if/&&/||, so its own die calls -- and any inside file_sha256, called through $(...) here -- are
# never at risk of running with set -e disabled). On failure, TOLERANT=1 warns and leaves SDN_OK=0
# (see the note above); TOLERANT=0 dies.
sidecar_download_node() {
	SDN_OK=0
	SDN_TARBALL=
	node_arch
	case $NV_NODE_ARCH in
	x64) _sdn_sha=$REL_NODE_SHA256_LINUX_X64 ;;
	arm64) _sdn_sha=$REL_NODE_SHA256_LINUX_ARM64 ;;
	esac
	_sdn_name=node-$REL_NODE_VERSION-linux-$NV_NODE_ARCH.tar.xz
	_sdn_base=$(node_dist_base)
	_sdn_url=$_sdn_base/$REL_NODE_VERSION/$_sdn_name
	_sdn_out=$1/$_sdn_name
	curl_get "$_sdn_url" "$_sdn_out.part" "$NV_MAX_DOWNLOAD_BYTES"
	if [ "$CG_OK" != 1 ]; then
		if [ "$2" = 1 ]; then
			warn "could not download the pinned Node runtime ($_sdn_url), needed to build the sidecar: $CG_ERR. Eitri is installed without one; run \"eitri setup\" once you have network access to nodejs.org"
			return 0
		fi
		die "could not download the pinned Node runtime ($_sdn_url): $CG_ERR"
	fi
	_sdn_have=$(file_sha256 "$_sdn_out.part")
	if [ "$_sdn_have" != "$_sdn_sha" ]; then
		rm -f -- "$_sdn_out.part" || :
		die "checksum mismatch for $_sdn_name: expected $_sdn_sha, got $_sdn_have. Refusing to build the sidecar around an unverified Node runtime; re-run, and report it at $NV_ISSUES if it persists"
	fi
	mv -- "$_sdn_out.part" "$_sdn_out" || die "cannot rename $_sdn_out.part"
	SDN_TARBALL=$_sdn_out
	SDN_OK=1
}

# sidecar_download_verdandi_source WORKDIR: the Verdandi source download, from the release's own asset URL
# (NV_REL_URL, set by whichever caller resolved a release). Reached only once Node is already down
# (see above), so this is always fatal on failure -- never tolerant.
sidecar_download_verdandi_source() {
	# --from-source has no release page to fetch this from: resolve_verdandi_source
	# has already produced the tarball itself (a `git archive`, local or of a fresh clone) and set
	# NV_VERDANDI_SOURCE_TARBALL_OVERRIDE, whose own sha256 IS REL_VERDANDI_SOURCE_SHA256 in that
	# case (there is no external pin to check a synthesized RELEASE's own asset against). The
	# ordinary release path below is unchanged.
	if [ -n "${NV_VERDANDI_SOURCE_TARBALL_OVERRIDE-}" ]; then
		_sdv_have=$(file_sha256 "$NV_VERDANDI_SOURCE_TARBALL_OVERRIDE")
		if [ "$_sdv_have" != "$REL_VERDANDI_SOURCE_SHA256" ]; then
			# Two different producers set this override, and a mismatch means two different
			# things: resolve_verdandi_source's own `git archive` genuinely was built moments
			# earlier (a checksum drift there is either the checkout changing mid-run or this
			# installer's own bug); use_verdandi_source_beside_tarball's copy is a file the USER
			# placed beside --tarball, which this installer never built at all, so the honest
			# reading there is "the wrong file" or "corrupt", not "changed after being built".
			die "the Verdandi source archive at $NV_VERDANDI_SOURCE_TARBALL_OVERRIDE does not match this release: expected $REL_VERDANDI_SOURCE_SHA256, got $_sdv_have. If this installer built it (--checkout/--verdandi-checkout), it changed after being built: report it at $NV_ISSUES. If you placed it beside --tarball yourself, it is not this release's own $REL_VERDANDI_SOURCE: re-download the correct asset, or remove it so this installer fetches it itself"
		fi
		SDV_TARBALL=$NV_VERDANDI_SOURCE_TARBALL_OVERRIDE
		return 0
	fi
	# A separate setup run beside a checkout-built RELEASE has no source override. Its
	# VERDANDI_SOURCE_SHA256 hashes git-archive bytes, which differ from the release asset.
	# Report an unresolvable checkout manifest rather than call that expected mismatch tampering.
	if [ "$REL_BUILT_FROM_CHECKOUT" = 1 ]; then
		die "$NV_RELEASE_PATH was built from a --checkout tree (EITRI_BUILT_FROM_CHECKOUT=1): its VERDANDI_SOURCE_SHA256 is a git-archive hash, not the real release asset's, so this cannot be fetched from $NV_REL_URL. Re-run sh install.sh --from-source --checkout DIR (the same tree, or one at the same commit) to rebuild it instead"
	fi
	_sdv_url=$NV_REL_URL/$REL_VERDANDI_SOURCE
	_sdv_out=$1/$REL_VERDANDI_SOURCE
	curl_get "$_sdv_url" "$_sdv_out.part" "$NV_MAX_DOWNLOAD_BYTES"
	if [ "$CG_OK" != 1 ]; then die "could not download the Verdandi source ($_sdv_url): $CG_ERR"; fi
	_sdv_have=$(file_sha256 "$_sdv_out.part")
	if [ "$_sdv_have" != "$REL_VERDANDI_SOURCE_SHA256" ]; then
		rm -f -- "$_sdv_out.part" || :
		die "checksum mismatch for $REL_VERDANDI_SOURCE: expected $REL_VERDANDI_SOURCE_SHA256, got $_sdv_have. Refusing to build the sidecar from unverified source; re-run, and report it at $NV_ISSUES if it persists"
	fi
	mv -- "$_sdv_out.part" "$_sdv_out" || die "cannot rename $_sdv_out.part"
	SDV_TARBALL=$_sdv_out
}

# sidecar_sdk_notice REPO: after npm ci, report the SDK version and LICENSE.md from the
# installed files before building. The user should know they are downloading a non-open-source
# SDK, and the notice must follow those files rather than a hardcoded version or licence.
sidecar_sdk_notice() {
	_ssn_pkg=$1/node_modules/@anthropic-ai/claude-agent-sdk/package.json
	_ssn_lic=$1/node_modules/@anthropic-ai/claude-agent-sdk/LICENSE.md
	_ssn_ver=
	if [ -f "$_ssn_pkg" ]; then
		_ssn_ver=$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$_ssn_pkg" | head -n 1) || _ssn_ver=
	fi
	_ssn_terms=
	if [ -f "$_ssn_lic" ]; then
		_ssn_terms=$(sed -n '/[^[:space:]]/{ p; q; }' "$_ssn_lic") || _ssn_terms=
	fi
	say "downloading Anthropic's claude-agent-sdk${_ssn_ver:+ $_ssn_ver} to this machine to build the sidecar -- it is not open source: ${_ssn_terms:-see its LICENSE.md once built}"
}

# accept_sidecar_artifact PATH: refuses unless the artifact's own --version says
# the pinned Node version, protocol NV_SIDECAR_PROTOCOL_MAJOR, and host_cli only (never sdk_bundled):
# a build that produced a runnable file is not enough on its own. Sets AA_VERSION_LINE.
accept_sidecar_artifact() {
	_aa_out=$("$1" --version 2>&1) || die "the built artifact ($1) does not run (--version failed): $_aa_out. Report it at $NV_ISSUES"
	_aa_l1=$(printf '%s\n' "$_aa_out" | sed -n 1p)
	_aa_l4=$(printf '%s\n' "$_aa_out" | sed -n 4p)
	case $_aa_l1 in
	*"(protocol $NV_SIDECAR_PROTOCOL_MAJOR, node $REL_NODE_VERSION,"*) ;;
	*) die "the built artifact reports \"$_aa_l1\", not protocol $NV_SIDECAR_PROTOCOL_MAJOR with node $REL_NODE_VERSION: refusing to install it. Report it at $NV_ISSUES" ;;
	esac
	if [ "$_aa_l4" != 'executable sources served: host_cli' ]; then
		die "the built artifact serves \"$_aa_l4\", not host_cli only: a self-built sidecar must never bundle the Claude Code CLI. Report it at $NV_ISSUES"
	fi
	AA_VERSION_LINE=$_aa_l1
}

# check_bundled_protoc_pin: validate metadata without downloading, including in a dry run.
# Published releases through 0.3.0 have none; keep those installable with an explicit warning.
check_bundled_protoc_pin() {
	if [ "$REL_GRPC_TOOLS_PRESENT" = 0 ]; then
		_pbp_cmp=$(semver_cmp "$REL_VERSION" 0.3.0)
		if [ "$_pbp_cmp" -le 0 ]; then
			warn "RELEASE $REL_VERSION has no protoc pin: using legacy npm install scripts and an unverified compiler download; upgrade to a release carrying GRPC_TOOLS_VERSION and GRPC_TOOLS_SHA256 fields for a verified build"
			return 0
		fi
		die "RELEASE $REL_VERSION has no protoc pin: refusing to build the sidecar; upgrade to a release carrying GRPC_TOOLS_VERSION and GRPC_TOOLS_SHA256 fields"
	fi
	if ! printf '%s\n' "$REL_GRPC_TOOLS_VERSION" | grep -Eqx '[0-9]+\.[0-9]+\.[0-9]+' ||
		! printf '%s\n' "$REL_GRPC_TOOLS_SHA256_LINUX_X64" | grep -Eqx '[0-9a-f]{64}' ||
		! printf '%s\n' "$REL_GRPC_TOOLS_SHA256_LINUX_ARM64" | grep -Eqx '[0-9a-f]{64}'; then
		die "RELEASE has an incomplete or malformed GRPC_TOOLS protoc pin: refusing to build the sidecar; upgrade to a release carrying a complete pin"
	fi
}

prepare_bundled_protoc() {
	SB_NPM_CI_FLAGS=
	SB_PROTOC_TARBALL=
	check_bundled_protoc_pin
	if [ "$REL_GRPC_TOOLS_PRESENT" = 0 ]; then return 0; fi
	SR_GRPC_TOOLS_VERSION=$REL_GRPC_TOOLS_VERSION
	SR_GRPC_TOOLS_SHA256_LINUX_X64=$REL_GRPC_TOOLS_SHA256_LINUX_X64
	SR_GRPC_TOOLS_SHA256_LINUX_ARM64=$REL_GRPC_TOOLS_SHA256_LINUX_ARM64
	sidecar_download_protoc "$1"
	SB_NPM_CI_FLAGS=--ignore-scripts
	SB_PROTOC_TARBALL=$SDP_TARBALL
}

# sidecar_build_core WORKDIR NODE_TARBALL VERDANDI_SOURCE_TARBALL: the work-directory layout, the
# node-cache placement, npm ci, the SDK notice, npm run build:binary, and the artifact pick +
# acceptance. Both callers below hand it already-downloaded-and-verified tarballs, whether fetched
# over the network or given as local files (--build-sidecar-into). Sets SB_ARTIFACT, SB_VERSION_LINE,
# SB_SDK_LICENSE, SB_NODE_LICENSE and SB_NODE_BIN. SB_NPM_CI_FLAGS goes on npm ci's command line, and a
# non-empty SB_PROTOC_TARBALL (a checked protoc archive) is unpacked into grpc-tools right after it.
sidecar_build_core() {
	_sbc_workdir=$1
	_sbc_node_tar=$2
	_sbc_src_tar=$3
	if [ -z "$SB_PROTOC_TARBALL" ]; then prepare_bundled_protoc "$_sbc_workdir"; fi
	_sbc_node_dir=$_sbc_workdir/node
	_sbc_repo=$_sbc_workdir/repo
	mkdir -p -- "$_sbc_node_dir" "$_sbc_repo" || die "cannot create $_sbc_workdir's build layout"
	archive_names_inside -tf "$_sbc_node_tar" "$_sbc_node_tar"
	tar -xf "$_sbc_node_tar" --no-same-owner -C "$_sbc_node_dir" || die "could not unpack $_sbc_node_tar: report it at $NV_ISSUES"
	links_stay_inside "$_sbc_node_dir" "$_sbc_node_tar"
	_sbc_node_top=$_sbc_node_dir/node-$REL_NODE_VERSION-linux-$NV_NODE_ARCH
	_sbc_node_bin=$_sbc_node_top/bin
	if [ ! -x "$_sbc_node_bin/node" ] || [ ! -x "$_sbc_node_bin/npm" ]; then
		die "$_sbc_node_tar does not contain bin/node and bin/npm at $_sbc_node_top: not a Node distribution tarball. Report it at $NV_ISSUES"
	fi
	archive_names_inside -tzf "$_sbc_src_tar" "$_sbc_src_tar"
	tar -xzf "$_sbc_src_tar" --no-same-owner -C "$_sbc_repo" || die "could not unpack $_sbc_src_tar: report it at $NV_ISSUES"
	links_stay_inside "$_sbc_repo" "$_sbc_src_tar"
	if [ ! -f "$_sbc_repo/package.json" ] || [ ! -f "$_sbc_repo/apps/claude-sidecar/package.json" ]; then
		die "$_sbc_src_tar does not look like the Verdandi source (no package.json / apps/claude-sidecar/package.json): report it at $NV_ISSUES"
	fi
	# buildBinary.mjs reuses this exact cache and re-verifies it against its own pinned checksum -- a
	# second, independent check.
	_sbc_cache_dir=$_sbc_repo/apps/claude-sidecar/build/node-cache
	mkdir -p -- "$_sbc_cache_dir" || die "cannot create $_sbc_cache_dir"
	cp -- "$_sbc_node_tar" "$_sbc_cache_dir/" || die "cannot copy $_sbc_node_tar into $_sbc_cache_dir"

	_sbc_npm_cache=$_sbc_workdir/npm-cache
	# npm retries a failed registry request itself (twice by default, waiting 10 to 60 s); it is
	# asked for five tries starting at 2 s, like this script's own downloads, unless the user's
	# environment sets them (npm_config_* or NPM_CONFIG_*; a value in ~/.npmrc ranks below these).
	# Every package is still checked against the lockfile's integrity hash.
	_sbc_npm_retries=${npm_config_fetch_retries:-${NPM_CONFIG_FETCH_RETRIES:-5}}
	_sbc_npm_retry_min=${npm_config_fetch_retry_mintimeout:-${NPM_CONFIG_FETCH_RETRY_MINTIMEOUT:-2000}}
	# shellcheck disable=SC2086 # a list of npm's own flags, empty or set by this script
	run_in "$_sbc_repo" env "PATH=$_sbc_node_bin:$PATH" "npm_config_cache=$_sbc_npm_cache" \
		"npm_config_fetch_retries=$_sbc_npm_retries" "npm_config_fetch_retry_mintimeout=$_sbc_npm_retry_min" \
		"$_sbc_node_bin/npm" ci --no-audit --no-fund $SB_NPM_CI_FLAGS
	if [ -n "$SB_PROTOC_TARBALL" ]; then sidecar_place_protoc "$_sbc_repo"; fi
	sidecar_sdk_notice "$_sbc_repo"
	run_in "$_sbc_repo" env "PATH=$_sbc_node_bin:$PATH" "npm_config_cache=$_sbc_npm_cache" \
		"$_sbc_node_bin/npm" run build:binary -w @verdandi/claude-sidecar

	_sbc_dist=$_sbc_repo/apps/claude-sidecar/dist-bin
	_sbc_artifact=$(find "$_sbc_dist" -maxdepth 1 -name "verdandi-claude-sidecar-*-linux-$NV_NODE_ARCH" -printf '%T@ %p\n' 2>/dev/null |
		LC_ALL=C sort -rn | head -n 1 | cut -d' ' -f2-) || _sbc_artifact=
	if [ -z "$_sbc_artifact" ]; then
		die "npm run build:binary produced no $_sbc_dist/verdandi-claude-sidecar-*-linux-$NV_NODE_ARCH: report it at $NV_ISSUES"
	fi
	accept_sidecar_artifact "$_sbc_artifact"
	SB_ARTIFACT=$_sbc_artifact
	SB_VERSION_LINE=$AA_VERSION_LINE
	SB_SDK_LICENSE=$_sbc_repo/node_modules/@anthropic-ai/claude-agent-sdk/LICENSE.md
	SB_NODE_LICENSE=$_sbc_node_top/LICENSE
	SB_NODE_BIN=$_sbc_node_bin
}

# sidecar_place_protoc REPO: with no install script run, grpc-tools has no protoc. Put the checked one
# where its script would have, refusing a grpc-tools of another version and an archive holding
# anything but the two executables: its hash says who chose it, this says what it is.
sidecar_place_protoc() {
	_spp_dir=$1/node_modules/grpc-tools
	if [ ! -f "$_spp_dir/package.json" ]; then
		die "npm ci installed no grpc-tools, which the build needs: report it at $NV_ISSUES"
	fi
	_spp_ver=$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$_spp_dir/package.json" | head -n 1) || _spp_ver=
	if [ "$_spp_ver" != "$SR_GRPC_TOOLS_VERSION" ]; then
		die "npm ci installed grpc-tools $_spp_ver, not the $SR_GRPC_TOOLS_VERSION this release pins protoc for: refusing to build. Report it at $NV_ISSUES"
	fi
	_spp_names=$(tar -tzf "$SB_PROTOC_TARBALL") || die "tar cannot list $SB_PROTOC_TARBALL: report it at $NV_ISSUES"
	_spp_names=$(printf '%s\n' "$_spp_names" | LC_ALL=C sort)
	if [ "$_spp_names" != "bin/${NV_NL}bin/grpc_node_plugin${NV_NL}bin/protoc" ]; then
		die "$SB_PROTOC_TARBALL holds other names than bin/protoc and bin/grpc_node_plugin: refusing to unpack it. Report it at $NV_ISSUES"
	fi
	tar -xzf "$SB_PROTOC_TARBALL" --no-same-owner -C "$_spp_dir" || die "could not unpack $SB_PROTOC_TARBALL: report it at $NV_ISSUES"
	for _spp_f in "$_spp_dir/bin/protoc" "$_spp_dir/bin/grpc_node_plugin"; do
		if [ -L "$_spp_f" ] || [ ! -f "$_spp_f" ] || [ ! -x "$_spp_f" ]; then
			die "$SB_PROTOC_TARBALL unpacked $_spp_f as something not a plain executable: refusing to run it. Report it at $NV_ISSUES"
		fi
	done
}

# install_sidecar_for_rev REV7 TOLERANT: the per-user path, used by both ensure_sidecar
# (TOLERANT=1) and --sidecar-only (TOLERANT=0). NV_SIDECAR_WORKDIR is read by on_exit, which removes
# it unless --keep-build.
install_sidecar_for_rev() {
	check_bundled_protoc_pin
	_isr_rev7=$1
	_isr_tolerant=$2
	_isr_dest=$NV_SIDECAR_ROOT/$_isr_rev7
	_isr_workdir=$NV_CACHE_NV/sidecar-build/$_isr_rev7
	rm -rf -- "$_isr_workdir" || die "cannot remove the stale sidecar build directory $_isr_workdir"
	mkdir -p -- "$_isr_workdir" || die "cannot create $_isr_workdir"
	NV_SIDECAR_WORKDIR=$_isr_workdir
	check_sidecar_build_space "$_isr_workdir"
	# Print status before the slow build: Node download, npm ci's roughly 120 MB SDK, TypeScript
	# and the binary take about a minute on a 25 Mbit/s link with a cold npm cache, limited by the
	# network -- hence the range. Other steps are quick and silent. The AUR entry point leaves
	# this message to the PKGBUILD's msg2.
	say "downloading Node and the Claude Agent SDK, then building the sidecar (1-3 min)"
	sidecar_download_node "$_isr_workdir" "$_isr_tolerant"
	if [ "$SDN_OK" != 1 ]; then return 0; fi
	sidecar_download_verdandi_source "$_isr_workdir"
	sidecar_build_core "$_isr_workdir" "$SDN_TARBALL" "$SDV_TARBALL"
	mkdir -p -- "$_isr_dest" || die "cannot create $_isr_dest"
	_isr_tmp=$_isr_dest/.verdandi-claude-sidecar.tmp.$$
	NV_SIDECAR_TMP=$_isr_tmp
	cp -- "$SB_ARTIFACT" "$_isr_tmp" || die "cannot write $_isr_tmp"
	chmod 0755 "$_isr_tmp" || die "cannot chmod $_isr_tmp"
	mv -- "$_isr_tmp" "$_isr_dest/verdandi-claude-sidecar" || die "cannot move the built sidecar into $_isr_dest"
	NV_SIDECAR_TMP=
	if [ -f "$SB_SDK_LICENSE" ]; then cp -- "$SB_SDK_LICENSE" "$_isr_dest/LICENSE.md" || die "cannot write $_isr_dest/LICENSE.md"; fi
	# BUILD is written last: sidecar_state only counts a rev present once both
	# the binary and this file exist and agree. Its own VERDANDI_REV line names the rev actually
	# built when --allow-verdandi-rev-mismatch let resolve_verdandi_source diverge
	# from the pin -- the directory is still keyed by REL_REV7 (what agent::EXPECTED_VERDANDI_REVISION
	# looks for), but this file says honestly what is inside it.
	printf 'VERDANDI_REV=%s\nSIDECAR_VERSION_LINE=%s\nBUILT_AT=%s\n' \
		"${NV_SIDECAR_ACTUAL_VERDANDI_REV:-$REL_VERDANDI_REV}" "$SB_VERSION_LINE" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >"$_isr_dest/BUILD" ||
		die "cannot write $_isr_dest/BUILD"
	say "built the sidecar for verdandi $_isr_rev7: $SB_VERSION_LINE"
}

# ensure_sidecar: called from do_install, before the swap.
ensure_sidecar() {
	if [ -z "$NV_NEW_REV7" ]; then
		# Only a dry run that could not read the new RELEASE gets here (dry_run_new_rev).
		say "which sidecar the new release uses is not known to this dry run"
		return 0
	fi
	sidecar_state "$NV_NEW_REV7"
	if [ "$NV_SIDECAR_PRESENT" = 1 ]; then
		say "sidecar for verdandi $NV_NEW_REV7: present"
		return 0
	fi
	if [ "$OPT_DRY_RUN" = 1 ]; then
		check_bundled_protoc_pin
		say "would build the sidecar for verdandi $NV_NEW_REV7 into $NV_SIDECAR_ROOT/$NV_NEW_REV7 (needs network access to nodejs.org and the release server; run \"eitri setup\" later if it cannot)"
		return 0
	fi
	# TOLERANT is 1 only when no WORKING install is
	# being replaced -- NV_INSTALLED_VERSION empty (a first install; do_install/do_from_source set it
	# before finish_install calls here) or the old rev's own sidecar absent (an upgrade from a broken
	# or sidecar-less install has nothing working to lose either). Otherwise 0, so a Node download
	# failure dies here, before swap_in, rather than swapping in a new install with no sidecar over
	# an old one that had one.
	_es_tolerant=1
	if [ -n "$NV_INSTALLED_VERSION" ]; then
		sidecar_state "$NV_OLD_REV7"
		if [ "$NV_SIDECAR_PRESENT" = 1 ]; then _es_tolerant=0; fi
	fi
	install_sidecar_for_rev "$NV_NEW_REV7" "$_es_tolerant"
}

# resolve_setup_release: --sidecar-only's and --build-sidecar-into's RELEASE: from
# beside the running script (dirname "$0" -- a .deb/.rpm/tarball install's own eitri-setup, or the
# eitri-bin PKGBUILD's copy of it), or --release-file. Deliberately never a hard-coded
# ~/.local/lib/eitri or /usr/lib/eitri: a .deb install's eitri-setup must never build against a
# tarball install's RELEASE lying around in ~/.local, or the reverse. Never search ~/.local
# and then /usr. Sets REL_* (parse_release).
resolve_setup_release() {
	if [ -n "$OPT_RELEASE_FILE" ]; then
		NV_RELEASE_PATH=$OPT_RELEASE_FILE
	else
		setup_dir
		NV_RELEASE_PATH=
		if [ -n "$NV_SETUP_DIR" ] && [ -f "$NV_SETUP_DIR/RELEASE" ]; then NV_RELEASE_PATH=$NV_SETUP_DIR/RELEASE; fi
	fi
	if [ -z "$NV_RELEASE_PATH" ]; then
		die "no RELEASE found beside $0, and no --release-file given: pass --release-file PATH, or run this as the eitri-setup installed beside a RELEASE (a .deb, .rpm or tarball install)"
	fi
	if [ ! -f "$NV_RELEASE_PATH" ]; then die "--release-file $NV_RELEASE_PATH: no such file"; fi
	parse_release "$NV_RELEASE_PATH"
}

# setup_dir sets NV_SETUP_DIR to the directory holding the running script, or nothing.
setup_dir() {
	NV_SETUP_DIR=$(cd -- "$(dirname -- "$0")" 2>/dev/null && pwd -P) || NV_SETUP_DIR=
}

# do_sidecar_only: --sidecar-only / `eitri setup`. No SHA256SUMS is fetched: every input
# is checked against the resolved RELEASE alone.
do_sidecar_only() {
	resolve_setup_release
	if [ "$OPT_DRY_RUN" = 1 ]; then
		check_bundled_protoc_pin
		say "would build the sidecar for verdandi $REL_REV7 into $NV_SIDECAR_ROOT/$REL_REV7 (from $NV_RELEASE_PATH)"
		return 0
	fi
	sidecar_state "$REL_REV7"
	if [ "$NV_SIDECAR_PRESENT" = 1 ]; then
		say "sidecar for verdandi $REL_REV7: present"
		report_claude "$NV_SIDECAR_BIN"
		return 0
	fi
	require_curl
	check_base_url "${OPT_BASE_URL:-$NV_DEFAULT_BASE_URL}"
	NV_REL_URL=$NV_BASE/releases/download/v$REL_VERSION
	acquire_lock
	install_sidecar_for_rev "$REL_REV7" 0
	sidecar_state "$REL_REV7"
	report_claude "$NV_SIDECAR_BIN"
}

# do_nvim_only: --nvim-only -- the nvim offer alone, for an already-installed Eitri.
# Reuses resolve_setup_release (--sidecar-only's own "RELEASE beside this script, or --release-file"
# rule): a .deb/.rpm/tarball install's own eitri-setup never reads another install's RELEASE lying
# around. Unlike the install path's own best-effort offer, this is the explicit ask: an already
# -adequate PATH nvim does not suppress it (it works on an existing install), and a
# download failure is fatal (TOLERANT=0), the same asymmetry --sidecar-only has over ensure_sidecar's
# own best-effort call.
do_nvim_only() {
	resolve_setup_release
	check_platform
	nvim_private_state "$REL_NVIM_VERSION"
	if [ "$NV_NVIM_PRIVATE_PRESENT" = 1 ]; then
		say "nvim $REL_NVIM_VERSION: already installed at $NV_NVIM_PRIVATE_BIN"
		return 0
	fi
	if [ "$OPT_DRY_RUN" = 1 ]; then
		say "would install nvim $REL_NVIM_VERSION into $NV_DATA/eitri/nvim/$REL_NVIM_VERSION (from $NV_RELEASE_PATH)"
		return 0
	fi
	require_curl
	acquire_lock
	install_nvim_version "$REL_NVIM_VERSION" "$REL_NVIM_SHA256_LINUX_X86_64" 0
}

# do_nvim_offer: plain setup's conditional half after do_sidecar_only. Use the same offer as
# finish_install, including adequate-PATH-nvim suppression, tty/--yes/--with-nvim/--no-nvim rules
# and tolerance of download failure. The explicit do_nvim_only ignores an adequate PATH nvim,
# skips the prompt and makes a download failure fatal, so it cannot serve as this offer.
# Skip require_curl/acquire_lock when nvim_check or --no-nvim already rules out a fetch;
# acquire_lock itself is dry-run-safe.
do_nvim_offer() {
	resolve_setup_release
	# Checked first, as do_nvim_only does -- with the pinned version already
	# installed privately there is nothing to offer, and nvim_check's "install a newer nvim" warning
	# about an older PATH nvim would be wrong (Eitri uses the private copy instead of that one).
	nvim_private_state "$REL_NVIM_VERSION"
	if [ "$NV_NVIM_PRIVATE_PRESENT" = 1 ]; then
		say "nvim $REL_NVIM_VERSION: already installed at $NV_NVIM_PRIVATE_BIN"
		return 0
	fi
	nvim_check
	NV_NEW_REV7=$REL_REV7
	if [ "$NV_NVIM_OK" != 1 ] && [ "$OPT_NVIM" != no ]; then
		require_curl
		acquire_lock
	fi
	maybe_offer_nvim
}

# do_build_sidecar_into: the AUR entry point -- `eitri-setup --build-sidecar-into DIR
# --node TARBALL --verdandi-source TARBALL`. Both inputs are already on disk (makepkg's own
# source=/sha256sums already fetched them); this still checks them against the resolved RELEASE
# before using either, the same discipline as every other input this installer trusts.
do_build_sidecar_into() {
	resolve_setup_release
	check_bundled_protoc_pin
	if [ "$OPT_DRY_RUN" = 1 ]; then
		say "would build the sidecar for verdandi $REL_REV7 into $OPT_BUILD_SIDECAR_INTO (from $NV_RELEASE_PATH)"
		return 0
	fi
	if [ ! -f "$OPT_NODE_TARBALL" ]; then die "--node $OPT_NODE_TARBALL: no such file"; fi
	if [ ! -f "$OPT_VERDANDI_SOURCE_TARBALL" ]; then die "--verdandi-source $OPT_VERDANDI_SOURCE_TARBALL: no such file"; fi
	node_arch
	case $NV_NODE_ARCH in
	x64) _bsi_node_sha=$REL_NODE_SHA256_LINUX_X64 ;;
	arm64) _bsi_node_sha=$REL_NODE_SHA256_LINUX_ARM64 ;;
	esac
	acquire_lock
	_bsi_workdir=$NV_CACHE_NV/sidecar-build/$REL_REV7
	rm -rf -- "$_bsi_workdir" || die "cannot remove the stale sidecar build directory $_bsi_workdir"
	mkdir -p -- "$_bsi_workdir" || die "cannot create $_bsi_workdir"
	NV_SIDECAR_WORKDIR=$_bsi_workdir
	check_sidecar_build_space "$_bsi_workdir"
	# Copied into the work directory first, then
	# hashed and unpacked from the copies alone -- the same discipline --tarball already holds
	# (obtain_release: hashed where it lay and unpacked from a second read is a TOCTOU; hashed and
	# used from one copy is not). Consistent with --tarball, not a new boundary: the AUR `build()`
	# caller's archives and this running script both sit in the same makepkg $srcdir, so anyone able
	# to swap the archives between the hash and the extract can already rewrite the script itself.
	# The copy used
	# to keep the generic basename node.tar.xz. sidecar_build_core's own node-cache priming
	# (immediately below) preserves whatever basename it is given, but Verdandi's buildBinary.mjs
	# looks in that cache only for the versioned name -- the same one sidecar_download_node's own
	# network path already writes (see its $_sdn_name). A generic basename here meant the AUR
	# build() path found nothing under the name buildBinary.mjs actually checks and downloaded Node a
	# second time from nodejs.org, discarding the byte-verified copy sitting right next to it.
	_bsi_node_copy=$_bsi_workdir/node-$REL_NODE_VERSION-linux-$NV_NODE_ARCH.tar.xz
	cp -- "$OPT_NODE_TARBALL" "$_bsi_node_copy" || die "cannot copy $OPT_NODE_TARBALL into $_bsi_workdir: check that it is readable and the disk is not full"
	_bsi_verdandi_copy=$_bsi_workdir/verdandi-source.tar.gz
	cp -- "$OPT_VERDANDI_SOURCE_TARBALL" "$_bsi_verdandi_copy" || die "cannot copy $OPT_VERDANDI_SOURCE_TARBALL into $_bsi_workdir: check that it is readable and the disk is not full"
	_bsi_have=$(file_sha256 "$_bsi_node_copy")
	if [ "$_bsi_have" != "$_bsi_node_sha" ]; then
		die "checksum mismatch for --node $OPT_NODE_TARBALL: expected $_bsi_node_sha, got $_bsi_have"
	fi
	_bsi_have=$(file_sha256 "$_bsi_verdandi_copy")
	if [ "$_bsi_have" != "$REL_VERDANDI_SOURCE_SHA256" ]; then
		die "checksum mismatch for --verdandi-source $OPT_VERDANDI_SOURCE_TARBALL: expected $REL_VERDANDI_SOURCE_SHA256, got $_bsi_have"
	fi
	sidecar_build_core "$_bsi_workdir" "$_bsi_node_copy" "$_bsi_verdandi_copy"
	mkdir -p -- "$OPT_BUILD_SIDECAR_INTO" || die "cannot create $OPT_BUILD_SIDECAR_INTO"
	_bsi_tmp=$OPT_BUILD_SIDECAR_INTO/.verdandi-claude-sidecar.tmp.$$
	NV_SIDECAR_TMP=$_bsi_tmp
	cp -- "$SB_ARTIFACT" "$_bsi_tmp" || die "cannot write $_bsi_tmp"
	chmod 0755 "$_bsi_tmp" || die "cannot chmod $_bsi_tmp"
	mv -- "$_bsi_tmp" "$OPT_BUILD_SIDECAR_INTO/verdandi-claude-sidecar" || die "cannot move the built sidecar into $OPT_BUILD_SIDECAR_INTO"
	NV_SIDECAR_TMP=
	printf '%s\n' "$REL_VERDANDI_REV" >"$OPT_BUILD_SIDECAR_INTO/verdandi-claude-sidecar.rev" ||
		die "cannot write $OPT_BUILD_SIDECAR_INTO/verdandi-claude-sidecar.rev"
	if [ -f "$SB_SDK_LICENSE" ]; then
		cp -- "$SB_SDK_LICENSE" "$OPT_BUILD_SIDECAR_INTO/LICENSE.md" || die "cannot write $OPT_BUILD_SIDECAR_INTO/LICENSE.md"
	fi
	if [ -f "$SB_NODE_LICENSE" ]; then
		cp -- "$SB_NODE_LICENSE" "$OPT_BUILD_SIDECAR_INTO/NODE-LICENSE" || die "cannot write $OPT_BUILD_SIDECAR_INTO/NODE-LICENSE"
	fi
	say "built the sidecar for verdandi $REL_REV7 into $OPT_BUILD_SIDECAR_INTO: $SB_VERSION_LINE"
}

# ---------------------------------------------------------------------------------------------
# Sidecar updates. A sidecar release is signed source: an update is built here, tried, and chosen by
# one file read at each spawn. Each step refuses with `selected` and updates/ as they were. The chain
# of trust is the key, then SHA256SUMS, then SIDECAR-RELEASE and the source tarball, then the lock
# file inside it and the Node and protoc hashes in the manifest; no package's install script runs.

# require_sidecar_contract: what an update is compared with, from this install's RELEASE.
require_sidecar_contract() {
	if ! printf '%s\n' "$REL_SIDECAR_VERSION" | grep -Eqx "$NV_SIDECAR_VERSION_ERE" ||
		! printf '%s\n' "$REL_SIDECAR_PROTOCOL_MAJOR" | grep -Eqx '[0-9]{1,6}' ||
		! printf '%s\n' "$REL_SIDECAR_REQUIRED_CAPABILITIES" | grep -Eqx '[a-z0-9_]+( [a-z0-9_]+)*'; then
		die "$NV_RELEASE_PATH has no valid SIDECAR_VERSION, SIDECAR_PROTOCOL_MAJOR and SIDECAR_REQUIRED_CAPABILITIES, so this Eitri cannot take sidecar updates: a newer Eitri release can"
	fi
}

# bundled_sidecar_present sets NV_BUNDLED_PRESENT: an executable sidecar beside the running script (the
# rule resolve_setup_release uses, never NV_LIB, which on a .deb could name another install) that runs
# --version, or else this RELEASE's <rev7> directory. A sibling has no BUILD, so sidecar_state alone
# would refuse every package that ships one. Sets NV_BUNDLED_BIN to the one it found.
bundled_sidecar_present() {
	NV_BUNDLED_PRESENT=0
	setup_dir
	_bsp_bin=$NV_SETUP_DIR/verdandi-claude-sidecar
	if [ -n "$NV_SETUP_DIR" ] && [ -f "$_bsp_bin" ] && [ -x "$_bsp_bin" ]; then
		if "$_bsp_bin" --version >/dev/null 2>&1; then
			NV_BUNDLED_PRESENT=1
			NV_BUNDLED_BIN=$_bsp_bin
			return 0
		fi
	fi
	sidecar_state "$REL_REV7"
	NV_BUNDLED_PRESENT=$NV_SIDECAR_PRESENT
	NV_BUNDLED_BIN=$NV_SIDECAR_BIN
}

# version_name VALUE sets VN=1 when VALUE is a sidecar version and nothing else. grep reads lines, so a
# value holding a newline would match by its first line.
version_name() {
	VN=0
	case $1 in *"$NV_NL"*) return 0 ;; esac
	if printf '%s\n' "$1" | grep -Eqx "$NV_SIDECAR_VERSION_ERE"; then VN=1; fi
}

# Read only an owned regular selection without other-write permission. A group-writable link
# is refused too: the host can tighten an old regular selection, but never a symlink's target.
# Sets NV_SELECTED_READABLE; NV_SELECTED_REFUSAL keeps the ownership or write-bit refusal for status.
selected_readable() {
	NV_SELECTED_READABLE=0
	NV_SELECTED_REFUSAL=
	_sr_ok=$(find -H "$1" -prune -type f -user "$(id -u)" ! -perm -0002 -print 2>/dev/null) || return 0
	if [ -z "$_sr_ok" ]; then
		_sr_foreign=$(find -H "$1" -prune -type f ! -user "$(id -u)" -print 2>/dev/null) || return 0
		if [ -n "$_sr_foreign" ]; then
			NV_SELECTED_REFUSAL="refused: not yours"
		else
			_sr_writable=$(find -H "$1" -prune -type f -perm -0002 -print 2>/dev/null) || return 0
			if [ -n "$_sr_writable" ]; then NV_SELECTED_REFUSAL="refused: writable by others"; fi
		fi
		return 0
	fi
	if [ -L "$1" ]; then
		_sr_ok=$(find -H "$1" -prune ! -perm -0020 -print 2>/dev/null) || return 0
		if [ -z "$_sr_ok" ]; then
			NV_SELECTED_REFUSAL="refused: writable by others"
			return 0
		fi
	fi
	NV_SELECTED_READABLE=1
}

# read_selected sets NV_SELECTED_VERSION to the version `selected` names, or nothing: absent, or not
# exactly one line of `version=` and a version, is no selection, the same answer a tab gets. Nothing else
# is read from that file. $() would drop every trailing newline, and the x keeps them; a NUL it would
# drop too comes back as a character no version holds. Past 256 bytes the cut leaves no version either:
# none is that long.
read_selected() {
	NV_SELECTED_VERSION=
	selected_readable "$NV_SIDECAR_SELECTED"
	if [ "$NV_SELECTED_READABLE" != 1 ]; then return 0; fi
	_rs=$(head -c 256 -- "$NV_SIDECAR_SELECTED" 2>/dev/null | tr '\000' '?'; printf x)
	_rs=${_rs%x}
	_rs=${_rs%"$NV_NL"}
	case $_rs in version=*) ;; *) return 0 ;; esac
	_rs=${_rs#version=}
	version_name "$_rs"
	if [ "$VN" = 1 ]; then NV_SELECTED_VERSION=$_rs; fi
}

# update_intact VERSION: sidecar_dir_state for updates/VERSION, where it decides whether to build that
# version or to select it. A link there is nothing an update made, so it is no copy of that version.
update_intact() {
	NV_SIDECAR_PRESENT=0
	if [ ! -L "$NV_SIDECAR_UPDATES/$1" ]; then sidecar_dir_state "$NV_SIDECAR_UPDATES/$1" "$1"; fi
}

# selected_replaceable: `selected` is something a rename replaces. mv onto a directory, or onto a link
# to one, puts the file inside it and exits 0, with nothing selected.
selected_replaceable() {
	if [ -d "$NV_SIDECAR_SELECTED" ]; then
		die "$NV_SIDECAR_SELECTED is a directory, or a link to one: this installer never made it. Remove it and re-run"
	fi
}

# write_selected VERSION: select VERSION for new tabs, replacing `selected` in one rename.
write_selected() {
	selected_replaceable
	NV_SELECTED_TMP=$NV_SIDECAR_ROOT/.selected.tmp.$$
	printf 'version=%s\n' "$1" >"$NV_SELECTED_TMP" || die "cannot write $NV_SELECTED_TMP"
	chmod 600 "$NV_SELECTED_TMP" || die "cannot make $NV_SELECTED_TMP private"
	mv -- "$NV_SELECTED_TMP" "$NV_SIDECAR_SELECTED" || die "cannot move $NV_SELECTED_TMP to $NV_SIDECAR_SELECTED"
	NV_SELECTED_TMP=
}

# sidecar_probe PATH: the installed Eitri's own judgement of the sidecar at PATH, on this machine with
# this claude: it starts it as a tab would and exits. Sets SP_OK, and SP_WHY to the reason: on a refusal
# the last line of what it printed, since a sidecar's own stderr comes before it.
sidecar_probe() {
	SP_OK=0
	SP_WHY=
	setup_dir
	if [ -z "$NV_SETUP_DIR" ] || [ ! -x "$NV_SETUP_DIR/shell" ]; then
		SP_WHY="there is no Eitri binary beside $0 to try it with"
		return 0
	fi
	if _sp_out=$("$NV_SETUP_DIR/shell" --sidecar-probe "$1" 2>&1); then
		case $NV_NL$_sp_out in
		*"${NV_NL}ok "*) SP_OK=1 ;;
		*) SP_WHY="the Eitri binary did not say it accepts it" ;;
		esac
		return 0
	fi
	SP_WHY=$(printf '%s\n' "$_sp_out" | sed -n '$p')
	if [ -z "$SP_WHY" ]; then SP_WHY="the Eitri binary gave no reason"; fi
}

# sidecar_preflight: what needs no network and no lock. A fallback must exist before anything is
# selected, and a machine that cannot check a signature goes without an update.
sidecar_preflight() {
	resolve_setup_release
	require_sidecar_contract
	bundled_sidecar_present
	if [ "$NV_BUNDLED_PRESENT" != 1 ]; then
		die "the bundled sidecar is not built yet, and an update needs it as the fallback: run \"eitri setup\" first"
	fi
	if ! command -v ssh-keygen >/dev/null 2>&1; then
		openssh_hint
		die "ssh-keygen was not found, so a sidecar release's signature cannot be checked, and an update is taken signed or not at all. Install OpenSSH's client$OH_HINT and re-run"
	fi
	if [ -z "$OPT_SIGNERS" ]; then
		_spf_text=$(embedded_release_signers)
		if ! printf '%s\n' "$_spf_text" | grep -Eq '^[[:space:]]*[^#[:space:]]'; then
			die "this installer lists no release key, so a sidecar release's signature cannot be checked, and an update is taken signed or not at all"
		fi
	fi
	require_curl
}

# sr_field TEXT KEY ERE sets SRF to KEY's value in SIDECAR-RELEASE's text, refusing a value that does
# not match. Every value has its expression, and is used only after it.
sr_field() {
	SRF=$(kv_value "$1" "$2")
	if ! printf '%s\n' "$SRF" | grep -Eqx "$3"; then
		die "SIDECAR-RELEASE names no valid $2: the release is damaged. Report it at $NV_ISSUES"
	fi
}

# parse_sidecar_release FILE sets the SR_ fields from a checked SIDECAR-RELEASE. Format 1 is additive,
# so an unknown key is ignored, and a release an older installer must not read says so with a new FORMAT.
parse_sidecar_release() {
	_psr=$(cat -- "$1") || die "cannot open $1"
	sr_field "$_psr" FORMAT '[0-9]{1,6}'
	if [ "$SRF" != 1 ]; then die "this release needs a newer Eitri (SIDECAR-RELEASE format $SRF)"; fi
	sr_field "$_psr" SIDECAR_VERSION "$NV_SIDECAR_VERSION_ERE"
	SR_VERSION=$SRF
	sr_field "$_psr" SIDECAR_COMMIT '[0-9a-f]{40}'
	SR_COMMIT=$SRF
	sr_field "$_psr" SIDECAR_SOURCE 'claude-sidecar-[0-9A-Za-z.-]+-source\.tar\.gz'
	SR_SOURCE=$SRF
	if [ "$SR_SOURCE" != "claude-sidecar-$SR_VERSION-source.tar.gz" ]; then
		die "SIDECAR-RELEASE names no valid SIDECAR_SOURCE: it is not claude-sidecar-$SR_VERSION-source.tar.gz, so the release is damaged. Report it at $NV_ISSUES"
	fi
	sr_field "$_psr" PROTOCOL_MAJOR '[0-9]{1,6}'
	SR_PROTOCOL_MAJOR=$SRF
	sr_field "$_psr" PROTOCOL_MINOR '[0-9]{1,6}'
	SR_PROTOCOL_MINOR=$SRF
	sr_field "$_psr" CAPABILITIES '[a-z0-9_]+( [a-z0-9_]+)*'
	SR_CAPABILITIES=$SRF
	sr_field "$_psr" SDK_VERSION '[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?'
	SR_SDK_VERSION=$SRF
	sr_field "$_psr" NODE_VERSION 'v[0-9]+\.[0-9]+\.[0-9]+'
	SR_NODE_VERSION=$SRF
	sr_field "$_psr" NODE_SHA256_linux_x64 '[0-9a-f]{64}'
	SR_NODE_SHA256_LINUX_X64=$SRF
	sr_field "$_psr" NODE_SHA256_linux_arm64 '[0-9a-f]{64}'
	SR_NODE_SHA256_LINUX_ARM64=$SRF
	sr_field "$_psr" GRPC_TOOLS_VERSION '[0-9]+\.[0-9]+\.[0-9]+'
	SR_GRPC_TOOLS_VERSION=$SRF
	sr_field "$_psr" GRPC_TOOLS_SHA256_linux_x64 '[0-9a-f]{64}'
	SR_GRPC_TOOLS_SHA256_LINUX_X64=$SRF
	sr_field "$_psr" GRPC_TOOLS_SHA256_linux_arm64 '[0-9a-f]{64}'
	SR_GRPC_TOOLS_SHA256_LINUX_ARM64=$SRF
	sr_field "$_psr" BUILD_STAMP '[0-9a-f]{16}'
	SR_BUILD_STAMP=$SRF
	sr_field "$_psr" CLI_RANGE '[0-9]+\.[0-9]+\.[0-9]+ [0-9]+\.[0-9]+\.[0-9]+'
	sr_field "$_psr" TESTED_CLI '[0-9]+\.[0-9]+\.[0-9]+( [0-9]+\.[0-9]+\.[0-9]+)*'
}

# sidecar_fetch_manifest: into NV_DL, the release's SHA256SUMS and its signature, then, with the
# signature good, SIDECAR-RELEASE checked against it and parsed. `latest` is GitHub's pointer to the
# newest non-prerelease; sidecar_decide is what keeps a served older one from lowering what runs.
sidecar_fetch_manifest() {
	check_base_url "${OPT_BASE_URL:-$NV_SIDECAR_REPO_BASE}"
	if [ -n "$OPT_VERSION" ]; then
		_sfm_url=$NV_BASE/releases/download/claude-sidecar-v$OPT_VERSION
	else
		_sfm_url=$NV_BASE/releases/latest/download
	fi
	# Nothing at the address is an answer of its own: the network works, and re-running cannot help.
	curl_get "$_sfm_url/SHA256SUMS" "$NV_DL/SHA256SUMS" "$NV_SIDECAR_META_MAX_BYTES"
	if [ "$CG_OK" != 1 ]; then
		case $CG_ERR in
		*'returned error: 404'*)
			if [ -n "$OPT_VERSION" ]; then die "no release claude-sidecar-v$OPT_VERSION at $NV_BASE: eitri sidecar check names the newest"; fi
			die "no sidecar release is published at $NV_BASE, so there is nothing to update to"
			;;
		esac
		die "could not download SHA256SUMS from $_sfm_url/SHA256SUMS: $CG_ERR"
	fi
	fetch "$_sfm_url/SHA256SUMS.sig" "$NV_DL/SHA256SUMS.sig" "SHA256SUMS.sig (a release without a signature is refused)" "$NV_SIDECAR_META_MAX_BYTES"
	verify_signature "$NV_DL/SHA256SUMS" "$NV_DL/SHA256SUMS.sig" "$NV_SIDECAR_NAMESPACE"
	NV_SUMS_TEXT=$(cat -- "$NV_DL/SHA256SUMS") || die "cannot open $NV_DL/SHA256SUMS"
	fetch "$_sfm_url/SIDECAR-RELEASE" "$NV_DL/SIDECAR-RELEASE.part" SIDECAR-RELEASE "$NV_SIDECAR_META_MAX_BYTES"
	verify_file "$NV_DL/SIDECAR-RELEASE.part" SIDECAR-RELEASE
	mv -- "$NV_DL/SIDECAR-RELEASE.part" "$NV_DL/SIDECAR-RELEASE" || die "cannot rename $NV_DL/SIDECAR-RELEASE.part"
	parse_sidecar_release "$NV_DL/SIDECAR-RELEASE"
}

# sidecar_decide: whether this release is one to take, before anything is spent on it. Sets SD_OK,
# and SD_WHY to why not; SD_CURRENT when that is only because new tabs already use it, which is no
# failure. With no --version the release must also be newer than `selected` and every
# complete update, so a served older release cannot lower what runs; naming a version is how a person
# goes down on purpose.
sidecar_decide() {
	SD_OK=0
	SD_WHY=
	SD_CURRENT=0
	if [ -n "$OPT_VERSION" ] && [ "$OPT_VERSION" != "$SR_VERSION" ]; then
		SD_WHY="asked for $OPT_VERSION, but this release says it is $SR_VERSION"
		return 0
	fi
	if [ "$SR_PROTOCOL_MAJOR" -gt "$REL_SIDECAR_PROTOCOL_MAJOR" ]; then
		SD_WHY="this release needs a newer Eitri: it speaks protocol major $SR_PROTOCOL_MAJOR, this Eitri $REL_SIDECAR_PROTOCOL_MAJOR"
		return 0
	fi
	if [ "$SR_PROTOCOL_MAJOR" != "$REL_SIDECAR_PROTOCOL_MAJOR" ]; then
		SD_WHY="this release speaks protocol major $SR_PROTOCOL_MAJOR, and this Eitri needs $REL_SIDECAR_PROTOCOL_MAJOR"
		return 0
	fi
	for _sd_c in $REL_SIDECAR_REQUIRED_CAPABILITIES; do
		case " $SR_CAPABILITIES " in
		*" $_sd_c "*) ;;
		*)
			SD_WHY="this release does not offer $_sd_c, which this Eitri needs of an update"
			return 0
			;;
		esac
	done
	_sd_cmp=$(semver_cmp "$SR_VERSION" "$REL_SIDECAR_VERSION")
	if [ "$_sd_cmp" != 1 ]; then
		SD_WHY="release $SR_VERSION is not newer than the bundled sidecar ($REL_SIDECAR_VERSION)"
		return 0
	fi
	case $SR_VERSION in
	*-rc.*)
		if [ -z "$OPT_VERSION" ]; then
			SD_WHY="release $SR_VERSION is a release candidate: name it with --version to take it"
			return 0
		fi
		;;
	esac
	update_intact "$SR_VERSION"
	if [ "$NV_SIDECAR_PRESENT" = 1 ]; then
		read_selected
		if [ "$NV_SELECTED_VERSION" = "$SR_VERSION" ]; then
			SD_CURRENT=1
			SD_WHY="$SR_VERSION is already what new tabs use"
		else
			SD_WHY="already built: eitri sidecar use $SR_VERSION"
		fi
		return 0
	fi
	if [ -z "$OPT_VERSION" ]; then
		read_selected
		_sd_floor=$NV_SELECTED_VERSION
		_sd_held=selected
		for _sd_d in "$NV_SIDECAR_UPDATES"/*; do
			if [ ! -d "$_sd_d" ] || [ -L "$_sd_d" ]; then continue; fi
			_sd_n=${_sd_d##*/}
			version_name "$_sd_n"
			if [ "$VN" != 1 ]; then continue; fi
			sidecar_dir_state "$_sd_d" "$_sd_n"
			if [ "$NV_SIDECAR_PRESENT" != 1 ]; then continue; fi
			if [ -n "$_sd_floor" ]; then
				_sd_cmp=$(semver_cmp "$_sd_n" "$_sd_floor")
				if [ "$_sd_cmp" != 1 ]; then continue; fi
			fi
			_sd_floor=$_sd_n
			_sd_held=kept
		done
		if [ -n "$_sd_floor" ]; then
			_sd_cmp=$(semver_cmp "$SR_VERSION" "$_sd_floor")
			if [ "$_sd_cmp" != 1 ]; then
				SD_WHY="release $SR_VERSION is not newer than $_sd_floor, which is $_sd_held here: name it with --version to take it anyway"
				return 0
			fi
		fi
	fi
	SD_OK=1
}

# sidecar_download_protoc WORKDIR: the protoc tarball for this machine, checked against the manifest.
# Sets SDP_TARBALL.
sidecar_download_protoc() {
	case $NV_NODE_ARCH in
	x64) _sdp_sha=$SR_GRPC_TOOLS_SHA256_LINUX_X64 ;;
	arm64) _sdp_sha=$SR_GRPC_TOOLS_SHA256_LINUX_ARM64 ;;
	esac
	_sdp_name=linux-$NV_NODE_ARCH.tar.gz
	_sdp_base=$(grpc_dist_base)
	_sdp_url=$_sdp_base/grpc-tools/v$SR_GRPC_TOOLS_VERSION/$_sdp_name
	_sdp_out=$1/$_sdp_name
	curl_get "$_sdp_url" "$_sdp_out.part" "$NV_SIDECAR_ARCHIVE_MAX_BYTES"
	if [ "$CG_OK" != 1 ]; then die "could not download the pinned protoc ($_sdp_url): $CG_ERR"; fi
	_sdp_have=$(file_sha256 "$_sdp_out.part")
	if [ "$_sdp_have" != "$_sdp_sha" ]; then
		rm -f -- "$_sdp_out.part" || :
		die "checksum mismatch for $_sdp_name: expected $_sdp_sha, got $_sdp_have. Refusing to build the sidecar with an unverified protoc; re-run, and report it at $NV_ISSUES if it persists"
	fi
	mv -- "$_sdp_out.part" "$_sdp_out" || die "cannot rename $_sdp_out.part"
	SDP_TARBALL=$_sdp_out
}

# sidecar_fact_is FACTS KEY WANT: the built artifact's KEY must be what the signed manifest says.
sidecar_fact_is() {
	_sfi=$(kv_value "$1" "$2")
	if [ "$_sfi" != "$3" ]; then
		die "the built sidecar says $2=$_sfi, and the signed release says $3: a build that differs from what was released is not used. Report it at $NV_ISSUES"
	fi
}

# sidecar_accept_release ARTIFACT: the built artifact is what the signed manifest describes, read with
# the pinned Node from the artifact's own --version --json.
sidecar_accept_release() {
	_sar=$("$1" --version --json 2>/dev/null | "$SB_NODE_BIN/node" -e "$NV_SIDECAR_FACTS_JS") ||
		die "the built sidecar ($1) could not report its own --version --json: report it at $NV_ISSUES"
	sidecar_fact_is "$_sar" sidecar_version "$SR_VERSION"
	sidecar_fact_is "$_sar" protocol "$SR_PROTOCOL_MAJOR.$SR_PROTOCOL_MINOR"
	sidecar_fact_is "$_sar" node "$SR_NODE_VERSION"
	sidecar_fact_is "$_sar" sdk_version "$SR_SDK_VERSION"
	sidecar_fact_is "$_sar" build_stamp "$SR_BUILD_STAMP"
	sidecar_fact_is "$_sar" executables '["host_cli"]'
}

# sidecar_install_update: the accepted artifact into updates/<version>/ and selected. BUILD goes in
# last, and the directory lands by one rename, so no reader finds half of it; a name already there is
# moved aside first, because mv onto a directory nests and exits 0. A run killed after the directory
# lands has selected nothing, and the next one finds the build complete. Pruning comes after the
# selection, so nothing it fails at can undo an update that already stands.
sidecar_install_update() {
	if [ ! -d "$NV_SIDECAR_UPDATES" ]; then
		mkdir -p -- "$NV_SIDECAR_UPDATES" || die "cannot create $NV_SIDECAR_UPDATES"
		NV_UPDATES_CREATED=1
	fi
	NV_UPDATE_TMP=$NV_SIDECAR_UPDATES/.$SR_VERSION.tmp.$$
	mkdir -- "$NV_UPDATE_TMP" || die "cannot create $NV_UPDATE_TMP"
	cp -- "$SB_ARTIFACT" "$NV_UPDATE_TMP/verdandi-claude-sidecar" || die "cannot write $NV_UPDATE_TMP/verdandi-claude-sidecar"
	chmod 0755 "$NV_UPDATE_TMP/verdandi-claude-sidecar" || die "cannot chmod $NV_UPDATE_TMP/verdandi-claude-sidecar"
	if [ -f "$SB_SDK_LICENSE" ]; then cp -- "$SB_SDK_LICENSE" "$NV_UPDATE_TMP/LICENSE.md" || die "cannot write $NV_UPDATE_TMP/LICENSE.md"; fi
	if [ -f "$SB_NODE_LICENSE" ]; then cp -- "$SB_NODE_LICENSE" "$NV_UPDATE_TMP/NODE-LICENSE" || die "cannot write $NV_UPDATE_TMP/NODE-LICENSE"; fi
	printf 'VERDANDI_REV=%s\nSIDECAR_VERSION_LINE=%s\nBUILT_AT=%s\n' \
		"$SR_COMMIT" "$SB_VERSION_LINE" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >"$NV_UPDATE_TMP/BUILD" ||
		die "cannot write $NV_UPDATE_TMP/BUILD"
	NV_UPDATE_DEST=$NV_SIDECAR_UPDATES/$SR_VERSION
	if [ -e "$NV_UPDATE_DEST" ] || [ -L "$NV_UPDATE_DEST" ]; then
		NV_UPDATE_ASIDE=$NV_UPDATE_TMP.aside
		mv -- "$NV_UPDATE_DEST" "$NV_UPDATE_ASIDE" || die "cannot move $NV_UPDATE_DEST aside"
	fi
	mv -- "$NV_UPDATE_TMP" "$NV_UPDATE_DEST" || die "cannot move the new sidecar to $NV_UPDATE_DEST"
	NV_UPDATE_TMP=
	if [ -n "$NV_UPDATE_ASIDE" ]; then
		rm -rf -- "$NV_UPDATE_ASIDE" || die "cannot remove $NV_UPDATE_ASIDE"
		NV_UPDATE_ASIDE=
	fi
	read_selected
	_siu_previous=$NV_SELECTED_VERSION
	write_selected "$SR_VERSION"
	prune_updates "$SR_VERSION" "$_siu_previous"
}

# prune_updates SELECTED PREVIOUS: updates/ keeps those two. A real directory named exactly like a
# version goes, as do the temporary names a killed run left; a link or any other name is not this
# installer's. Runs under the lock, so no temporary name belongs to a live run. A sidecar pruned under
# a running tab stays mapped, and a spawn that read `selected` just before finds no file and takes the
# bundled one.
prune_updates() {
	sidecar_root_ours
	if [ "$SRO" != 1 ]; then return 0; fi
	if [ -L "$NV_SIDECAR_UPDATES" ]; then
		warn "$NV_SIDECAR_UPDATES is a symlink, so nothing behind it was removed: this installer never made that link"
		return 0
	fi
	for _pu_d in "$NV_SIDECAR_UPDATES"/*; do
		if [ ! -d "$_pu_d" ] || [ -L "$_pu_d" ]; then continue; fi
		_pu_n=${_pu_d##*/}
		version_name "$_pu_n"
		if [ "$VN" != 1 ] || [ "$_pu_n" = "$1" ] || [ "$_pu_n" = "$2" ]; then continue; fi
		if ! rm -rf -- "$_pu_d"; then warn "could not remove $_pu_d"; fi
	done
	for _pu_d in "$NV_SIDECAR_UPDATES"/.*.tmp.* "$NV_SIDECAR_ROOT"/.selected.tmp.*; do
		if [ ! -e "$_pu_d" ] && [ ! -L "$_pu_d" ]; then continue; fi
		if ! rm -rf -- "$_pu_d"; then warn "could not remove $_pu_d"; fi
	done
}

# do_sidecar_update: --sidecar-update. Runs under the lock with a download directory of its own:
# recover_interrupted repairs $NV_LIB, which this path has no business with.
do_sidecar_update() {
	NV_LEAVE_LIB=1
	sidecar_preflight
	acquire_lock
	selected_replaceable
	NV_DL=$NV_CACHE_NV/sidecar-update
	rm -rf -- "$NV_DL" || die "cannot remove $NV_DL, left by an interrupted run"
	mkdir -m 0700 -- "$NV_DL" || die "cannot create $NV_DL"
	prepare_signers
	sidecar_fetch_manifest
	sidecar_decide
	if [ "$SD_CURRENT" = 1 ]; then
		say "$SD_WHY"
		return 0
	fi
	if [ "$SD_OK" != 1 ]; then die "$SD_WHY"; fi

	# From here on every file comes from the release's own tag, so `latest` moving cannot mix two.
	NV_REL_URL=$NV_BASE/releases/download/claude-sidecar-v$SR_VERSION
	download_verified "$SR_SOURCE" "$NV_SIDECAR_ARCHIVE_MAX_BYTES"
	_dsu_work=$NV_DL/build
	mkdir -- "$_dsu_work" || die "cannot create $_dsu_work"
	say "downloading Node and the Claude Agent SDK, then building the sidecar (1-3 min)"
	# The helpers read these. A release may pin another Node than the one this Eitri bundles.
	REL_NODE_VERSION=$SR_NODE_VERSION
	REL_NODE_SHA256_LINUX_X64=$SR_NODE_SHA256_LINUX_X64
	REL_NODE_SHA256_LINUX_ARM64=$SR_NODE_SHA256_LINUX_ARM64
	sidecar_download_node "$_dsu_work" 0
	sidecar_download_protoc "$_dsu_work"
	check_free_space "$_dsu_work" "$NV_SIDECAR_UPDATE_MIN_KIB" "building a sidecar update needs about 1.2 GiB while it runs (a measured build peaked near 1.15 GiB)"
	SB_NPM_CI_FLAGS=--ignore-scripts
	SB_PROTOC_TARBALL=$SDP_TARBALL
	sidecar_build_core "$_dsu_work" "$SDN_TARBALL" "$NV_DL/$SR_SOURCE"
	sidecar_accept_release "$SB_ARTIFACT"
	sidecar_probe "$SB_ARTIFACT"
	if [ "$SP_OK" != 1 ]; then die "built, but it will not start here: $SP_WHY"; fi
	sidecar_install_update
	say "sidecar $SR_VERSION built, and this Eitri accepts it."
	say "Open tabs keep the sidecar they started with. New Claude tabs use $SR_VERSION."
	say "To go back: eitri sidecar use bundled"
}

# do_sidecar_check: --sidecar-check. The manifest's half of an update, in a directory of its own and
# without the lock, so it runs beside an update and changes nothing. It prints only fields that
# passed the signature. A release it would not take is an answer, not a failure.
do_sidecar_check() {
	sidecar_preflight
	check_cache_root
	NV_SIDECAR_CHECK_DIR=$(mktemp -d "$NV_CACHE_NV/sidecar-check.XXXXXX") || die "cannot create a directory under $NV_CACHE_NV"
	NV_DL=$NV_SIDECAR_CHECK_DIR
	prepare_signers
	sidecar_fetch_manifest
	sidecar_decide
	_dsc_run=$REL_SIDECAR_VERSION
	read_selected
	if [ -n "$NV_SELECTED_VERSION" ]; then
		_dsc_cmp=$(semver_cmp "$NV_SELECTED_VERSION" "$REL_SIDECAR_VERSION")
		sidecar_dir_state "$NV_SIDECAR_UPDATES/$NV_SELECTED_VERSION" "$NV_SELECTED_VERSION"
		if [ "$_dsc_cmp" = 1 ] && [ "$NV_SIDECAR_PRESENT" = 1 ]; then _dsc_run=$NV_SELECTED_VERSION; fi
	fi
	say "newest release: $SR_VERSION (protocol $SR_PROTOCOL_MAJOR.$SR_PROTOCOL_MINOR, Agent SDK $SR_SDK_VERSION). You run $_dsc_run."
	if [ "$SD_OK" = 1 ]; then
		say "eitri sidecar update takes it: 1-3 minutes, about 1.2 GiB free, and network access to nodejs.org, npm and grpc.io."
	else
		say "$SD_WHY"
	fi
}

# sidecar_facts BIN sets SF_PROTOCOL, SF_SDK and SF_TESTED from the one line of BIN's --version --json,
# each empty when it does not say. This path has no Node, and sed is enough for what JSON.stringify
# writes. SF_TESTED keeps its quotes, so that one version is not found inside another.
sidecar_facts() {
	_sf=$("$1" --version --json 2>/dev/null | head -n 1) || _sf=
	SF_PROTOCOL=$(printf '%s\n' "$_sf" | sed -n 's/.*"protocol":{"major":\([0-9]*\),"minor":\([0-9]*\)}.*/\1.\2/p')
	SF_SDK=$(printf '%s\n' "$_sf" | sed -n 's/.*"sdk_version":"\([^"]*\)".*/\1/p')
	SF_TESTED=$(printf '%s\n' "$_sf" | sed -n 's/.*"tested_cli_versions":\[\([^]]*\)\].*/\1/p')
}

# status_row LABEL TEXT: one line of `eitri sidecar status`.
status_row() {
	printf '%-22s %s\n' "$1" "$2"
}

# status_selected sets SS_UPDATE to the update a new tab starts, by the rule a tab applies (strictly
# newer than bundled, with its binary and BUILD; nothing is compared at a spawn), and SS_NOTE to what
# there is to say about `selected`.
status_selected() {
	SS_UPDATE=
	SS_NOTE=
	if [ -z "$NV_SELECTED_VERSION" ]; then
		if [ -n "${NV_SELECTED_REFUSAL-}" ]; then
			SS_NOTE=$NV_SELECTED_REFUSAL
		elif [ -e "$NV_SIDECAR_SELECTED" ] || [ -L "$NV_SIDECAR_SELECTED" ]; then
			SS_NOTE="names no version, so unused"
		fi
		return 0
	fi
	_ss_cmp=$(semver_cmp "$NV_SELECTED_VERSION" "$REL_SIDECAR_VERSION")
	case $_ss_cmp in
	-1)
		SS_NOTE="$NV_SELECTED_VERSION, older than the bundled sidecar, so unused"
		return 0
		;;
	0)
		SS_NOTE="$NV_SELECTED_VERSION, the same as the bundled sidecar, so unused"
		return 0
		;;
	esac
	_ss_dir=$NV_SIDECAR_UPDATES/$NV_SELECTED_VERSION
	if [ ! -f "$_ss_dir/verdandi-claude-sidecar" ] || [ ! -f "$_ss_dir/BUILD" ]; then
		SS_NOTE="$NV_SELECTED_VERSION, not complete, so unused"
		return 0
	fi
	SS_UPDATE=$NV_SELECTED_VERSION
	sidecar_dir_state "$_ss_dir" "$NV_SELECTED_VERSION"
	if [ "$NV_SIDECAR_PRESENT" != 1 ]; then SS_NOTE="$NV_SELECTED_VERSION, which does not match its BUILD file"; fi
}

# status_named sets SN_TEXT and SN_BIN when the environment names a sidecar, which wins over all of
# this: a tab takes it whatever is selected, and blank is not named.
status_named() {
	SN_TEXT=
	SN_BIN=
	case ${EITRI_SIDECAR_BINARY-} in
	*[![:space:]]*)
		SN_TEXT="$EITRI_SIDECAR_BINARY, named by EITRI_SIDECAR_BINARY"
		SN_BIN=$EITRI_SIDECAR_BINARY
		return 0
		;;
	esac
	case ${EITRI_VERDANDI_CHECKOUT-} in
	*[![:space:]]*) SN_TEXT="the checkout $EITRI_VERDANDI_CHECKOUT, named by EITRI_VERDANDI_CHECKOUT" ;;
	esac
}

# do_sidecar_status: --sidecar-status. Reads files and runs --version: no lock, so it answers during an
# update, and no network.
do_sidecar_status() {
	resolve_setup_release
	require_sidecar_contract
	bundled_sidecar_present
	read_selected
	status_selected
	status_named
	SF_PROTOCOL='' SF_SDK='' SF_TESTED=''
	_dss_bundled=
	if [ "$NV_BUNDLED_PRESENT" = 1 ]; then
		sidecar_facts "$NV_BUNDLED_BIN"
		if [ -n "$SF_PROTOCOL" ]; then _dss_bundled=" (protocol $SF_PROTOCOL)"; fi
	fi
	if [ -n "$SN_TEXT" ]; then
		SF_TESTED=
		if [ -n "$SN_BIN" ] && [ -f "$SN_BIN" ] && [ -x "$SN_BIN" ]; then sidecar_facts "$SN_BIN"; fi
		_dss_run=$SN_TEXT
	elif [ -n "$SS_UPDATE" ]; then
		sidecar_facts "$NV_SIDECAR_UPDATES/$SS_UPDATE/verdandi-claude-sidecar"
		_dss_run="update $SS_UPDATE"
	elif [ "$NV_BUNDLED_PRESENT" = 1 ]; then
		_dss_run="bundled $REL_SIDECAR_VERSION"
	else
		_dss_run="nothing yet, run eitri setup"
	fi
	if [ -z "$SN_TEXT" ] && [ -n "$SF_PROTOCOL" ] && [ -n "$SF_SDK" ]; then
		_dss_run="$_dss_run (protocol $SF_PROTOCOL, Agent SDK $SF_SDK)"
	fi
	status_row "running for new tabs" "$_dss_run"
	if [ -n "$SS_NOTE" ]; then status_row selected "$SS_NOTE"; fi
	if [ "$NV_BUNDLED_PRESENT" != 1 ]; then
		printf 'bundled %s: not built yet, run eitri setup\n' "$REL_SIDECAR_VERSION"
	elif [ -n "$SS_UPDATE" ] && [ -z "$SN_TEXT" ]; then
		status_row "bundled with Eitri" "$REL_SIDECAR_VERSION$_dss_bundled, the fallback"
	else
		status_row "bundled with Eitri" "$REL_SIDECAR_VERSION$_dss_bundled"
	fi
	_dss_kept=
	for _dss_d in "$NV_SIDECAR_UPDATES"/*; do
		if [ ! -d "$_dss_d" ] || [ -L "$_dss_d" ]; then continue; fi
		_dss_n=${_dss_d##*/}
		version_name "$_dss_n"
		if [ "$VN" != 1 ] || [ "$_dss_n" = "$NV_SELECTED_VERSION" ]; then continue; fi
		sidecar_dir_state "$_dss_d" "$_dss_n"
		if [ "$NV_SIDECAR_PRESENT" = 1 ]; then _dss_kept="$_dss_kept $_dss_n"; fi
	done
	if [ -n "$_dss_kept" ]; then status_row kept "${_dss_kept# }"; fi
	if ! command -v claude >/dev/null 2>&1; then
		status_row "claude on this machine" "not found"
		return 0
	fi
	_dss_c=$(claude --version 2>/dev/null | head -n 1) || _dss_c=
	_dss_c=${_dss_c%% *}
	if [ -z "$_dss_c" ] || [ -z "$SF_TESTED" ]; then
		status_row "claude on this machine" "${_dss_c:-(version unknown)}"
		return 0
	fi
	case $SF_TESTED in
	*"\"$_dss_c\""*) status_row "claude on this machine" "$_dss_c, tested with this sidecar" ;;
	*) status_row "claude on this machine" "$_dss_c, not among the versions this sidecar was tested with" ;;
	esac
}

# do_sidecar_use: --sidecar-use bundled|VERSION. Takes the lock, so it cannot race an update's write of
# `selected`. No network. It does not repeat an update's comparison with the signed manifest: neither
# that nor the pinned Node survives the build, and a file of this uid gets nothing stronger from a
# copy of this uid. What it can ask of a build is that the binary is the one BUILD names, that it is
# above what Eitri bundles, and that this Eitri accepts it now. A version reaches a path only once it
# has parsed.
do_sidecar_use() {
	NV_LEAVE_LIB=1
	resolve_setup_release
	require_sidecar_contract
	_dsuse=$OPT_SIDECAR_USE
	if [ "$_dsuse" != bundled ]; then
		version_name "$_dsuse"
		if [ "$VN" != 1 ]; then die "--sidecar-use: expected bundled or a version such as 0.3.2"; fi
	fi
	acquire_lock
	bundled_sidecar_present
	if [ "$_dsuse" = bundled ]; then
		sidecar_use_bundled
		return 0
	fi
	_dsuse_cmp=$(semver_cmp "$_dsuse" "$REL_SIDECAR_VERSION")
	if [ "$_dsuse_cmp" != 1 ]; then
		die "update $_dsuse is not newer than the bundled sidecar ($REL_SIDECAR_VERSION), and a sidecar older than the bundled one is never used"
	fi
	update_intact "$_dsuse"
	if [ "$NV_SIDECAR_PRESENT" != 1 ]; then
		die "update $_dsuse is not built here, or not all of it is: eitri sidecar update --version $_dsuse builds it"
	fi
	sidecar_probe "$NV_SIDECAR_BIN"
	if [ "$SP_OK" != 1 ]; then die "update $_dsuse will not start here: $SP_WHY"; fi
	write_selected "$_dsuse"
	say "New Claude tabs use $_dsuse. Open tabs keep the sidecar they started with."
	say "To go back: eitri sidecar use bundled"
	if [ "$NV_BUNDLED_PRESENT" != 1 ]; then
		say "The bundled sidecar is not built yet, so nothing stands behind it: run eitri setup"
	fi
}

# sidecar_use_bundled: `selected` goes. A link there goes as a link. A file that names nothing goes
# the same way, since going back is what a person asks for when something is wrong with it.
sidecar_use_bundled() {
	if [ -d "$NV_SIDECAR_SELECTED" ] && [ ! -L "$NV_SIDECAR_SELECTED" ]; then
		die "$NV_SIDECAR_SELECTED is a directory: this installer never made it. Remove it and re-run"
	fi
	read_selected
	rm -f -- "$NV_SIDECAR_SELECTED" || die "cannot remove $NV_SIDECAR_SELECTED"
	_sub_built=
	if [ -n "$NV_SELECTED_VERSION" ]; then
		sidecar_dir_state "$NV_SIDECAR_UPDATES/$NV_SELECTED_VERSION" "$NV_SELECTED_VERSION"
		if [ "$NV_SIDECAR_PRESENT" = 1 ]; then
			_sub_built=" Update $NV_SELECTED_VERSION stays built: eitri sidecar use $NV_SELECTED_VERSION"
		fi
	fi
	if [ "$NV_BUNDLED_PRESENT" = 1 ]; then
		say "New Claude tabs use the bundled sidecar $REL_SIDECAR_VERSION.$_sub_built"
	else
		say "The bundled sidecar $REL_SIDECAR_VERSION is not built yet: run eitri setup.$_sub_built"
	fi
}

# ---------------------------------------------------------------------------------------------
# Building from source

# build_hint: the per-distro build-dependency command, the twin of
# runtime_hint's "runtime" column.
build_hint() {
	case $OS_FAMILY in
	debian) printf '%s\n' "install them with: sudo apt install build-essential clang pkg-config libgtk-4-dev libwebkitgtk-6.0-dev protobuf-compiler git" ;;
	fedora) printf '%s\n' "install them with: sudo dnf install gcc clang pkgconf gtk4-devel webkitgtk6.0-devel protobuf-compiler git" ;;
	arch) printf '%s\n' "install them with: sudo pacman -S base-devel clang gtk4 webkitgtk-6.0 protobuf git" ;;
	opensuse) printf '%s\n' "the exact openSUSE package names are unverified: you will need a C toolchain, pkg-config, GTK 4 and WebKitGTK 6.0 development headers, protobuf-compiler and git" ;;
	*) printf '%s\n' "you will need a C toolchain, pkg-config, GTK 4 and WebKitGTK 6.0 development headers (dev packages), protobuf-compiler and git" ;;
	esac
}

# check_build_tools: the build-tools row of the system checks. Refuses with the distro hint (and,
# for rustc/cargo, the rustup one-liner -- printed, never run) rather than running a package manager.
check_build_tools() {
	read_os_release
	_cbt_hint=$(build_hint)
	_cbt_rustup='curl --proto '"'"'=https'"'"' --tlsv1.2 -sSf https://sh.rustup.rs | sh'
	if ! command -v cargo >/dev/null 2>&1 || ! command -v rustc >/dev/null 2>&1; then
		die "building from source needs cargo and rustc $NV_MIN_RUSTC or newer: $_cbt_hint. If rustup is not installed: $_cbt_rustup"
	fi
	_cbt_rv=$(rustc --version 2>/dev/null | awk '{ print $2 }') || _cbt_rv=
	_cbt_rv=${_cbt_rv%%-*}
	if [ -n "$_cbt_rv" ]; then
		_cbt_c=$(dotted_cmp "$_cbt_rv" "$NV_MIN_RUSTC")
		if [ "$_cbt_c" = -1 ]; then
			die "building from source needs rustc $NV_MIN_RUSTC or newer, and this is $_cbt_rv: rustup update, or $_cbt_hint"
		fi
	fi
	if ! command -v cc >/dev/null 2>&1 && ! command -v gcc >/dev/null 2>&1 && ! command -v clang >/dev/null 2>&1; then
		die "building from source needs a C compiler (cc, gcc or clang): $_cbt_hint"
	fi
	if ! command -v pkg-config >/dev/null 2>&1; then
		die "building from source needs pkg-config: $_cbt_hint"
	fi
	if ! pkg-config --exists gtk4 2>/dev/null; then
		die "building from source needs GTK 4 development files (pkg-config found no gtk4.pc): $_cbt_hint"
	fi
	if ! pkg-config --exists webkitgtk-6.0 2>/dev/null; then
		die "building from source needs WebKitGTK 6.0 development files (pkg-config found no webkitgtk-6.0.pc): $_cbt_hint"
	fi
	if ! command -v protoc >/dev/null 2>&1; then
		die "building from source needs protoc (protobuf-compiler): $_cbt_hint"
	fi
	if ! command -v git >/dev/null 2>&1; then
		die "building from source needs git: $_cbt_hint"
	fi
	say "build tools: ok"
}

# eitri_repo_url / verdandi_repo_url: the public repositories --from-source clones. Test-only
# overrides (matched by run-in-env.sh's EITRI_INSTALL_TEST_* allowlist pattern already, no new
# entry needed there) stand in for a local bare repo fixture; production always uses the real URL.
eitri_repo_url() {
	if [ "$NV_TEST_MODE" = 1 ] && [ -n "${EITRI_INSTALL_TEST_REPO_URL-}" ]; then
		printf '%s\n' "$EITRI_INSTALL_TEST_REPO_URL"
	else
		printf '%s\n' "$NV_EITRI_REPO_URL"
	fi
}
verdandi_repo_url() {
	if [ "$NV_TEST_MODE" = 1 ] && [ -n "${EITRI_INSTALL_TEST_VERDANDI_REPO_URL-}" ]; then
		printf '%s\n' "$EITRI_INSTALL_TEST_VERDANDI_REPO_URL"
	else
		printf '%s\n' "$NV_VERDANDI_REPO_URL"
	fi
}

# checkout_version DIR reads shell/Cargo.toml's quoted version or the root workspace.package
# version when inherited. Recognize both version.workspace = true and version = { workspace = true }:
# a grep matching only version followed by = misses the dotted form before dispatch can inspect it.
# Call as a plain statement because this can die (rule 2).
checkout_version() {
	_cv_shell=$1/shell/Cargo.toml
	if [ ! -f "$_cv_shell" ]; then die "$1 has no shell/Cargo.toml: not an Eitri checkout"; fi
	_cv_line=$(grep -E '^version[[:space:]]*=|^version\.workspace[[:space:]]*=' "$_cv_shell" | head -n 1) || _cv_line=
	case $_cv_line in
	*workspace*) _cv_line=$(grep -E '^version[[:space:]]*=' "$1/Cargo.toml" | head -n 1) || _cv_line= ;;
	esac
	CV_VERSION=$(printf '%s\n' "$_cv_line" | sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p')
	if ! printf '%s\n' "$CV_VERSION" | grep -Eqx "$NV_VERSION_ERE"; then
		die "could not determine a version from $1 (shell/Cargo.toml, or [workspace.package] in its root Cargo.toml): not an Eitri checkout, or its version field is not a plain quoted string"
	fi
}

# lockfile_rev DIR PACKAGE: the 40-hex commit DIR/Cargo.lock's `source = "git+...#<sha>"` line names
# for PACKAGE -- the same technique shell/build.rs uses for its own --version line,
# reproduced here because this runs before any binary of DIR's own tree exists to ask. A plain
# statement (rule 2: this can die).
lockfile_rev() {
	_lr_lock=$1/Cargo.lock
	if [ ! -f "$_lr_lock" ]; then die "$1 has no Cargo.lock: not an Eitri checkout, or it has not been resolved (run cargo generate-lockfile first)"; fi
	_lr_rev=$(awk -v pkg="$2" '
		$0 == "name = \"" pkg "\"" { want = 1; next }
		want && /^source = / { sub(/^source = "/, ""); sub(/"$/, ""); print; exit }
		/^\[\[package\]\]/ { want = 0 }
	' "$_lr_lock") || _lr_rev=
	case $_lr_rev in
	*"#"*) LR_REV=${_lr_rev##*#} ;;
	*) LR_REV= ;;
	esac
	if ! printf '%s\n' "$LR_REV" | grep -Eqx '[0-9a-f]{40}'; then
		die "could not determine $2's commit from $_lr_lock: not an Eitri checkout with a git dependency on $2, or Cargo.lock is stale"
	fi
}

# checkout_fork_rev DIR: sets CFR_REV to the Neovide fork commit a --from-source --checkout build of DIR
# links. Private trees pin a git dependency whose Cargo.lock source line records the commit;
# the public export uses a neovide/ submodule path dependency with no source line. Read that
# gitlink with git ls-tree HEAD neovide and require the checkout to match: the build compiles
# whatever the submodule holds, so a mismatch would make RELEASE lie. shell/build.rs resolves
# its version's fork commit in the same order. Call as a plain statement (rule 2).
checkout_fork_rev() {
	_cfr_dir=$1
	_cfr_lock=$_cfr_dir/Cargo.lock
	if [ ! -f "$_cfr_lock" ]; then die "$_cfr_dir has no Cargo.lock: not an Eitri checkout, or it has not been resolved (run cargo generate-lockfile first)"; fi
	_cfr_src=$(awk '
		$0 == "name = \"neovide\"" { want = 1; next }
		want && /^source = / { sub(/^source = "/, ""); sub(/"$/, ""); print; exit }
		/^\[\[package\]\]/ { want = 0 }
	' "$_cfr_lock") || _cfr_src=
	if [ -n "$_cfr_src" ]; then
		lockfile_rev "$_cfr_dir" neovide
		CFR_REV=$LR_REV
		return 0
	fi
	# The public shape: no source line. The gitlink HEAD records for neovide/ ...
	_cfr_link=$(git -C "$_cfr_dir" ls-tree HEAD neovide 2>/dev/null) || _cfr_link=
	case $_cfr_link in
	"160000 commit "*) CFR_REV=$(printf '%s\n' "$_cfr_link" | awk '{ print $3 }') ;;
	*) die "could not determine neovide's commit in $_cfr_dir: Cargo.lock names no git source for it and HEAD records no neovide submodule -- not an Eitri checkout" ;;
	esac
	if ! printf '%s\n' "$CFR_REV" | grep -Eqx '[0-9a-f]{40}'; then
		die "the neovide submodule commit HEAD records in $_cfr_dir is not 40 hex: $CFR_REV"
	fi
	# ... and the checkout the build will actually compile must be at it.
	# Only a real checkout of its own: an uninitialised submodule leaves an empty neovide/, where
	# `git -C neovide rev-parse HEAD` would climb to the enclosing repository and answer with ITS commit
	# (shell/build.rs guards the same way, with --show-toplevel).
	_cfr_head=
	if [ -e "$_cfr_dir/neovide/.git" ]; then
		_cfr_head=$(git -C "$_cfr_dir/neovide" rev-parse HEAD 2>/dev/null) || _cfr_head=
	fi
	if [ -z "$_cfr_head" ]; then
		die "$_cfr_dir/neovide is not checked out (the fork is a git submodule there): run 'git -C $_cfr_dir submodule update --init neovide' and try again"
	fi
	if [ "$_cfr_head" != "$CFR_REV" ]; then
		die "$_cfr_dir/neovide is at $_cfr_head, but HEAD records $CFR_REV for it: run 'git -C $_cfr_dir submodule update neovide' (or commit the submodule change) and try again"
	fi
}

# agent_toml_verdandi DIR: sets AT_GIT_URL and AT_REV from DIR's Verdandi pin: the root Cargo.toml's
# claude-runtime-protocol line (its [workspace.dependencies], which every crate takes the pin from), or
# agent/Cargo.toml's in a checkout from before the pin moved there. Asserts AT_REV is 40 hex (a 7-char
# rev refuses) -- a synthesized RELEASE has nothing else to check VERDANDI_REV against, so a short
# prefix here is not enough to trust. A plain statement (rule 2: this can die).
agent_toml_verdandi() {
	_atv_file=$1/Cargo.toml
	_atv_line=
	if [ -f "$_atv_file" ]; then
		_atv_line=$(grep -E '^claude-runtime-protocol[[:space:]]*=' "$_atv_file" | head -n 1) || _atv_line=
	fi
	if [ -z "$_atv_line" ]; then
		_atv_file=$1/agent/Cargo.toml
		if [ ! -f "$_atv_file" ]; then die "$1 has no agent/Cargo.toml: not an Eitri checkout"; fi
		_atv_line=$(grep -E '^claude-runtime-protocol[[:space:]]*=' "$_atv_file" | head -n 1) || _atv_line=
	fi
	if [ -z "$_atv_line" ]; then
		die "$_atv_file names no claude-runtime-protocol dependency: not an Eitri checkout, or it no longer pins Verdandi the way this installer expects"
	fi
	AT_GIT_URL=$(printf '%s\n' "$_atv_line" | sed -n 's/.*git[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p')
	AT_REV=$(printf '%s\n' "$_atv_line" | sed -n 's/.*rev[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p')
	if [ -z "$AT_GIT_URL" ] || [ -z "$AT_REV" ]; then
		die "could not parse a git URL and rev out of $_atv_file's claude-runtime-protocol dependency: $_atv_line"
	fi
	if ! printf '%s\n' "$AT_REV" | grep -Eqx '[0-9a-f]{40}'; then
		die "$_atv_file pins claude-runtime-protocol at rev \"$AT_REV\", which is not a full 40-hex commit: --from-source --checkout needs the full commit, not a short prefix, to synthesize a trustworthy RELEASE. Pin the full commit in $_atv_file and re-run"
	fi
}

# resolve_verdandi_source: sets NV_VERDANDI_SOURCE_TARBALL_OVERRIDE and (when it built the archive)
# REL_VERDANDI_SOURCE_SHA256 to what this build is actually building the sidecar from.
# Requires REL_VERDANDI_REV already set (both callers set it before calling this). Three cases:
#   - OPT_VERDANDI_CHECKOUT given (DEV-1): checked (rev, clean tree) and archived from that tree;
#   - else OPT_CHECKOUT given and the pin's own source is public: a fresh clone of the public
#     Verdandi at the full rev, archived the same way (synthesize_release_from_checkout already
#     refused this branch when the source is private and --verdandi-checkout was not given);
#   - else: left empty -- sidecar_download_verdandi_source's own network path (a real release
#     asset) applies later, unchanged.
resolve_verdandi_source() {
	if [ -n "$OPT_VERDANDI_CHECKOUT" ]; then
		_rvs_dir=$OPT_VERDANDI_CHECKOUT
		if [ ! -e "$_rvs_dir/.git" ]; then die "--verdandi-checkout $_rvs_dir is not a git checkout (no .git)"; fi
		_rvs_head=$(git -C "$_rvs_dir" rev-parse HEAD 2>/dev/null) || die "cannot determine HEAD in --verdandi-checkout $_rvs_dir"
		case $_rvs_head in
		"$REL_VERDANDI_REV"*) ;;
		*)
			if [ "$OPT_ALLOW_VERDANDI_REV_MISMATCH" != 1 ]; then
				die "--verdandi-checkout $_rvs_dir is at $_rvs_head, not the pinned $REL_VERDANDI_REV: pass --allow-verdandi-rev-mismatch to build against it anyway (the built sidecar will then not match the revision this build otherwise trusts)"
			fi
			warn "--verdandi-checkout $_rvs_dir is at $_rvs_head, not the pinned $REL_VERDANDI_REV (--allow-verdandi-rev-mismatch): building against it anyway"
			NV_SIDECAR_ACTUAL_VERDANDI_REV=$_rvs_head
			;;
		esac
		_rvs_status=$(git -C "$_rvs_dir" status --porcelain --untracked-files=no 2>/dev/null) ||
			die "cannot determine the git status of --verdandi-checkout $_rvs_dir"
		if [ -n "$_rvs_status" ]; then
			die "--verdandi-checkout $_rvs_dir has modified tracked files: commit or stash them, or point at a clean checkout"
		fi
		_rvs_rev7=$(first7 "$_rvs_head")
		mkdir -p -- "$NV_CACHE_NV/verdandi-src-build" || die "cannot create $NV_CACHE_NV/verdandi-src-build"
		_rvs_out=$NV_CACHE_NV/verdandi-src-build/verdandi-$_rvs_rev7-source.tar.gz
		# Archived from DIR without ever building in place (never in place, because
		# buildBinary.mjs writes build/, dist/ and dist-bin/ and npm ci replaces node_modules). git
		# archive only reads the checkout; the caller's own before/after snapshot proves it changed
		# nothing.
		( cd "$_rvs_dir" && git archive --format=tar.gz -o "$_rvs_out" HEAD ) ||
			die "git archive failed in --verdandi-checkout $_rvs_dir"
		REL_VERDANDI_SOURCE_SHA256=$(file_sha256 "$_rvs_out")
		NV_VERDANDI_SOURCE_TARBALL_OVERRIDE=$_rvs_out
		return 0
	fi
	if [ -n "$OPT_CHECKOUT" ] && [ "$NV_VERDANDI_REV_IS_PUBLIC" = 1 ]; then
		_rvs_url=$(verdandi_repo_url)
		_rvs_clone=$NV_CACHE_NV/verdandi-src
		rm -rf -- "$_rvs_clone" || die "cannot remove the stale $_rvs_clone"
		git clone --quiet "$_rvs_url" "$_rvs_clone" ||
			die "could not clone the public Verdandi source ($_rvs_url): check your network connection and re-run"
		git -C "$_rvs_clone" checkout --quiet "$REL_VERDANDI_REV" ||
			die "the public Verdandi clone has no commit $REL_VERDANDI_REV: report it at $NV_ISSUES"
		_rvs_rev7=$(first7 "$REL_VERDANDI_REV")
		mkdir -p -- "$NV_CACHE_NV/verdandi-src-build" || die "cannot create $NV_CACHE_NV/verdandi-src-build"
		_rvs_out=$NV_CACHE_NV/verdandi-src-build/verdandi-$_rvs_rev7-source.tar.gz
		( cd "$_rvs_clone" && git archive --format=tar.gz -o "$_rvs_out" HEAD ) ||
			die "git archive failed in the cloned Verdandi source"
		REL_VERDANDI_SOURCE_SHA256=$(file_sha256 "$_rvs_out")
		NV_VERDANDI_SOURCE_TARBALL_OVERRIDE=$_rvs_out
	fi
}

# use_verdandi_source_beside_tarball: after unpack_new reads RELEASE, look for the named source
# asset beside --tarball, allowing an offline sidecar build without fetching this asset.
# Reuse sidecar_download_verdandi_source's trusted RELEASE checksum via the override variable;
# without a local asset, obtain_release provides NV_REL_URL as the download fallback.
# Copy into this run's 0700 NV_DL before hashing and extracting. Reopening the original in a
# shared directory could extract swapped, unverified source and npm scripts despite a valid
# hash of the earlier read, just as for --tarball/--sums/--sig.
use_verdandi_source_beside_tarball() {
	if [ -z "$OPT_TARBALL" ] || [ -z "${REL_VERDANDI_SOURCE-}" ]; then return 0; fi
	_uvsbt_dir=$(dirname -- "$OPT_TARBALL")
	_uvsbt_beside=$_uvsbt_dir/$REL_VERDANDI_SOURCE
	if [ -f "$_uvsbt_beside" ]; then
		_uvsbt_copy=$NV_DL/$REL_VERDANDI_SOURCE
		cp -- "$_uvsbt_beside" "$_uvsbt_copy" ||
			die "cannot copy $_uvsbt_beside into $NV_DL: check that the disk is not full, then re-run"
		say "using the Verdandi source beside $OPT_TARBALL ($REL_VERDANDI_SOURCE)"
		NV_VERDANDI_SOURCE_TARBALL_OVERRIDE=$_uvsbt_copy
	fi
}

# synthesize_release_from_checkout DIR: --from-source --checkout (the development loop)
# -- builds a RELEASE this installer trusts the same way it trusts a downloaded one (every
# field validated identically by parse_release_text), from DIR's own tree instead of a checksummed
# release asset. Sets NV_VERSION and NV_RELEASE_PATH; parse_release has been called on the result by
# the time this returns, so every REL_* field (the from-source ones included) is set exactly as it
# would be after a real download.
synthesize_release_from_checkout() {
	_src_dir=$1
	if [ ! -e "$_src_dir/.git" ]; then die "--checkout $_src_dir is not a git checkout (no .git)"; fi
	checkout_version "$_src_dir"
	NV_VERSION=$CV_VERSION
	_src_head=$(git -C "$_src_dir" rev-parse HEAD 2>/dev/null) || die "cannot determine HEAD in --checkout $_src_dir"
	if ! printf '%s\n' "$_src_head" | grep -Eqx '[0-9a-f]{40}'; then
		die "git rev-parse HEAD in $_src_dir did not return a 40-hex commit: is it a real git checkout with at least one commit?"
	fi
	checkout_fork_rev "$_src_dir"
	_src_fork=$CFR_REV
	agent_toml_verdandi "$_src_dir"
	case $AT_GIT_URL in
	https://github.com/HunterGrey-cyber/verdandi*) NV_VERDANDI_REV_IS_PUBLIC=1 ;;
	*) NV_VERDANDI_REV_IS_PUBLIC=0 ;;
	esac
	if [ "$NV_VERDANDI_REV_IS_PUBLIC" != 1 ] && [ -z "$OPT_VERDANDI_CHECKOUT" ]; then
		die "$_atv_file pins Verdandi from a private source ($AT_GIT_URL), so its source cannot be fetched publicly: pass --verdandi-checkout DIR (a local checkout of it), or set EITRI_VERDANDI_CHECKOUT"
	fi
	_src_pins=$_src_dir/packaging/pins.env
	if [ ! -f "$_src_pins" ]; then die "$_src_dir has no packaging/pins.env: not an Eitri checkout"; fi
	REL_VERDANDI_REV=$AT_REV
	resolve_verdandi_source
	if [ -z "$NV_VERDANDI_SOURCE_TARBALL_OVERRIDE" ]; then
		die "could not resolve where to build the sidecar's source from -- report it at $NV_ISSUES"
	fi
	_src_rev7=$(first7 "$AT_REV")
	# Read out to plain statements before the group below (rule 2): kv_get can die, and a
	# command substitution inside a `{ ...; } >file || die` group has its own errexit suspended
	# under a non-POSIX-mode bash.
	# Keep absent keys absent for older checkouts; even one empty key must retain pin validation.
	_src_pins_grpc_present=0
	if grep -Eq '^GRPC_TOOLS_(VERSION|SHA256_linux_(x64|arm64))=' "$_src_pins"; then
		_src_pins_grpc_present=1
	fi
	_src_pins_grpc_version=$(kv_get "$_src_pins" GRPC_TOOLS_VERSION)
	_src_pins_grpc_x64=$(kv_get "$_src_pins" GRPC_TOOLS_SHA256_linux_x64)
	_src_pins_grpc_arm64=$(kv_get "$_src_pins" GRPC_TOOLS_SHA256_linux_arm64)
	_src_pins_node_version=$(kv_get "$_src_pins" NODE_VERSION)
	_src_pins_node_x64=$(kv_get "$_src_pins" NODE_SHA256_linux_x64)
	_src_pins_node_arm64=$(kv_get "$_src_pins" NODE_SHA256_linux_arm64)
	_src_pins_nvim_version=$(kv_get "$_src_pins" NVIM_VERSION)
	_src_pins_nvim_sha=$(kv_get "$_src_pins" NVIM_SHA256_linux_x86_64)
	_src_contract=$_src_dir/packaging/sidecar-contract.env
	if [ -f "$_src_contract" ]; then
		_src_sidecar_version=$(kv_get "$_src_contract" SIDECAR_VERSION)
		_src_sidecar_major=$(kv_get "$_src_contract" SIDECAR_PROTOCOL_MAJOR)
		_src_sidecar_caps=$(kv_get "$_src_contract" SIDECAR_REQUIRED_CAPABILITIES)
	fi
	mkdir -p -- "$NV_CACHE_NV" || die "cannot create $NV_CACHE_NV"
	_src_release=$NV_CACHE_NV/from-source-RELEASE.$$
	{
		printf 'EITRI_VERSION=%s\n' "$NV_VERSION"
		printf 'EITRI_COMMIT=%s\n' "$_src_head"
		printf 'NEOVIDE_FORK_COMMIT=%s\n' "$_src_fork"
		printf 'VERDANDI_REV=%s\n' "$AT_REV"
		printf 'VERDANDI_SOURCE=verdandi-%s-source.tar.gz\n' "$_src_rev7"
		printf 'VERDANDI_SOURCE_SHA256=%s\n' "$REL_VERDANDI_SOURCE_SHA256"
		if [ -f "$_src_contract" ]; then
			printf 'SIDECAR_VERSION=%s\n' "$_src_sidecar_version"
			printf 'SIDECAR_PROTOCOL_MAJOR=%s\n' "$_src_sidecar_major"
			printf 'SIDECAR_REQUIRED_CAPABILITIES=%s\n' "$_src_sidecar_caps"
		fi
		if [ "$_src_pins_grpc_present" = 1 ]; then
			printf 'GRPC_TOOLS_VERSION=%s\n' "$_src_pins_grpc_version"
			printf 'GRPC_TOOLS_SHA256_linux_x64=%s\n' "$_src_pins_grpc_x64"
			printf 'GRPC_TOOLS_SHA256_linux_arm64=%s\n' "$_src_pins_grpc_arm64"
		fi
		printf 'NODE_VERSION=%s\n' "$_src_pins_node_version"
		printf 'NODE_SHA256_linux_x64=%s\n' "$_src_pins_node_x64"
		printf 'NODE_SHA256_linux_arm64=%s\n' "$_src_pins_node_arm64"
		printf 'NVIM_VERSION=%s\n' "$_src_pins_nvim_version"
		printf 'NVIM_SHA256_linux_x86_64=%s\n' "$_src_pins_nvim_sha"
		# This RELEASE hashes git-archive bytes, not a release asset. Mark it so a later setup
		# run reports that it cannot reproduce the archive instead of accusing its download of tampering.
		printf 'EITRI_BUILT_FROM_CHECKOUT=1\n'
	} >"$_src_release" || die "cannot write $_src_release"
	NV_RELEASE_PATH=$_src_release
	parse_release "$_src_release"
}

# clone_public_eitri VERSION: --from-source (without --checkout), the clone step --
# obtain_release has already fetched and verified SHA256SUMS/.sig and REL_* comes from a checked
# RELEASE asset (fetch_verified_release, below). Clones the public repo with submodules at tag
# v<VERSION> into <cache>/eitri/src/, and refuses unless HEAD equals REL_EITRI_COMMIT -- the
# release the checksums vouch for and the source tree being built must be the same commit.
clone_public_eitri() {
	_cpn_url=$(eitri_repo_url)
	_cpn_dir=$NV_CACHE_NV/src
	rm -rf -- "$_cpn_dir" || die "cannot remove the stale $_cpn_dir"
	git clone --quiet --branch "v$1" --recurse-submodules "$_cpn_url" "$_cpn_dir" ||
		die "could not clone $_cpn_url at v$1: check your network connection and re-run"
	_cpn_head=$(git -C "$_cpn_dir" rev-parse HEAD 2>/dev/null) || die "cannot determine HEAD in the cloned $_cpn_dir"
	if [ "$_cpn_head" != "$REL_EITRI_COMMIT" ]; then
		die "the clone of $_cpn_url at v$1 is at $_cpn_head, not $REL_EITRI_COMMIT (this release's own RELEASE): refusing to build from a tag that does not match the release it is signed and checksummed with. Report it at $NV_ISSUES"
	fi
	NV_FROM_SOURCE_DIR=$_cpn_dir
}

# fetch_verified_release: --from-source (without --checkout) -- obtain_release has already
# set NV_SUMS_TEXT/NV_VERSION/NV_REL_URL; this fetches and checks the RELEASE asset itself the same
# way dry_run_new_rev's network branch does for a dry run, then parses it for real.
fetch_verified_release() {
	_fvr_want=$(sums_hash RELEASE)
	fetch "$NV_REL_URL/RELEASE" "$NV_DL/RELEASE.part" RELEASE "$NV_MAX_DOWNLOAD_BYTES"
	_fvr_have=$(file_sha256 "$NV_DL/RELEASE.part")
	if [ "$_fvr_have" != "$_fvr_want" ]; then
		rm -f -- "$NV_DL/RELEASE.part" || :
		die "checksum mismatch for RELEASE: SHA256SUMS says $_fvr_want, the download is $_fvr_have. The release is corrupt or was altered; report it at $NV_ISSUES"
	fi
	mv -- "$NV_DL/RELEASE.part" "$NV_DL/RELEASE" || die "cannot rename $NV_DL/RELEASE.part"
	parse_release "$NV_DL/RELEASE"
	if [ "$REL_VERSION" != "$NV_VERSION" ]; then
		die "the RELEASE of $NV_VERSION says $REL_VERSION: refusing. Report it at $NV_ISSUES"
	fi
	NV_RELEASE_PATH=$NV_DL/RELEASE
}

# companion_required NAME: older release lines may omit a companion. Compare with rc.0 so
# every candidate of the first required release line requires it too.
companion_required() {
	NV_COMPANION_REQUIRED=1
	case $1 in
	eitri-server) _cr_since=$NV_SERVER_SINCE ;;
	eitri-editor-bridge) _cr_since=$NV_BRIDGE_SINCE ;;
	*) return 0 ;;
	esac
	_cr_cmp=$(semver_cmp "$NV_VERSION" "$_cr_since-rc.0")
	if [ "$_cr_cmp" -lt 0 ]; then NV_COMPANION_REQUIRED=0; fi
}

# from_source_stage SRC RELEASE_PATH: SRC is the tree that was just built (target/release/ holds the
# binaries required by that release); RELEASE_PATH is the RELEASE this build matches. Builds the same on-disk shape
# unpack_new produces from a downloaded tarball (NV_STAGE_TOP, NV_LIB.new), so ensure_sidecar,
# maybe_offer_nvim, stage_files, swap_in, commit_files and prune_sidecars -- finish_install's own
# body -- are the one path both a prebuilt install and a from-source one share from here on. There
# is no THIRD-PARTY-LICENSES/SOURCE generation here (release.sh generates those, from the pinned Skia archive): a from-source build says plainly what it is rather
# than inventing licence attribution it cannot yet produce correctly.
from_source_stage() {
	_fss_src=$1
	_fss_release=$2
	rm -rf -- "$NV_STAGE" || die "cannot remove the stale $NV_STAGE"
	_fss_top=$NV_STAGE/eitri-$NV_VERSION-x86_64-linux
	NV_STAGE_TOP=$_fss_top
	mkdir -p -- "$_fss_top/bin" "$_fss_top/lib/eitri" "$_fss_top/share/applications" "$_fss_top/share/licenses/eitri" "$_fss_top/share/eitri/eitri.nvim" "$_fss_top/share/gnome-shell/extensions/$NV_EXT_ID" ||
		die "cannot create $_fss_top"
	cp -- "$_fss_src/packaging/eitri.launcher.sh" "$_fss_top/bin/eitri" || die "cannot stage the launcher"
	chmod 0755 "$_fss_top/bin/eitri" || die "cannot chmod the staged launcher"
	for _fss_b in $NV_BINARIES; do
		companion_required "$_fss_b"
		if [ "$NV_COMPANION_REQUIRED" = 0 ] && [ ! -e "$_fss_src/target/release/$_fss_b" ] &&
			[ ! -L "$_fss_src/target/release/$_fss_b" ]; then continue; fi
		if [ ! -x "$_fss_src/target/release/$_fss_b" ]; then
			die "the build did not produce target/release/$_fss_b: report it at $NV_ISSUES"
		fi
		cp -- "$_fss_src/target/release/$_fss_b" "$_fss_top/lib/eitri/$_fss_b" || die "cannot stage $_fss_b"
		chmod 0755 "$_fss_top/lib/eitri/$_fss_b" || die "cannot chmod $_fss_b"
	done
	cp -- "$_fss_src/packaging/install.sh" "$_fss_top/lib/eitri/eitri-setup" || die "cannot stage eitri-setup"
	chmod 0755 "$_fss_top/lib/eitri/eitri-setup" || die "cannot chmod eitri-setup"
	cp -- "$_fss_release" "$_fss_top/lib/eitri/RELEASE" || die "cannot stage RELEASE"
	if [ ! -f "$_fss_src/packaging/$NV_MAIN_DESKTOP" ]; then
		die "$_fss_src has no packaging/$NV_MAIN_DESKTOP: it is a source tree from before Eitri's app icon (its desktop entry is packaging/$NV_OLD_DESKTOP), which this installer does not lay out. $(before_icon_advice "that tree with its own packaging/install.sh --from-source")"
	fi
	for _fss_d in $NV_DESKTOP; do
		if [ ! -f "$_fss_src/packaging/$_fss_d" ]; then die "$_fss_src has no packaging/$_fss_d: report it at $NV_ISSUES"; fi
		cp -- "$_fss_src/packaging/$_fss_d" "$_fss_top/share/applications/$_fss_d" || die "cannot stage the desktop entry $_fss_d"
	done
	for _fss_i in $NV_PLUGIN_FILES; do
		if [ ! -f "$_fss_src/nvim/eitri.nvim/$_fss_i" ]; then die "$_fss_src has no nvim/eitri.nvim/$_fss_i: report it at $NV_ISSUES"; fi
		mkdir -p -- "$_fss_top/share/eitri/eitri.nvim/${_fss_i%/*}" || die "cannot create $_fss_top/share/eitri/eitri.nvim/${_fss_i%/*}"
		cp -- "$_fss_src/nvim/eitri.nvim/$_fss_i" "$_fss_top/share/eitri/eitri.nvim/$_fss_i" || die "cannot stage the plugin file $_fss_i"
	done
	for _fss_i in $NV_EXT_FILES; do
		if [ ! -f "$_fss_src/gnome-extension/$_fss_i" ]; then die "$_fss_src has no gnome-extension/$_fss_i: report it at $NV_ISSUES"; fi
		cp -- "$_fss_src/gnome-extension/$_fss_i" "$_fss_top/share/gnome-shell/extensions/$NV_EXT_ID/$_fss_i" || die "cannot stage the extension file $_fss_i"
	done
	for _fss_i in $NV_ICON_FILES; do
		if [ ! -f "$_fss_src/packaging/icons/$_fss_i" ]; then die "$_fss_src has no packaging/icons/$_fss_i: report it at $NV_ISSUES"; fi
		mkdir -p -- "$_fss_top/share/icons/${_fss_i%/*}" || die "cannot create $_fss_top/share/icons/${_fss_i%/*}"
		cp -- "$_fss_src/packaging/icons/$_fss_i" "$_fss_top/share/icons/$_fss_i" || die "cannot stage the icon $_fss_i"
	done
	# Each shipped plugin directory: its tracked manifest, and the site the build just wrote (shell/build.rs) without
	# the build's fingerprint. A release build has no source-tree search step, so this is how a from-source install
	# finds the plugin by name.
	# A tree with no plugins/ at all is a release from before the shipped plugins, refused with the way to install
	# it as a tarball of one is; a tree with plugins/ but no manifest is broken.
	if [ ! -d "$_fss_src/plugins" ]; then
		die "$_fss_src has no plugins/: it is a source tree from before Eitri shipped plugins, which this installer does not lay out. Nothing was changed. To install it, use its own installer: packaging/install.sh --from-source in that tree (or that release's install.sh asset with --from-source)."
	fi
	for _fss_p in $NV_SHIPPED_PLUGINS; do
		if [ ! -f "$_fss_src/plugins/$_fss_p/eitri-plugin.toml" ]; then die "$_fss_src has no plugins/$_fss_p/eitri-plugin.toml: report it at $NV_ISSUES"; fi
		if [ ! -f "$_fss_src/plugins/$_fss_p/web/dist/index.html" ]; then
			# In a checkout built before, removing web/dist/ by hand does not make cargo rerun the build
			# script that writes it (it watches the site's inputs, not its output), so the advice comes first.
			if [ -n "$OPT_CHECKOUT" ]; then
				die "the build did not produce plugins/$_fss_p/web/dist/index.html. Nothing was changed. If plugins/$_fss_p/web/dist/ was removed from a checkout that was built before, cargo does not know to write it again: run cargo clean -p shell --release --target-dir $_fss_src/target and install again. Otherwise report it at $NV_ISSUES"
			fi
			die "the build did not produce plugins/$_fss_p/web/dist/index.html: report it at $NV_ISSUES"
		fi
		mkdir -p -- "$_fss_top/lib/eitri/plugins/$_fss_p/web" || die "cannot create $_fss_top/lib/eitri/plugins/$_fss_p"
		cp -- "$_fss_src/plugins/$_fss_p/eitri-plugin.toml" "$_fss_top/lib/eitri/plugins/$_fss_p/eitri-plugin.toml" ||
			die "cannot stage plugins/$_fss_p/eitri-plugin.toml"
		cp -R -- "$_fss_src/plugins/$_fss_p/web/dist" "$_fss_top/lib/eitri/plugins/$_fss_p/web/dist" ||
			die "cannot stage plugins/$_fss_p/web/dist"
		rm -f -- "$_fss_top/lib/eitri/plugins/$_fss_p/web/dist/.inputs-sha256" ||
			die "cannot remove the build's fingerprint from the staged plugins/$_fss_p"
	done
	if [ -f "$_fss_src/LICENSE" ]; then
		cp -- "$_fss_src/LICENSE" "$_fss_top/share/licenses/eitri/LICENSE" || die "cannot stage LICENSE"
	else
		printf 'No LICENSE file was found in %s.\n' "$_fss_src" >"$_fss_top/share/licenses/eitri/LICENSE" ||
			die "cannot write a placeholder LICENSE"
	fi
	if [ -f "$_fss_src/NOTICE" ]; then
		cp -- "$_fss_src/NOTICE" "$_fss_top/share/licenses/eitri/NOTICE" || die "cannot stage NOTICE"
	else
		case $(licence_files) in
		*NOTICE*)
			printf 'No NOTICE file was found in %s.\n' "$_fss_src" >"$_fss_top/share/licenses/eitri/NOTICE" ||
				die "cannot write a placeholder NOTICE"
			;;
		esac
	fi
	{
		printf 'This copy of Eitri was built directly from source (sh install.sh --from-source),\n'
		printf 'not from a signed release build, so this file is a placeholder rather than the\n'
		printf 'generated third-party licence text a release build carries.\n\n'
		printf 'source tree:          %s\n' "$_fss_src"
		printf 'commit:               %s\n' "$REL_EITRI_COMMIT"
		printf 'neovide fork commit:  %s\n\n' "$REL_NEOVIDE_FORK_COMMIT"
		printf 'To generate the real text, run packaging/collect-licenses.py in that tree once its\n'
		printf 'own build has produced target/release and agent-ui/web/node_modules -- see that\n'
		printf 'script'"'"'s own --help.\n'
		# The one notice that needs no generating: the logo (the application icon) is not covered by
		# LICENSE's MIT, and its CC BY credit travels with every copy of it.
		if [ -f "$_fss_src/packaging/icons/LICENSE" ]; then
			printf '\nThe application icon is the Eitri logo, which LICENSE does not cover:\n\n'
			cat -- "$_fss_src/packaging/icons/LICENSE"
		fi
	} >"$_fss_top/share/licenses/eitri/THIRD-PARTY-LICENSES" || die "cannot write THIRD-PARTY-LICENSES"
	cp -- "$_fss_top/share/licenses/eitri/THIRD-PARTY-LICENSES" "$_fss_top/share/licenses/eitri/SOURCE" ||
		die "cannot write SOURCE"
	parse_release "$_fss_top/lib/eitri/RELEASE"
	if [ "$REL_VERSION" != "$NV_VERSION" ]; then
		die "the RELEASE for this build says $REL_VERSION, not $NV_VERSION: refusing. Report it at $NV_ISSUES"
	fi
	NV_NEW_REV7=$REL_REV7
	mkdir -p -- "$NV_LIBROOT" || die "cannot create $NV_LIBROOT"
	NV_NEW_CREATED=1
	cp -R -- "$_fss_top/lib/eitri" "$NV_LIB.new" || die "cannot copy the new install to $NV_LIB.new (is the disk full?)"
}

# finish_install: shared by do_install and do_from_source from the point each has produced NV_LIB.new
# onward. Factored out so the two never drift on what happens after the binaries
# themselves are in hand.
finish_install() {
	ensure_sidecar
	maybe_offer_nvim
	stage_files
	swap_in
	commit_files
	prune_sidecars
	sidecar_state "$NV_NEW_REV7"
	if [ "$NV_SIDECAR_PRESENT" = 1 ]; then report_claude "$NV_SIDECAR_BIN"; else report_claude; fi
	path_warning
	if [ "$OPT_DRY_RUN" = 1 ]; then
		apparmor_note
		say "dry run: nothing was changed"
		return 0
	fi
	say "installed Eitri $NV_VERSION into $NV_LIB; run it with: eitri [project directory]"
	if [ -n "$NV_INSTALLED_VERSION" ]; then
		say "restart open Eitri windows: they keep running, but new tabs and the handoff need the new install"
	fi
	# Last, so the one-time sudo steps are the last thing on screen.
	apparmor_note
}

# ---------------------------------------------------------------------------------------------
# The AppArmor user-namespace restriction
#
# Ubuntu 23.10+ sets kernel.apparmor_restrict_unprivileged_userns=1: a program may create a user
# namespace only while an AppArmor profile granting `userns` confines it, and WebKitGTK's bwrap
# sandbox needs one. Without it Eitri shows the fix in the agent panel's place instead of the
# panel (shell/src/webkit_sandbox.rs). This installer never runs sudo, so it writes the profile for
# this install under its own data directory and prints the two commands that install and load it.
#
# The profile is packaging/apparmor/eitri with this install's own resolved path, always quoted and
# with AppArmor's glob characters escaped, and a name of its own -- eitri-user-<uid>, so no two
# users' per-user profiles replace each other in the kernel. A literal path rather than an @{HOME}
# glob (which works too): the grant covers this user's install and nobody else's. Eitri itself
# renders the same bytes and names the same commands (shell/src/webkit_sandbox.rs; its test sources
# this file and compares), so the steps printed here and the ones the panel's place shows agree.

# apparmor_restricted: AA_RESTRICTED=1 when the kernel restricts unprivileged user namespaces.
apparmor_restricted() {
	AA_RESTRICTED=0
	_ar_f=$NV_PROC/sys/kernel/apparmor_restrict_unprivileged_userns
	_ar_v=
	if [ -r "$_ar_f" ]; then
		_ar_v=$(cat -- "$_ar_f" 2>/dev/null) || _ar_v=
	fi
	if [ "$_ar_v" = 1 ]; then AA_RESTRICTED=1; fi
}

# sh_quote WORD: WORD as one word of a POSIX shell command, as the steps print it -- as it is when
# it is only ASCII letters, digits and _./-, else in single quotes. The letters are spelled out
# rather than ranged: a range follows the locale in some shells.
sh_quote() {
	case $1 in
	'' | *[!abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_./-]*)
		printf "'%s'\n" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"
		;;
	*) printf '%s\n' "$1" ;;
	esac
}

# apparmor_escape PATH: PATH with AppArmor's glob characters, the backslash and the double quote
# each escaped with a backslash (a path holding a space and []{}*?^@" loaded and attached on the
# Ubuntu 24.04 VM this way).
apparmor_escape() {
	printf '%s' "$1" | sed 's/[][\\{}*?^@"]/\\&/g'
}

# apparmor_plan EXE UID: the profile's name for EXE (the package's own for /usr/lib/eitri/shell,
# else eitri-user-UID), where this installer writes it (AA_SRC), where it is installed
# (AA_TARGET), and the two commands (AA_CMD1 installs it, AA_CMD2 loads it).
apparmor_plan() {
	if [ "$1" = /usr/lib/eitri/shell ]; then AA_NAME=eitri; else AA_NAME=eitri-user-$2; fi
	AA_SRC=$NV_DATA/eitri/apparmor/$AA_NAME
	AA_TARGET=$NV_APPARMOR_D/$AA_NAME
	_ap_src=$(sh_quote "$AA_SRC")
	_ap_dst=$(sh_quote "$AA_TARGET")
	AA_CMD1="sudo install -m 0644 $_ap_src $_ap_dst"
	AA_CMD2="sudo apparmor_parser -r $_ap_dst"
}

# apparmor_render NAME EXE: the profile text (packaging/apparmor/eitri, with NAME and EXE).
apparmor_render() {
	_arn_path=$(apparmor_escape "$2")
	cat <<'EOF'
# AppArmor profile for Eitri's `shell` (packaging/apparmor/eitri).
#
# Ubuntu 23.10 and later set kernel.apparmor_restrict_unprivileged_userns=1: a program may create a
# user namespace only while an AppArmor profile that grants `userns` confines it. WebKitGTK 6.0 runs
# its web and network processes in a bwrap sandbox, which needs one, so without this profile the
# agent panel cannot start. The profile grants that and nothing else: it is unconfined otherwise,
# the same shape as the profiles Ubuntu's own apparmor package ships for its WebKit applications
# (epiphany, for one). Every process Eitri starts inherits it.
#
# The .deb installs this file as /etc/apparmor.d/eitri and loads nothing (it has no maintainer
# scripts): `sudo apparmor_parser -r /etc/apparmor.d/eitri`, or the next restart, loads it. For
# any other install, the curl installer and Eitri itself write this same text with that install's
# own path and a profile name of its own.

abi <abi/4.0>,
include <tunables/global>

EOF
	printf 'profile %s "%s" flags=(unconfined) {\n' "$1" "$_arn_path"
	cat <<'EOF'
  userns,

  # Site-specific additions and overrides. See local/README for details.
EOF
	printf '  include if exists <local/%s>\n}\n' "$1"
}

# apparmor_note: where the restriction is on, write this install's profile under $NV_DATA and say
# how to install and load it (a dry run only says where it would go). Never fatal: the install is
# complete by now, and Eitri writes the same file and shows the same steps itself.
apparmor_note() {
	apparmor_restricted
	if [ "$AA_RESTRICTED" != 1 ]; then return 0; fi
	# The path AppArmor attaches by is the resolved one, the path `shell` runs from.
	_an_lib=$(cd -P -- "$NV_LIB" 2>/dev/null && pwd -P) || _an_lib=$NV_LIB
	_an_exe=$_an_lib/shell
	_an_uid=$(id -u 2>/dev/null) || _an_uid=
	_an_why="this system restricts unprivileged user namespaces (Ubuntu's AppArmor rule), so the agent panel's sandbox (WebKit's bwrap) needs an AppArmor profile for this install"
	_an_hatch="until then Eitri shows these steps in the agent panel's place; starting it with WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1 also works, but that removes the operating system's sandbox from the process that renders the model's output"
	case $_an_uid in
	'' | *[!0-9]*)
		warn "$_an_why, and \`id -u\` gave no user id to name it by: Eitri shows the steps in the agent panel's place"
		return 0
		;;
	esac
	case $_an_exe in
	*[[:cntrl:]]*)
		warn "$_an_why, and $NV_LIB holds a control character, which no profile can name: install Eitri under a plainer HOME, or $_an_hatch"
		return 0
		;;
	esac
	apparmor_plan "$_an_exe" "$_an_uid"
	if [ "$OPT_DRY_RUN" = 1 ]; then
		say "$_an_why: would write it to $AA_SRC"
		return 0
	fi
	_an_text=$(apparmor_render "$AA_NAME" "$_an_exe")
	if ! mkdir -p -- "$NV_DATA/eitri/apparmor" ||
		! printf '%s\n' "$_an_text" >"$AA_SRC.tmp.$$" ||
		! mv -- "$AA_SRC.tmp.$$" "$AA_SRC"; then
		rm -f -- "$AA_SRC.tmp.$$"
		warn "$_an_why, and it could not be written to $AA_SRC (see the message above): Eitri writes it itself and shows the steps in the agent panel's place"
		return 0
	fi
	_an_have=
	if [ -f "$AA_TARGET" ]; then _an_have=$(cat -- "$AA_TARGET" 2>/dev/null) || _an_have=; fi
	if [ -n "$_an_have" ] && [ "$_an_have" = "$_an_text" ]; then
		say "the AppArmor profile $AA_TARGET for this install is in place; it loads at boot, or now with: $AA_CMD2"
		return 0
	fi
	# Said once, last, and plainly: what stays off, until what, and why; the two commands end the run.
	say "the agent panel stays off until these two commands are run once. Why: $_an_why, and putting one in place takes sudo, which this installer never runs. The editor and the terminal work without it, and Eitri shows the same steps in the panel's place. The profile is written to $AA_SRC. (Starting Eitri with WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1 also brings the panel up, but that removes the operating system's sandbox from the process that renders the model's output.)"
	say "    $AA_CMD1"
	say "    $AA_CMD2"
}

# fetch_skia_binaries WORKDIR PINS_FILE: `cargo build` on a
# --from-source tree otherwise leaves skia-bindings to download its own prebuilt Skia archive
# straight off GitHub with no content check at all (its own vendored downloader carries a literal
# `// TODO: verify key` comment); this fetches the exact archive PINS_FILE pins
# (SKIA_BINARIES_URL_UPSTREAM) and checks it against SKIA_BINARIES_SHA256 first. Sets
# NV_SKIA_ARCHIVE to the checked local copy; the caller passes it to `cargo build` as
# SKIA_BINARIES_URL=file://$NV_SKIA_ARCHIVE, skia-bindings' own documented escape hatch for a pinned
# local copy, so the build never touches the network for this file at all.
fetch_skia_binaries() {
	_fsb_workdir=$1
	_fsb_pins=$2
	_fsb_url=$(kv_get "$_fsb_pins" SKIA_BINARIES_URL_UPSTREAM)
	_fsb_sha=$(kv_get "$_fsb_pins" SKIA_BINARIES_SHA256)
	if [ -z "$_fsb_url" ] || [ -z "$_fsb_sha" ]; then
		die "$_fsb_pins names no SKIA_BINARIES_URL_UPSTREAM/SKIA_BINARIES_SHA256: not an Eitri checkout's pins.env. Report it at $NV_ISSUES"
	fi
	_fsb_name=${_fsb_url##*/}
	# In test mode, redirected to a fixture server the same way node_dist_base is -- deliberately
	# never falling back to the real GitHub URL when the override is unset, so a test that forgets
	# to set it fails fast against a refused loopback port instead of downloading the real archive.
	if [ "$NV_TEST_MODE" = 1 ]; then
		_fsb_get=${EITRI_INSTALL_TEST_SKIA_BASE_URL:-http://127.0.0.1:1}/$_fsb_name
	else
		_fsb_get=$_fsb_url
	fi
	_fsb_out=$_fsb_workdir/$_fsb_name
	curl_get "$_fsb_get" "$_fsb_out.part" "$NV_MAX_DOWNLOAD_BYTES"
	if [ "$CG_OK" != 1 ]; then die "could not download the pinned Skia binaries ($_fsb_get): $CG_ERR"; fi
	_fsb_have=$(file_sha256 "$_fsb_out.part")
	if [ "$_fsb_have" != "$_fsb_sha" ]; then
		rm -f -- "$_fsb_out.part" || :
		die "checksum mismatch for $_fsb_name: expected $_fsb_sha, got $_fsb_have. Refusing to build against an unverified Skia archive; re-run, and report it at $NV_ISSUES if it persists"
	fi
	mv -- "$_fsb_out.part" "$_fsb_out" || die "cannot rename $_fsb_out.part"
	NV_SKIA_ARCHIVE=$_fsb_out
}

# do_from_source: --from-source. check_glibc is deliberately not called -- building from
# source is this installer's own escape hatch off the glibc-2.39 floor prebuilt binaries are held to.
# check_platform itself is not called either (the x86_64 check below is not a call to it: unlike
# check_platform, it names v1's own architecture ceiling, not the reason prebuilt releases refuse).
# check_gtk_webkit still runs: whatever built the binary, it still needs a GTK 4.14+/WebKitGTK 6.0
# runtime to start.
do_from_source() {
	if [ "$OPT_DRY_RUN" = 1 ]; then
		# Every other mode's --dry-run only ever prints network fetches and file moves (each behind
		# `run`/its own OPT_DRY_RUN check); a from-source build's own clone and compile are not
		# meaningfully simulated that way, so this says so plainly rather than only pretending to.
		die "--dry-run is not supported with --from-source (it clones and compiles for real): omit --dry-run"
	fi
	# The pinned Skia archive is x86_64, and skia-bindings 0.156.0 unpacks it without checking
	# the target. Refuse other architectures before cloning or compiling, rather than reach a
	# confusing linker error. ARM builds remain unsupported until an architecture-selected Skia
	# archive is pinned and can be tested.
	if [ -z "$OPT_CHECKOUT" ]; then
		check_base_url "${OPT_BASE_URL:-$NV_DEFAULT_BASE_URL}"
		refuse_older_layout_early
	fi
	_dfs_arch=$(uname -m) || die "uname failed"
	case $_dfs_arch in
	x86_64) ;;
	*) die "v1 builds x86_64 only, and this is $_dfs_arch: there is no from-source path for another architecture yet (no aarch64 Skia archive is pinned). Track this at $NV_ISSUES" ;;
	esac
	check_build_tools
	check_gtk_webkit
	nvim_check
	require_curl
	NV_INSTALLED_VERSION=
	NV_OLD_REV7=
	rev_of_release "$NV_SYSTEM_RELEASE" "$NV_SYSTEM_REMEDY"
	NV_SYS_REV7=$RR_REV7
	if [ -f "$NV_LIB/RELEASE" ]; then
		NV_INSTALLED_VERSION=$(kv_get "$NV_LIB/RELEASE" EITRI_VERSION)
		if ! printf '%s\n' "$NV_INSTALLED_VERSION" | grep -Eqx "$NV_VERSION_ERE"; then NV_INSTALLED_VERSION=; fi
		rev_of_release "$NV_LIB/RELEASE" "remove $NV_LIB (only the program: your settings and state live elsewhere)"
		NV_OLD_REV7=$RR_REV7
	fi
	acquire_lock
	recover_interrupted

	if [ -n "$OPT_CHECKOUT" ]; then
		synthesize_release_from_checkout "$OPT_CHECKOUT"
		# Canonicalize --checkout before run_in changes cwd: a relative target path would otherwise
		# resolve inside the checkout during cargo build but outside it during removal and staging.
		# synthesize_release_from_checkout already requires OPT_CHECKOUT/.git to exist.
		NV_FROM_SOURCE_DIR=$(CDPATH='' cd -P -- "$OPT_CHECKOUT" && pwd -P) || die "cannot resolve --checkout $OPT_CHECKOUT to an absolute path"
	else
		check_base_url "${OPT_BASE_URL:-$NV_DEFAULT_BASE_URL}"
		obtain_release
		# Apply the downgrade refusal to source downloads too: a valid signature does not prevent
		# a mirror serving an older signed release. --checkout has no release server to roll back from.
		if [ -n "$NV_INSTALLED_VERSION" ]; then
			_dfs_c=$(semver_cmp "$NV_VERSION" "$NV_INSTALLED_VERSION")
			if [ "$_dfs_c" = -1 ] && [ -z "$OPT_VERSION" ]; then
				die "Eitri $NV_INSTALLED_VERSION is installed, and the release offered is older ($NV_VERSION): refusing to downgrade silently (a stale mirror or a withdrawn release would look like this). To install $NV_VERSION anyway: --version $NV_VERSION"
			fi
		fi
		fetch_verified_release
		clone_public_eitri "$NV_VERSION"
		if [ -n "$OPT_VERDANDI_CHECKOUT" ]; then resolve_verdandi_source; fi
	fi

	companion_required eitri-server
	if [ "$NV_COMPANION_REQUIRED" = 1 ] && [ ! -f "$NV_FROM_SOURCE_DIR/server/Cargo.toml" ]; then
		die "source tree for $NV_VERSION has no server/Cargo.toml: eitri-server is required from $NV_SERVER_SINCE, including candidates"
	fi
	companion_required eitri-editor-bridge
	if [ "$NV_COMPANION_REQUIRED" = 1 ] && [ ! -f "$NV_FROM_SOURCE_DIR/shell/src/bin/eitri-editor-bridge.rs" ]; then
		die "source tree for $NV_VERSION has no shell/src/bin/eitri-editor-bridge.rs: eitri-editor-bridge is required from $NV_BRIDGE_SINCE, including candidates"
	fi

	_dfs_node_workdir=$NV_CACHE_NV/from-source-node
	rm -rf -- "$_dfs_node_workdir" || die "cannot remove the stale $_dfs_node_workdir"
	mkdir -p -- "$_dfs_node_workdir" || die "cannot create $_dfs_node_workdir"
	sidecar_download_node "$_dfs_node_workdir" 0
	_dfs_node_top=$_dfs_node_workdir/node-$REL_NODE_VERSION-linux-$NV_NODE_ARCH
	archive_names_inside -tf "$SDN_TARBALL" "$SDN_TARBALL"
	tar -xf "$SDN_TARBALL" --no-same-owner -C "$_dfs_node_workdir" || die "could not unpack $SDN_TARBALL: report it at $NV_ISSUES"
	links_stay_inside "$_dfs_node_workdir" "$SDN_TARBALL"
	if [ ! -x "$_dfs_node_top/bin/node" ] || [ ! -x "$_dfs_node_top/bin/npm" ]; then
		die "$SDN_TARBALL does not contain bin/node and bin/npm at $_dfs_node_top: report it at $NV_ISSUES"
	fi

	_dfs_skia_workdir=$NV_CACHE_NV/from-source-skia
	rm -rf -- "$_dfs_skia_workdir" || die "cannot remove the stale $_dfs_skia_workdir"
	mkdir -p -- "$_dfs_skia_workdir" || die "cannot create $_dfs_skia_workdir"
	fetch_skia_binaries "$_dfs_skia_workdir" "$NV_FROM_SOURCE_DIR/packaging/pins.env"

	say "building Eitri $NV_VERSION from source in $NV_FROM_SOURCE_DIR"
	# Staging reads src/target/release. --target-dir overrides CARGO_TARGET_DIR and build.target-dir
	# so cargo writes where staging reads. Remove shipped binaries before building so a skipped
	# build cannot stage stale output under a RELEASE naming the current HEAD.
	# Known gap: --target-dir does not override build.target or CARGO_BUILD_TARGET, which nest
	# output under a triple. Such a build fails staging with missing target/release binaries
	# rather than shipping stale ones. An explicit target would need a corresponding staging-layout
	# change; this remains unsupported for now.
	for _dfs_b in $NV_BINARIES; do
		rm -f -- "$NV_FROM_SOURCE_DIR/target/release/$_dfs_b" || die "cannot remove the stale $NV_FROM_SOURCE_DIR/target/release/$_dfs_b"
	done
	set -- -p shell -p agent
	if [ -f "$NV_FROM_SOURCE_DIR/server/Cargo.toml" ]; then set -- "$@" -p eitri-server; fi
	run_in "$NV_FROM_SOURCE_DIR" env -u EITRI_SERVER_SHA256 "PATH=$_dfs_node_top/bin:$PATH" "SKIA_BINARIES_URL=file://$NV_SKIA_ARCHIVE" \
		cargo build --release --locked --target-dir "$NV_FROM_SOURCE_DIR/target" "$@" --bins

	from_source_stage "$NV_FROM_SOURCE_DIR" "$NV_RELEASE_PATH"
	finish_install
	# on_exit cleans this run's workdir and from-source-node on success or failure unless
	# --keep-build was requested, including failures inside finish_install.
}

# dry_run_new_rev: a dry run unpacks nothing, but which sidecar the new install uses decides what
# ensure_sidecar and prune_sidecars report, so the same RELEASE is read without writing anything:
# out of the --tarball file (already checked against --sums), or as the release's own RELEASE asset
# checked against SHA256SUMS -- the installer trusts a RELEASE that passed SHA256SUMS,
# and the asset is the file the tarball carries. Sets NV_NEW_REV7; it stays empty, and the reports
# say so, only when SHA256SUMS lists no RELEASE or the tarball cannot be read that way.
dry_run_new_rev() {
	NV_NEW_REV7=
	_dr_text=
	_dr_top=eitri-$NV_VERSION-x86_64-linux
	if [ -n "$OPT_TARBALL" ]; then
		_dr_from="read from $NV_TARBALL_NAME"
		for _dr_m in "$_dr_top/lib/eitri/RELEASE" "./$_dr_top/lib/eitri/RELEASE"; do
			_dr_text=$(tar -xzOf "$NV_TARBALL" "$_dr_m" 2>/dev/null) || _dr_text=
			if [ -n "$_dr_text" ]; then break; fi
		done
	else
		_dr_from='checked against SHA256SUMS'
		_dr_n=$(printf '%s\n' "$NV_SUMS_TEXT" | awk '$2 == "RELEASE" { n++ } END { print n + 0 }')
		if [ "$_dr_n" = 1 ]; then
			_dr_want=$(sums_hash RELEASE)
			# The x keeps the file's own trailing newlines through $(...), so the hash is of exactly
			# what was served. fetch dies on a failed download, ending the $(...) before the x.
			_dr_text=$(
				fetch "$NV_REL_URL/RELEASE" - RELEASE "$NV_MAX_DOWNLOAD_BYTES"
				printf x
			)
			_dr_text=${_dr_text%x}
			_dr_have=$(text_sha256 "$_dr_text")
			if [ "$_dr_have" != "$_dr_want" ]; then
				die "checksum mismatch for RELEASE: SHA256SUMS says $_dr_want, the download is $_dr_have. The release is corrupt or was altered; report it at $NV_ISSUES"
			fi
		fi
	fi
	if [ -z "$_dr_text" ]; then
		say "this dry run could not obtain the new release's RELEASE, so which sidecar it uses is not known"
		return 0
	fi
	parse_release_text "$_dr_text" "the RELEASE of $NV_VERSION"
	if [ "$REL_VERSION" != "$NV_VERSION" ]; then
		die "the RELEASE of $NV_VERSION says $REL_VERSION: refusing. Report it at $NV_ISSUES"
	fi
	NV_NEW_REV7=$REL_REV7
	say "the new release's RELEASE ($_dr_from) names verdandi $NV_NEW_REV7"
}

# Through 0.3.0 the acknowledgement sits inside LICENSE, so those releases have no NOTICE.
# A later source tree can still call itself 0.3.0; install its NOTICE when staged.
licence_files() {
	_lf_cmp=$(semver_cmp "$NV_VERSION" 0.3.0)
	if [ "$_lf_cmp" -le 0 ]; then
		if [ -n "${NV_STAGE_TOP:-}" ] && [ -f "$NV_STAGE_TOP/share/licenses/eitri/NOTICE" ]; then
			printf '%s NOTICE\n' "$NV_LICENCE_FILES"
		else
			printf '%s\n' "$NV_LICENCE_FILES"
		fi
	else
		printf '%s NOTICE\n' "$NV_LICENCE_FILES"
	fi
}

# unpack_new: the verified tarball, unpacked in the cache and checked, becomes $NV_LIB.new.
unpack_new() {
	_un_top=$NV_STAGE/eitri-$NV_VERSION-x86_64-linux
	NV_STAGE_TOP=$_un_top
	if [ "$OPT_DRY_RUN" = 1 ]; then
		say "would unpack $NV_TARBALL_NAME into $NV_STAGE, check its layout, and copy its lib/eitri to $NV_LIB.new"
		dry_run_new_rev
		return 0
	fi
	mkdir -p -- "$NV_STAGE" || die "cannot create $NV_STAGE"
	archive_names_inside -tzf "$NV_TARBALL" "$NV_TARBALL_NAME"
	tar -xzf "$NV_TARBALL" --no-same-owner -C "$NV_STAGE" || die "could not unpack $NV_TARBALL_NAME: it is not a valid Eitri release. Nothing was changed and your current install is untouched; report it at $NV_ISSUES"
	links_stay_inside "$NV_STAGE" "$NV_TARBALL_NAME"
	if [ ! -f "$_un_top/share/applications/$NV_MAIN_DESKTOP" ] && [ -f "$_un_top/share/applications/$NV_OLD_DESKTOP" ]; then
		die "$NV_TARBALL_NAME is from a release before Eitri's app icon (its desktop entry is share/applications/$NV_OLD_DESKTOP, not $NV_MAIN_DESKTOP), which this installer does not lay out. $(before_icon_advice 'that release with its own install.sh, a release asset')"
	fi
	if [ ! -f "$_un_top/bin/eitri" ]; then die "$NV_TARBALL_NAME has no bin/eitri: it is not a valid Eitri release. Nothing was changed; report it at $NV_ISSUES"; fi
	# A file the layout gained in NV_LAYOUT_SINCE is missing from a release that says it is older: that
	# release is old, not broken. (refuse_older_layout stops such a version before it is downloaded; this
	# is the same answer for a route that reaches here.)
	_un_why="it is not a valid Eitri release. Nothing was changed; report it at $NV_ISSUES"
	older_layout "$NV_VERSION"
	if [ "$OL_OLDER" = 1 ]; then
		_un_how=$(older_layout_how "$NV_VERSION")
		_un_why="Eitri $NV_VERSION is older than this installer installs (it lays out releases from $NV_LAYOUT_SINCE on). Nothing was changed; install $_un_how"
	fi
	for _un_f in $NV_DESKTOP; do
		if [ ! -f "$_un_top/share/applications/$_un_f" ]; then die "$NV_TARBALL_NAME has no share/applications/$_un_f: $_un_why"; fi
	done
	for _un_f in $NV_PLUGIN_FILES; do
		if [ ! -f "$_un_top/share/eitri/eitri.nvim/$_un_f" ]; then die "$NV_TARBALL_NAME has no share/eitri/eitri.nvim/$_un_f: $_un_why"; fi
	done
	for _un_f in $NV_EXT_FILES; do
		if [ ! -f "$_un_top/share/gnome-shell/extensions/$NV_EXT_ID/$_un_f" ]; then die "$NV_TARBALL_NAME has no share/gnome-shell/extensions/$NV_EXT_ID/$_un_f: it is not a valid Eitri release. Nothing was changed; report it at $NV_ISSUES"; fi
	done
	for _un_f in $NV_ICON_FILES; do
		if [ ! -f "$_un_top/share/icons/$_un_f" ]; then die "$NV_TARBALL_NAME has no share/icons/$_un_f: it is not a valid Eitri release. Nothing was changed; report it at $NV_ISSUES"; fi
	done
	for _un_f in $NV_BINARIES eitri-setup; do
		# Check supplied companions even when the release line permits their absence.
		companion_required "$_un_f"
		if [ "$NV_COMPANION_REQUIRED" = 0 ] && [ ! -e "$_un_top/lib/eitri/$_un_f" ] &&
			[ ! -L "$_un_top/lib/eitri/$_un_f" ]; then continue; fi
		if [ -f "$_un_top/lib/eitri/$_un_f" ] && [ ! -x "$_un_top/lib/eitri/$_un_f" ] &&
			[ -n "$(find "$_un_top/lib/eitri/$_un_f" -maxdepth 0 -perm -u+x 2>/dev/null)" ]; then
			# Its mode says executable and the system will not run it: the file system under the cache is mounted noexec.
			die "lib/eitri/$_un_f came out of $NV_TARBALL_NAME with its executable bit, yet cannot be run from $NV_STAGE: the file system holding it is probably mounted noexec. Eitri installs and runs from under $NV_HOME, so that file system has to allow running programs (check with: findmnt -T $NV_STAGE). Nothing was changed. If it is not mounted noexec, report it at $NV_ISSUES"
		fi
		if [ ! -f "$_un_top/lib/eitri/$_un_f" ] || [ ! -x "$_un_top/lib/eitri/$_un_f" ]; then
			die "$NV_TARBALL_NAME has no executable lib/eitri/$_un_f: it is not a valid Eitri release. Nothing was changed; report it at $NV_ISSUES"
		fi
	done
	for _un_f in $(licence_files); do
		if [ ! -f "$_un_top/share/licenses/eitri/$_un_f" ]; then die "$NV_TARBALL_NAME has no share/licenses/eitri/$_un_f: it is not a valid Eitri release. Nothing was changed; report it at $NV_ISSUES"; fi
	done
	if [ ! -d "$_un_top/lib/eitri/plugins" ]; then
		die "$NV_TARBALL_NAME is from a release before Eitri shipped plugins (it has no lib/eitri/plugins/), which this installer does not lay out. Nothing was changed. To install that release, use its own install.sh (a release asset)."
	fi
	for _un_f in $NV_SHIPPED_PLUGINS; do
		if [ ! -f "$_un_top/lib/eitri/plugins/$_un_f/eitri-plugin.toml" ]; then die "$NV_TARBALL_NAME has no lib/eitri/plugins/$_un_f/eitri-plugin.toml: $_un_why"; fi
	done
	if [ ! -f "$_un_top/lib/eitri/RELEASE" ]; then die "$NV_TARBALL_NAME has no lib/eitri/RELEASE: it is not a valid Eitri release. Nothing was changed; report it at $NV_ISSUES"; fi
	parse_release "$_un_top/lib/eitri/RELEASE"
	if [ "$REL_VERSION" != "$NV_VERSION" ]; then
		die "$NV_TARBALL_NAME's RELEASE says $REL_VERSION, not $NV_VERSION: refusing. Report it at $NV_ISSUES"
	fi
	NV_NEW_REV7=$REL_REV7
	mkdir -p -- "$NV_LIBROOT" || die "cannot create $NV_LIBROOT"
	NV_NEW_CREATED=1
	cp -R -- "$_un_top/lib/eitri" "$NV_LIB.new" || die "cannot copy the new install to $NV_LIB.new (is the disk full?)"
	for _un_f in $NV_OLD_BINARIES; do
		rm -f -- "$NV_LIB.new/$_un_f" || die "cannot remove the retired companion $_un_f from $NV_LIB.new"
	done
}

# swap_in: the swap of the new install into place.
swap_in() {
	if [ -e "$NV_LIB.old" ] || [ -L "$NV_LIB.old" ]; then run rm -rf -- "$NV_LIB.old"; fi
	if [ -e "$NV_LIB" ] || [ -L "$NV_LIB" ]; then run mv -- "$NV_LIB" "$NV_LIB.old"; fi
	run mv -- "$NV_LIB.new" "$NV_LIB"
	NV_NEW_CREATED=0
	if [ -e "$NV_LIB.old" ] || [ -L "$NV_LIB.old" ]; then run rm -rf -- "$NV_LIB.old"; fi
}

# desktop_exec_quote PATH: PATH as one quoted Exec argument, by the Desktop Entry Specification's
# rules. Inside double quotes `"`, `` ` ``, `$` and `\` take a backslash; then the string-value
# escape rule applies on top, doubling every backslash (the Desktop Entry rules' "\\\\" for a literal
# backslash). GLib refuses a single `\$` outright, measured: "Key file contains key Exec which has
# a value that cannot be interpreted". A literal `%` is `%%`.
desktop_exec_quote() {
	_dq=$(printf '%s\n' "$1" | sed -e 's/[\\"`$]/\\&/g' -e 's/\\/\\\\/g' -e 's/%/%%/g') || die "sed failed"
	printf '"%s"\n' "$_dq"
}

# older_layout VERSION: OL_OLDER=1 when VERSION is below NV_LAYOUT_SINCE, that line's candidates
# included. Rule 2: a variable.
older_layout() {
	OL_OLDER=0
	_ol_cmp=$(semver_cmp "$1" "$NV_LAYOUT_SINCE-rc.0")
	if [ "$_ol_cmp" -lt 0 ]; then OL_OLDER=1; fi
}

# older_layout_how VERSION: how to install VERSION with its own installer: the release asset from the base
# URL in use, and, for the public repository, the file at the tag.
older_layout_how() {
	_olh_base=${NV_BASE:-$NV_DEFAULT_BASE_URL}
	_olh_how="$1 with its own install.sh: the release asset $_olh_base/releases/download/v$1/install.sh"
	if [ "$_olh_base" = "$NV_DEFAULT_BASE_URL" ]; then
		_olh_how="$_olh_how, or the file at the tag, https://raw.githubusercontent.com/HunterGrey-cyber/eitri/v$1/packaging/install.sh"
	fi
	if [ "$OPT_MODE" = from-source ]; then _olh_how="$_olh_how (with --from-source --version $1)"; fi
	printf '%s\n' "$_olh_how"
}

# refuse_older_layout VERSION: when VERSION is older than this installer lays out, stop before its
# tarball is fetched and before anything is changed, saying how to install it instead.
refuse_older_layout() {
	older_layout "$1"
	if [ "$OL_OLDER" != 1 ]; then return 0; fi
	_rol_how=$(older_layout_how "$1")
	die "Eitri $1 is older than this installer installs: it lays out releases from $NV_LAYOUT_SINCE on, which carry the page window's desktop entry, the nvim plugin's health check and the agents plugins that $1 does not. $(before_icon_advice "$_rol_how")"
}

# refuse_older_layout_early: what the command line alone says about the version (--version, or the version in
# the --tarball file's name) is judged before the install lock is taken and before an interrupted run is
# recovered, so this refusal changes nothing, not even a --keep-build tree a later cleanup would remove. The
# latest version is known only from SHA256SUMS, after the lock (obtain_release).
refuse_older_layout_early() {
	if [ -n "$OPT_VERSION" ]; then
		refuse_older_layout "$OPT_VERSION"
		return 0
	fi
	if [ -n "$OPT_TARBALL" ]; then
		_role_v=${OPT_TARBALL##*/}
		_role_v=${_role_v#eitri-}
		_role_v=${_role_v%-x86_64-linux.tar.gz}
		if printf '%s\n' "$_role_v" | grep -Eqx "$NV_VERSION_ERE"; then refuse_older_layout "$_role_v"; fi
	fi
}

# before_icon_advice WHAT: the way back to a release (or a source tree) from before the app icon, whose
# layout -- the desktop entry eitri.desktop, no icons -- this installer does not lay out. Its own
# installer would put eitri.desktop beside the cn.huntergrey.eitri.desktop and the icons this one put
# there, so --uninstall comes first; WHAT is what to install after it. The list says what --uninstall
# (do_uninstall, without --purge) removes and keeps.
before_icon_advice() {
	printf '%s' "Installing it with its own installer would leave any desktop entry and icons this installer installed beside its own. Nothing was changed. To go back to it, first run: eitri setup --uninstall (or, with this installer: sh install.sh --uninstall). It removes the launcher, the program in $NV_LIB, the desktop entries and icons, the licences, the private nvim and the sidecars Eitri built, and keeps your settings ($NV_CONFIG) and state ($NV_STATE/eitri); then install $1"
}

# old_desktop_text: the eitri.desktop an installer before the app icon wrote for this user, byte for
# byte: those releases' share/applications/eitri.desktop (packaging/legacy/eitri.desktop in this tree,
# which a test holds equal to this text) with its Exec line rewritten, as stage_files does, to the
# quoted launcher path and --quiet %f. Every release that installed the entry under that name wrote
# exactly this.
old_desktop_text() {
	printf '%s\n' '[Desktop Entry]' 'Type=Application' 'Name=Eitri' \
		'Comment=Neovim, embedded, with a Claude agent panel' \
		"Exec=$(desktop_exec_quote "$NV_BINDIR/eitri") --quiet %f" \
		'Terminal=false' 'Categories=Development;TextEditor;' 'MimeType=inode/directory;' 'StartupNotify=true'
}

# stdin_sha256: the SHA-256 of standard input, byte for byte (the output of old_desktop_text keeps its
# trailing newline through the pipe, which a $(...) would not).
stdin_sha256() {
	if command -v sha256sum >/dev/null 2>&1; then
		_ss_out=$(sha256sum) || die "sha256sum failed"
	elif command -v shasum >/dev/null 2>&1; then
		_ss_out=$(shasum -a 256) || die "shasum failed"
	else
		die "neither sha256sum nor shasum was found: install coreutils (or perl's shasum) and re-run"
	fi
	printf '%s\n' "${_ss_out%% *}"
}

# old_desktop_state: NV_OLD_DESKTOP_STATE is none, ours or foreign for $NV_DATA/applications/eitri.desktop,
# the name earlier releases installed the desktop entry under (it is cn.huntergrey.eitri.desktop now).
# "ours" is a regular file whose bytes are exactly old_desktop_text's: the whole file, compared by hash,
# so a copy with a line removed, added, reordered or edited, or with another line ending, is not. Anything
# else -- a symlink, a hand-written or edited entry, one for another launcher -- is foreign, and is never
# removed: it is the user's, whatever it is called.
old_desktop_state() {
	NV_OLD_DESKTOP_STATE=none
	_od_f=$NV_DATA/applications/$NV_OLD_DESKTOP
	if [ ! -e "$_od_f" ] && [ ! -L "$_od_f" ]; then return 0; fi
	NV_OLD_DESKTOP_STATE=foreign
	if [ -L "$_od_f" ] || [ ! -f "$_od_f" ] || [ ! -r "$_od_f" ]; then return 0; fi
	_od_have=$(file_sha256 "$_od_f")
	_od_want=$(old_desktop_text | stdin_sha256)
	if [ "$_od_have" = "$_od_want" ]; then NV_OLD_DESKTOP_STATE=ours; fi
}

# remove_old_desktop: an install over one from before the app icon removes the old eitri.desktop, when
# it is ours (old_desktop_state), so the app grid does not show Eitri twice. A dock or dash entry
# pinned from it is gone with it and has to be pinned again once: its desktop id changed.
remove_old_desktop() {
	old_desktop_state
	case $NV_OLD_DESKTOP_STATE in
	ours)
		say "removing the old desktop entry $NV_DATA/applications/$NV_OLD_DESKTOP: Eitri's is $NV_MAIN_DESKTOP now, so a launcher pinned to the dash or dock has to be pinned again once"
		run rm -f -- "$NV_DATA/applications/$NV_OLD_DESKTOP"
		;;
	foreign)
		say "$NV_DATA/applications/$NV_OLD_DESKTOP was left alone: it is not the entry an earlier Eitri installed (it is not byte for byte what an earlier installer wrote), so it may list Eitri a second time; remove it yourself if it is stale"
		;;
	esac
}

# refresh_icon_cache: gtk-update-icon-cache -f -t on this user's hicolor directory (only that one),
# where the tool exists. -t is --ignore-theme-index: a user directory has no index.theme of its own.
# Never fatal and never silent about a failure: the icon shows after the cache is rebuilt by anything,
# and a desktop that scans the directories itself never needed it.
refresh_icon_cache() {
	if ! command -v gtk-update-icon-cache >/dev/null 2>&1; then return 0; fi
	if [ "$OPT_DRY_RUN" = 1 ]; then
		say "would run: gtk-update-icon-cache -f -t $NV_DATA/icons/hicolor"
		return 0
	fi
	gtk-update-icon-cache -f -t "$NV_DATA/icons/hicolor" >/dev/null 2>&1 ||
		warn "gtk-update-icon-cache failed on $NV_DATA/icons/hicolor: the icon appears once the cache is rebuilt (log out and in, or run it yourself)"
}

# refresh_desktop_database: update-desktop-database on this user's applications directory, where the
# tool exists, so the MIME types the entry lists reach Open With without a new login. Never fatal.
refresh_desktop_database() {
	if ! command -v update-desktop-database >/dev/null 2>&1; then return 0; fi
	if [ "$OPT_DRY_RUN" = 1 ]; then
		say "would run: update-desktop-database $NV_DATA/applications"
		return 0
	fi
	update-desktop-database "$NV_DATA/applications" >/dev/null 2>&1 ||
		warn "update-desktop-database failed on $NV_DATA/applications: Eitri appears in Open With once the cache is rebuilt (log out and in, or run it yourself)"
}

# tidy_icon_dirs: after --uninstall removed the icon files, the empty directories it emptied go (rmdir
# only ever removes an empty one), and so does the cache gtk-update-icon-cache wrote -- when nothing
# else is left in hicolor, which is then removed too. When another application's icons are there, the
# cache is rebuilt for them instead.
tidy_icon_dirs() {
	_ti_h=$NV_DATA/icons/hicolor
	if [ -L "$_ti_h" ] || [ ! -d "$_ti_h" ]; then return 0; fi
	for _ti_a in "$_ti_h"/*/apps; do
		if [ -d "$_ti_a" ]; then
			rmdir -- "$_ti_a" 2>/dev/null || :
			rmdir -- "${_ti_a%/apps}" 2>/dev/null || :
		fi
	done
	_ti_left=$(ls -A -- "$_ti_h" 2>/dev/null) || _ti_left=
	case $_ti_left in
	icon-theme.cache)
		rm -f -- "$_ti_h/icon-theme.cache" || :
		rmdir -- "$_ti_h" 2>/dev/null || :
		rmdir -- "$NV_DATA/icons" 2>/dev/null || :
		;;
	'')
		rmdir -- "$_ti_h" 2>/dev/null || :
		rmdir -- "$NV_DATA/icons" 2>/dev/null || :
		;;
	*) refresh_icon_cache ;;
	esac
}

# desktop_exec_line EXEC FILE: the Exec= line an installed desktop entry gets, EXEC being the launcher's
# quoted absolute path. The panel's entry runs the `panel` subcommand, which has to come first; the
# page window's entry (a bare `eitri %f` in the tarball) takes the launcher then --quiet and keeps its %f; the main
# entry, which opens files from outside, also says --from-desktop and takes %F.
desktop_exec_line() {
	case $2 in
	cn.huntergrey.eitri.Panel.desktop) printf 'Exec=%s panel --quiet %%f\n' "$1" ;;
	cn.huntergrey.eitri.Page.desktop) printf 'Exec=%s --quiet %%f\n' "$1" ;;
	*) printf 'Exec=%s --quiet --from-desktop %%F\n' "$1" ;;
	esac
}

# The plugin's directory, $NV_DATA/eitri/eitri.nvim, is written (and removed) only when it is not a
# symlink: a link there is the user's own checkout of the plugin (a plugin manager's `dir =`, a
# development tree), and writing through it would change their files. It is left alone, and named.
# The GNOME Shell extension's directory, $NV_DATA/gnome-shell/extensions/$NV_EXT_ID, follows the same
# rule: a symlink there is the user's own checkout of the extension and is not written, removed or
# counted by install_complete.
# stage_files writes the launcher, desktop entries with escaped Exec, nvim plugin, icons and
# licences to temporary names in their destination directories. Do this before swapping:
# an unwritable destination must preserve the old install and its rollback copy instead of
# leaving new libraries with an old launcher or notices. commit_files renames after the swap;
# on_exit removes leftover temporary files.
stage_files() {
	NV_TMP_LAUNCHER=$NV_BINDIR/.eitri.tmp.$$
	_sf_apps=$NV_DATA/applications
	_sf_lic=$NV_DATA/licenses/eitri
	_sf_exec=$(desktop_exec_quote "$NV_BINDIR/eitri")
	run mkdir -p -- "$NV_BINDIR" "$_sf_apps" "$_sf_lic"
	if [ "$OPT_DRY_RUN" = 1 ]; then
		say "would write the launcher $NV_BINDIR/eitri (from the tarball's bin/eitri, with the line '$NV_LAUNCHER_MARKER')"
		for _sf_f in $NV_DESKTOP; do say "would write $_sf_apps/$_sf_f with $(desktop_exec_line "$_sf_exec" "$_sf_f")"; done
		if [ ! -L "$NV_DATA/eitri/eitri.nvim" ]; then
			for _sf_f in $NV_PLUGIN_FILES; do say "would write $NV_DATA/eitri/eitri.nvim/$_sf_f"; done
		else
			say "would leave $NV_DATA/eitri/eitri.nvim alone: it is a symlink"
		fi
		if [ ! -L "$NV_DATA/gnome-shell/extensions/$NV_EXT_ID" ]; then
			for _sf_f in $NV_EXT_FILES; do say "would write $NV_DATA/gnome-shell/extensions/$NV_EXT_ID/$_sf_f"; done
		else
			say "would leave $NV_DATA/gnome-shell/extensions/$NV_EXT_ID alone: it is a symlink"
		fi
		for _sf_f in $NV_ICON_FILES; do say "would write $NV_DATA/icons/$_sf_f"; done
		for _sf_f in $(licence_files); do say "would write $_sf_lic/$_sf_f"; done
		return 0
	fi
	_sf_keep='nothing installed was changed: make it writable and re-run'
	cat -- "$NV_STAGE_TOP/bin/eitri" >"$NV_TMP_LAUNCHER" || die "cannot write $NV_TMP_LAUNCHER; $_sf_keep"
	if ! grep -Fqx -- "$NV_LAUNCHER_MARKER" "$NV_TMP_LAUNCHER"; then
		# Appended, not inserted: the launcher's --help prints its own doc-comment lines near the
		# top by a fixed line range, and inserting the marker there would shift it.
		printf '\n%s\n' "$NV_LAUNCHER_MARKER" >>"$NV_TMP_LAUNCHER" || die "cannot write $NV_TMP_LAUNCHER; $_sf_keep"
	fi
	chmod 0755 "$NV_TMP_LAUNCHER" || die "cannot chmod $NV_TMP_LAUNCHER; $_sf_keep"
	for _sf_f in $NV_DESKTOP; do
		_sf_line=$(desktop_exec_line "$_sf_exec" "$_sf_f")
		NV_EXEC=$_sf_line awk '
			/^Exec=/ { print ENVIRON["NV_EXEC"]; next }
			/^TryExec=/ { next }
			{ print }' "$NV_STAGE_TOP/share/applications/$_sf_f" >"$_sf_apps/.$_sf_f.tmp.$$" ||
			die "cannot write $_sf_apps/.$_sf_f.tmp.$$; $_sf_keep"
	done
	if [ ! -L "$NV_DATA/eitri/eitri.nvim" ]; then
		for _sf_f in $NV_PLUGIN_FILES; do
			mkdir -p -- "$NV_DATA/eitri/eitri.nvim/${_sf_f%/*}" || die "cannot create $NV_DATA/eitri/eitri.nvim/${_sf_f%/*}; $_sf_keep"
			cp -- "$NV_STAGE_TOP/share/eitri/eitri.nvim/$_sf_f" "$NV_DATA/eitri/eitri.nvim/${_sf_f%/*}/.${_sf_f##*/}.tmp.$$" ||
				die "cannot write $NV_DATA/eitri/eitri.nvim/${_sf_f%/*}/.${_sf_f##*/}.tmp.$$; $_sf_keep"
		done
	else
		warn "$NV_DATA/eitri/eitri.nvim is a symlink, so the plugin was not installed over it: it is yours, and it is left as it is"
	fi
	if [ ! -L "$NV_DATA/gnome-shell/extensions/$NV_EXT_ID" ]; then
		mkdir -p -- "$NV_DATA/gnome-shell/extensions/$NV_EXT_ID" || die "cannot create $NV_DATA/gnome-shell/extensions/$NV_EXT_ID; $_sf_keep"
		for _sf_f in $NV_EXT_FILES; do
			cp -- "$NV_STAGE_TOP/share/gnome-shell/extensions/$NV_EXT_ID/$_sf_f" "$NV_DATA/gnome-shell/extensions/$NV_EXT_ID/.$_sf_f.tmp.$$" ||
				die "cannot write $NV_DATA/gnome-shell/extensions/$NV_EXT_ID/.$_sf_f.tmp.$$; $_sf_keep"
		done
	else
		warn "$NV_DATA/gnome-shell/extensions/$NV_EXT_ID is a symlink, so the GNOME Shell extension was not installed over it: it is yours, and it is left as it is"
	fi
	NV_STAGED_LICENCES=$(licence_files)
	for _sf_f in $NV_STAGED_LICENCES; do
		cp -- "$NV_STAGE_TOP/share/licenses/eitri/$_sf_f" "$_sf_lic/.$_sf_f.tmp.$$" ||
			die "cannot write $_sf_lic/.$_sf_f.tmp.$$; $_sf_keep"
	done
	for _sf_f in $NV_ICON_FILES; do
		mkdir -p -- "$NV_DATA/icons/${_sf_f%/*}" || die "cannot create $NV_DATA/icons/${_sf_f%/*}; $_sf_keep"
		cp -- "$NV_STAGE_TOP/share/icons/$_sf_f" "$NV_DATA/icons/${_sf_f%/*}/.${_sf_f##*/}.tmp.$$" ||
			die "cannot write $NV_DATA/icons/${_sf_f%/*}/.${_sf_f##*/}.tmp.$$; $_sf_keep"
	done
}

# commit_files: after the swap, stage_files' files are renamed into place (every executable outside
# $NV_LIB goes in by temporary name, chmod, then mv). A launcher that is not
# Eitri's -- no marker, not the old root install.sh's -- is moved aside, never overwritten.
commit_files() {
	_cf_dst=$NV_BINDIR/eitri
	if [ -L "$_cf_dst" ] || [ -e "$_cf_dst" ]; then
		_cf_ours=0
		if [ ! -L "$_cf_dst" ] && [ -f "$_cf_dst" ]; then
			if grep -Fqx -- "$NV_LAUNCHER_MARKER" "$_cf_dst"; then _cf_ours=1; fi
			_cf_l2=$(sed -n '2p' "$_cf_dst") || _cf_l2=
			if [ "$_cf_l2" = "$NV_OLD_LAUNCHER_LINE2" ]; then _cf_ours=1; fi
		fi
		if [ "$_cf_ours" = 0 ]; then
			_cf_bak=$NV_BINDIR/eitri.bak-$(date +%Y%m%d-%H%M%S)
			if [ -e "$_cf_bak" ] || [ -L "$_cf_bak" ]; then _cf_bak=$_cf_bak-$$; fi
			warn "$_cf_dst was not installed by this installer, so it is moved to $_cf_bak rather than overwritten; delete it once you no longer need it"
			run mv -- "$_cf_dst" "$_cf_bak"
		fi
	fi
	if [ "$OPT_DRY_RUN" = 1 ]; then
		remove_old_desktop
		refresh_icon_cache
		refresh_desktop_database
		return 0
	fi
	# The new lib is in place from here on: a failure leaves the install unfinished, and a re-run
	# finishes it (install_complete keeps it from saying "up to date" instead).
	_cf_fin="Eitri $NV_VERSION is in $NV_LIB but not finished: fix this and re-run the installer to finish it"
	mv -- "$NV_TMP_LAUNCHER" "$_cf_dst" || die "cannot move the launcher into $_cf_dst; $_cf_fin"
	for _cf_f in $NV_DESKTOP; do
		mv -- "$NV_DATA/applications/.$_cf_f.tmp.$$" "$NV_DATA/applications/$_cf_f" ||
			die "cannot write $NV_DATA/applications/$_cf_f; $_cf_fin"
	done
	if [ ! -L "$NV_DATA/eitri/eitri.nvim" ]; then
		for _cf_f in $NV_PLUGIN_FILES; do
			mv -- "$NV_DATA/eitri/eitri.nvim/${_cf_f%/*}/.${_cf_f##*/}.tmp.$$" "$NV_DATA/eitri/eitri.nvim/$_cf_f" ||
				die "cannot write $NV_DATA/eitri/eitri.nvim/$_cf_f; $_cf_fin"
		done
	fi
	if [ ! -L "$NV_DATA/gnome-shell/extensions/$NV_EXT_ID" ]; then
		for _cf_f in $NV_EXT_FILES; do
			mv -- "$NV_DATA/gnome-shell/extensions/$NV_EXT_ID/.$_cf_f.tmp.$$" "$NV_DATA/gnome-shell/extensions/$NV_EXT_ID/$_cf_f" ||
				die "cannot write $NV_DATA/gnome-shell/extensions/$NV_EXT_ID/$_cf_f; $_cf_fin"
		done
	fi
	for _cf_f in $NV_ICON_FILES; do
		mv -- "$NV_DATA/icons/${_cf_f%/*}/.${_cf_f##*/}.tmp.$$" "$NV_DATA/icons/$_cf_f" ||
			die "cannot write $NV_DATA/icons/$_cf_f; $_cf_fin"
	done
	for _cf_f in $NV_STAGED_LICENCES; do
		mv -- "$NV_DATA/licenses/eitri/.$_cf_f.tmp.$$" "$NV_DATA/licenses/eitri/$_cf_f" ||
			die "cannot write $NV_DATA/licenses/eitri/$_cf_f; $_cf_fin"
	done
	# After the new entry is in place, so an interruption never leaves neither.
	remove_old_desktop
	refresh_icon_cache
	refresh_desktop_database
	# Write the version stamp last. An interrupt or failed mv leaves it absent or naming an
	# older version, so install_complete sees unfinished work even if every individual file
	# exists from a complete install of a different version.
	printf '%s\n' "$NV_VERSION" >"$NV_DATA/licenses/eitri/.installed-version.tmp.$$" ||
		die "cannot write $NV_DATA/licenses/eitri/.installed-version.tmp.$$; $_cf_fin"
	mv -- "$NV_DATA/licenses/eitri/.installed-version.tmp.$$" "$NV_DATA/licenses/eitri/.installed-version" ||
		die "cannot write $NV_DATA/licenses/eitri/.installed-version; $_cf_fin"
}

# install_complete sets NV_COMPLETE when the launcher marker, desktop entry, the licence
# files (licence_files: LICENSE, THIRD-PARTY-LICENSES, SOURCE and, after 0.3.0, NOTICE) and version
# stamp are present for NV_VERSION. Up-to-date checks need it as well as
# the sidecar so a rerun repairs an interrupted post-swap commit. Existence alone cannot
# distinguish files from an older complete install, especially the release-specific SOURCE.
install_complete() {
	NV_COMPLETE=0
	if [ -L "$NV_BINDIR/eitri" ] || [ ! -f "$NV_BINDIR/eitri" ]; then return 0; fi
	if ! grep -Fqx -- "$NV_LAUNCHER_MARKER" "$NV_BINDIR/eitri"; then return 0; fi
	for _ic_f in $NV_DESKTOP; do
		if [ ! -f "$NV_DATA/applications/$_ic_f" ]; then return 0; fi
	done
	if [ ! -L "$NV_DATA/eitri/eitri.nvim" ]; then
		for _ic_f in $NV_PLUGIN_FILES; do
			if [ ! -f "$NV_DATA/eitri/eitri.nvim/$_ic_f" ]; then return 0; fi
		done
	fi
	if [ ! -L "$NV_DATA/gnome-shell/extensions/$NV_EXT_ID" ]; then
		for _ic_f in $NV_EXT_FILES; do
			if [ ! -f "$NV_DATA/gnome-shell/extensions/$NV_EXT_ID/$_ic_f" ]; then return 0; fi
		done
	fi
	for _ic_f in $NV_ICON_FILES; do
		if [ ! -f "$NV_DATA/icons/$_ic_f" ]; then return 0; fi
	done
	for _ic_f in $(licence_files); do
		if [ ! -f "$NV_DATA/licenses/eitri/$_ic_f" ]; then return 0; fi
	done
	_ic_stamp=$(cat -- "$NV_DATA/licenses/eitri/.installed-version" 2>/dev/null) || _ic_stamp=
	if [ "$_ic_stamp" != "$NV_VERSION" ]; then return 0; fi
	NV_COMPLETE=1
}

# sidecar_root_ours sets SRO=1 when the sidecar root resolves to a directory named sidecar, as
# do_uninstall checks the eitri data root; otherwise it warns and sets SRO=0. A user-created link
# elsewhere could contain unrelated directories named like a rev or a version, and treating those
# names as ours would remove the user's files.
sidecar_root_ours() {
	_sro_real=$(cd -P -- "$NV_SIDECAR_ROOT" 2>/dev/null && pwd -P) || _sro_real=
	case $_sro_real in
	*/sidecar) SRO=1 ;;
	*)
		SRO=0
		warn "$NV_SIDECAR_ROOT leads to ${_sro_real:-a directory that cannot be entered}, not to a directory named sidecar, so nothing behind it was removed: this installer never made that link. Remove what Eitri put there yourself if you no longer need it"
		;;
	esac
}

# prune_sidecars: a sidecar <rev7> directory goes only when no installed RELEASE
# names it, it is not the new install's rev, and it is not the rev of the install just replaced
# (kept until the next successful upgrade: a .deb and a tarball install share this directory, and
# running windows still use the replaced one). Anything not named like a rev is not ours to judge.
prune_sidecars() {
	if [ ! -d "$NV_SIDECAR_ROOT" ]; then return 0; fi
	sidecar_root_ours
	if [ "$SRO" != 1 ]; then return 0; fi
	for _pr_d in "$NV_SIDECAR_ROOT"/*; do
		if [ ! -d "$_pr_d" ] || [ -L "$_pr_d" ]; then continue; fi
		_pr_n=${_pr_d##*/}
		case $_pr_n in
		[0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]) ;;
		*) continue ;;
		esac
		if [ "$_pr_n" = "$NV_NEW_REV7" ] || [ "$_pr_n" = "$NV_OLD_REV7" ] || [ "$_pr_n" = "$NV_SYS_REV7" ]; then continue; fi
		if [ -z "$NV_NEW_REV7" ]; then
			# Only a dry run that could not read the new RELEASE gets here (dry_run_new_rev).
			say "removing the sidecar for verdandi $_pr_n, unless the new release uses it (this dry run could not tell)"
		else
			say "removing the sidecar for verdandi $_pr_n: no installed Eitri uses it"
		fi
		run rm -rf -- "$_pr_d"
	done
}

# path_warning: report which eitri this PATH runs.
path_warning() {
	_pw_first=
	_pw_ifs=$IFS
	IFS=:
	set -f
	for _pw_d in $PATH; do
		_pw_d=${_pw_d%/}
		case $_pw_d in /*) ;; *) continue ;; esac
		if [ -f "$_pw_d/eitri" ] && [ -x "$_pw_d/eitri" ]; then
			_pw_first=$_pw_d/eitri
			break
		fi
	done
	set +f
	IFS=$_pw_ifs
	if [ -z "$_pw_first" ]; then
		warn "$NV_BINDIR is not on your PATH: add it (in your shell's profile: export PATH=\"\$HOME/.local/bin:\$PATH\"), or run $NV_BINDIR/eitri"
	elif [ "$_pw_first" != "$NV_BINDIR/eitri" ]; then
		warn "\`eitri\` on this PATH runs $_pw_first, not the one just installed: put $NV_BINDIR before ${_pw_first%/eitri} in PATH, remove the other install, or run $NV_BINDIR/eitri"
	fi
}

# ---------------------------------------------------------------------------------------------
# Install / upgrade

do_install() {
	check_platform
	check_glibc
	check_gtk_webkit
	nvim_check
	if [ -z "$OPT_TARBALL" ]; then
		require_curl
		check_base_url "${OPT_BASE_URL:-$NV_DEFAULT_BASE_URL}"
	fi
	refuse_older_layout_early
	acquire_lock
	recover_interrupted
	# Read now, before anything is downloaded or changed: prune_sidecars keeps this rev, and a
	# system RELEASE that cannot be read stops the run here rather than after the swap.
	rev_of_release "$NV_SYSTEM_RELEASE" "$NV_SYSTEM_REMEDY"
	NV_SYS_REV7=$RR_REV7

	NV_INSTALLED_VERSION=
	NV_OLD_REV7=
	if [ -f "$NV_LIB/RELEASE" ]; then
		NV_INSTALLED_VERSION=$(kv_get "$NV_LIB/RELEASE" EITRI_VERSION)
		if ! printf '%s\n' "$NV_INSTALLED_VERSION" | grep -Eqx "$NV_VERSION_ERE"; then NV_INSTALLED_VERSION=; fi
		rev_of_release "$NV_LIB/RELEASE" "remove $NV_LIB (only the program: your settings and state live elsewhere)"
		NV_OLD_REV7=$RR_REV7
	elif [ -e "$NV_LIB" ]; then
		say "$NV_LIB holds an install without a RELEASE (a development install); it will be replaced"
	fi

	obtain_release
	if [ -n "$NV_INSTALLED_VERSION" ]; then
		_di_c=$(semver_cmp "$NV_VERSION" "$NV_INSTALLED_VERSION")
		if [ "$_di_c" = -1 ] && [ -z "$OPT_VERSION" ]; then
			die "Eitri $NV_INSTALLED_VERSION is installed, and the release offered is older ($NV_VERSION): refusing to downgrade silently (a stale mirror or a withdrawn release would look like this). To install $NV_VERSION anyway: --version $NV_VERSION"
		fi
		if [ "$_di_c" = 0 ]; then
			sidecar_state "$NV_OLD_REV7"
			install_complete
			if [ "$NV_SIDECAR_PRESENT" = 1 ] && [ "$NV_COMPLETE" = 1 ]; then
				say "Eitri $NV_VERSION is up to date"
				# Offer nvim before returning even when Eitri itself is up to date, so --with-nvim/--yes
				# can complete a --no-nvim install or retry after network access returns. Read the installed
				# RELEASE: it matches the version just offered and supplies the correct pinned nvim.
				if [ "$NV_NVIM_OK" != 1 ]; then
					parse_release "$NV_LIB/RELEASE"
					NV_NEW_REV7=$REL_REV7
					nvim_private_state "$REL_NVIM_VERSION"
					if [ "$NV_NVIM_PRIVATE_PRESENT" = 1 ]; then
						say "nvim $REL_NVIM_VERSION: already installed at $NV_NVIM_PRIVATE_BIN"
					else
						if [ "$OPT_NVIM" != no ]; then require_curl; fi
						maybe_offer_nvim
					fi
				fi
				report_claude "$NV_SIDECAR_BIN"
				# A re-run is how a user asks again: an install from before these steps existed,
				# or one whose steps were never run, gets them here too (or hears the profile is
				# in place), last as after any install.
				apparmor_note
				return 0
			fi
			if [ "$NV_COMPLETE" != 1 ]; then
				say "Eitri $NV_VERSION is installed but not finished (its launcher, desktop entry, icon or licences are missing): installing it again"
			fi
		fi
	fi

	if [ -n "$OPT_TARBALL" ]; then
		if [ "$OPT_DRY_RUN" = 1 ]; then
			NV_TARBALL=$OPT_TARBALL
		else
			# Copied first, then hashed and unpacked from the copy alone: hashed where it lay and
			# unpacked from a second read, a tarball swapped in between was installed (see
			# obtain_release).
			NV_TARBALL=$NV_DL/$NV_TARBALL_NAME
			cp -- "$OPT_TARBALL" "$NV_TARBALL" || die "cannot copy $OPT_TARBALL into $NV_DL: check that the disk is not full, then re-run"
		fi
		verify_file_in_place
	elif [ "$OPT_DRY_RUN" = 1 ]; then
		NV_TARBALL=$NV_DL/$NV_TARBALL_NAME
		_di_h=$(sums_hash "$NV_TARBALL_NAME")
		say "would download $NV_REL_URL/$NV_TARBALL_NAME to $NV_TARBALL.part, check its sha256 is $_di_h, and rename it"
	else
		NV_REL_URL=$NV_BASE/releases/download/v$NV_VERSION
		download_verified "$NV_TARBALL_NAME" "$NV_MAX_DOWNLOAD_BYTES"
		NV_TARBALL=$NV_DL/$NV_TARBALL_NAME
	fi
	say "installing Eitri $NV_VERSION${NV_INSTALLED_VERSION:+ (replacing $NV_INSTALLED_VERSION)}"
	unpack_new
	if [ "$OPT_DRY_RUN" != 1 ]; then use_verdandi_source_beside_tarball; fi
	finish_install
}

# verify_file_in_place: --tarball's file -- in a real run, this run's own copy of it -- checked
# against --sums.
verify_file_in_place() {
	_vp_want=$(sums_hash "$NV_TARBALL_NAME")
	_vp_have=$(file_sha256 "$NV_TARBALL")
	if [ "$_vp_want" != "$_vp_have" ]; then
		die "checksum mismatch for $OPT_TARBALL: SHA256SUMS says $_vp_want, the file is $_vp_have. It is corrupt or was altered, so nothing was installed; download it again"
	fi
}

# ---------------------------------------------------------------------------------------------
# Uninstall

# under_home PATH: 1 when PATH is strictly inside $HOME and has no `.` or `..` component.
under_home() {
	_uh=0
	case $1 in
	"$NV_HOME"/?*)
		case /$1/ in
		*/../* | */./*) ;;
		*) _uh=1 ;;
		esac
		;;
	esac
	printf '%s\n' "$_uh"
}

do_uninstall() {
	# The settings directory is a removal target under --purge and named in every uninstall's
	# messages, so a newline in it is refused here as xdg_dir refuses one in the other XDG variables.
	case $NV_CONFIG in
	*"$NV_NL"*) die "XDG_CONFIG_HOME contains a newline, so no path built from it can be trusted: set XDG_CONFIG_HOME to one absolute directory (or unset it) and re-run" ;;
	esac
	rev_of_release "$NV_SYSTEM_RELEASE" "$NV_SYSTEM_REMEDY"
	_du_sys=$RR_REV7
	_du_launcher_note=
	_du_link_note=
	_du_sidecar_link_note=
	_du_ext_note=
	# Keep uninstall targets as separate positional parameters. Joining then splitting could
	# turn a newline in an XDG value into an unrelated directory to remove; xdg_dir also refuses it.
	set -- "$NV_LIB" "$NV_LIB.new" "$NV_LIB.old"
	if [ -L "$NV_BINDIR/eitri" ] || [ -e "$NV_BINDIR/eitri" ]; then
		if [ ! -L "$NV_BINDIR/eitri" ] && [ -f "$NV_BINDIR/eitri" ] && grep -Fqx -- "$NV_LAUNCHER_MARKER" "$NV_BINDIR/eitri"; then
			set -- "$@" "$NV_BINDIR/eitri"
		else
			_du_launcher_note="$NV_BINDIR/eitri was left alone: it was not installed by this installer (no '$NV_LAUNCHER_MARKER' line)"
		fi
	fi
	for _du_f in $NV_DESKTOP; do set -- "$@" "$NV_DATA/applications/$_du_f"; done
	set -- "$@" "$NV_DATA/licenses/eitri"
	# The entry earlier releases installed as eitri.desktop goes only if it is the one this installer
	# wrote (old_desktop_state); a user's own file of that name is left, and named.
	_du_desktop_note=
	old_desktop_state
	case $NV_OLD_DESKTOP_STATE in
	ours) set -- "$@" "$NV_DATA/applications/$NV_OLD_DESKTOP" ;;
	foreign) _du_desktop_note="$NV_DATA/applications/$NV_OLD_DESKTOP was left alone: it is not the entry an earlier Eitri installed (it is not byte for byte what an earlier installer wrote); remove it yourself if it is stale" ;;
	esac
	# The icon: exactly the files install writes (NV_ICON_FILES), by path -- never a pattern, which would
	# also take a user's own icon of Eitri's name in a size or format this installer never writes
	# (96x96, say). A path of the list holds the file this installer wrote when it is a regular file; a
	# symlink or a directory there is the user's, left in place and named.
	for _du_f in $NV_ICON_FILES; do
		_du_if=$NV_DATA/icons/$_du_f
		if [ -L "$_du_if" ] || { [ -e "$_du_if" ] && [ ! -f "$_du_if" ]; }; then
			say "$_du_if was left alone: it is a symlink or a directory, not the icon file this installer wrote"
		elif [ -e "$_du_if" ]; then
			set -- "$@" "$_du_if"
		fi
	done
	# The GNOME Shell extension's directory goes unless it is a symlink: then it is the user's own checkout.
	_du_ext=$NV_DATA/gnome-shell/extensions/$NV_EXT_ID
	if [ -L "$_du_ext" ]; then
		_du_ext_note="$_du_ext is a symlink, so it was left alone: this installer never made that link"
	else
		set -- "$@" "$_du_ext"
	fi
	# Remove private nvim and sidecars only if the resolved data root is named eitri, even when
	# linked to another disk. A link elsewhere, such as ~/.config, could expose unrelated nvim
	# directories; the installer never creates such a link and must leave its contents alone.
	_du_ours=1
	if [ -d "$NV_DATA/eitri" ]; then
		_du_real=$(cd -P -- "$NV_DATA/eitri" 2>/dev/null && pwd -P) || _du_real=
		case $_du_real in
		*/eitri) ;;
		*)
			_du_ours=0
			_du_link_note="$NV_DATA/eitri leads to ${_du_real:-a directory that cannot be entered}, not to a directory named eitri, so nothing under it (an nvim, sidecars) was removed: this installer never made that link. Remove what Eitri put there yourself if you no longer need it"
			;;
		esac
	fi
	if [ "$_du_ours" = 1 ]; then
		# The AppArmor profile apparmor_note wrote goes too; the copy installed under
		# /etc/apparmor.d is root's, and the note below says how to remove it.
		set -- "$@" "$NV_DATA/eitri/nvim" "$NV_DATA/eitri/apparmor"
		# The plugin goes with them, unless that path is a link: then it is the user's own checkout.
		if [ ! -L "$NV_DATA/eitri/eitri.nvim" ]; then
			set -- "$@" "$NV_DATA/eitri/eitri.nvim"
		else
			_du_link_note="$_du_link_note$NV_DATA/eitri/eitri.nvim is a symlink, so it was left alone: this installer never made that link"
		fi
		# The check just above only ever verified
		# $NV_DATA/eitri itself, not $NV_SIDECAR_ROOT (its own "sidecar" subdirectory) -- a
		# symlinked sidecar root, planted by the user in their own $XDG_DATA_HOME, was followed
		# with no check at all, so a 7-hex-named directory BEHIND the link (a user's own,
		# unrelated directory merely named like a short git sha) was removed as if it were one of
		# Eitri's own sidecars. Same rule, same shape: a link is fine as long as it leads to a
		# real directory actually named "sidecar".
		_du_sidecar_ours=0
		if [ -d "$NV_SIDECAR_ROOT" ]; then
			_du_sidecar_real=$(cd -P -- "$NV_SIDECAR_ROOT" 2>/dev/null && pwd -P) || _du_sidecar_real=
			case $_du_sidecar_real in
			*/sidecar) _du_sidecar_ours=1 ;;
			*) _du_sidecar_link_note="$NV_SIDECAR_ROOT leads to ${_du_sidecar_real:-a directory that cannot be entered}, not to a directory named sidecar, so nothing behind it was removed: this installer never made that link. Remove what Eitri put there yourself if you no longer need it" ;;
			esac
		fi
		if [ "$_du_sidecar_ours" = 1 ]; then
			for _du_d in "$NV_SIDECAR_ROOT"/*; do
				if [ ! -d "$_du_d" ] || [ -L "$_du_d" ]; then continue; fi
				_du_n=${_du_d##*/}
				case $_du_n in
				[0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]) ;;
				*) continue ;;
				esac
				if [ "$_du_n" = "$_du_sys" ]; then
					say "keeping the sidecar for verdandi $_du_n: $NV_SYSTEM_RELEASE's install uses it"
					continue
				fi
				set -- "$@" "$_du_d"
			done
			# A package's window reads these as it reads its sidecar, so they stay with it. Each is
			# removed by its own name: one that is a link goes as a link, and what it leads to stays.
			if [ -z "$_du_sys" ]; then
				set -- "$@" "$NV_SIDECAR_UPDATES" "$NV_SIDECAR_SELECTED"
				for _du_d in "$NV_SIDECAR_ROOT"/.selected.tmp.*; do set -- "$@" "$_du_d"; done
			elif [ -e "$NV_SIDECAR_UPDATES" ] || [ -e "$NV_SIDECAR_SELECTED" ]; then
				say "keeping $NV_SIDECAR_UPDATES and $NV_SIDECAR_SELECTED: $NV_SYSTEM_RELEASE's install uses them"
			fi
		fi
	fi
	# Include all build scratch in uninstall targets, including --keep-build leftovers and
	# scratch from a run interrupted before on_exit, so purge can remove them too.
	for _du_p in download unpack sidecar-build sidecar-update src nvim-build verdandi-src verdandi-src-build \
		from-source-node from-source-skia; do
		set -- "$@" "$NV_CACHE_NV/$_du_p"
	done
	for _du_p in "$NV_CACHE_NV"/sidecar-check.*; do set -- "$@" "$_du_p"; done
	_du_cfg_note=
	if [ "$OPT_PURGE" = 1 ]; then
		# Exactly two: the settings directory Eitri reads when EITRI_CONFIG_DIR is unset (settings_dir:
		# $XDG_CONFIG_HOME/eitri or ~/.config/eitri, so the copy Eitri actually reads goes and any other
		# is left alone), and the state directory by xdg_dir's rule. Never EITRI_CONFIG_DIR itself: it
		# may name a directory another program keeps its settings in, and the user is told it stays.
		set -- "$@" "$NV_CONFIG" "$NV_STATE/eitri"
		if [ -n "${EITRI_CONFIG_DIR-}" ]; then
			_du_ecd=$EITRI_CONFIG_DIR
			while :; do
				case $_du_ecd in
				?*/) _du_ecd=${_du_ecd%/} ;;
				*) break ;;
				esac
			done
			# One that is the settings directory, or inside it, goes with it; saying it stayed would
			# be false.
			case $_du_ecd in
			"$NV_CONFIG" | "$NV_CONFIG"/*) ;;
			*) _du_cfg_note="left $EITRI_CONFIG_DIR alone: EITRI_CONFIG_DIR names it, and that may be a directory another program keeps its settings in. Remove it yourself if it holds only Eitri's settings" ;;
			esac
		fi
	fi

	# Only what exists is removed, so only what exists is checked: a path not there needs nothing,
	# and once the user has removed an outside-$HOME one by hand, as the refusal asks, a re-run
	# goes through (it never could).
	_du_i=$#
	while [ "$_du_i" -gt 0 ]; do
		_du_p=$1
		shift
		if [ -e "$_du_p" ] || [ -L "$_du_p" ]; then set -- "$@" "$_du_p"; fi
		_du_i=$((_du_i - 1))
	done

	# Every path is checked before any is removed -- and before the lock is taken, so a refusal
	# changes nothing at all: taken first, the lock's exit cleanup removed the very cache
	# directories just refused.
	_du_bad=
	for _du_p do
		_du_ok=$(under_home "$_du_p")
		if [ "$_du_ok" != 1 ]; then _du_bad="$_du_bad '$_du_p'"; fi
	done
	if [ -n "$_du_bad" ]; then
		die "refusing to uninstall: these paths are not inside $NV_HOME, and this installer only removes paths inside it:$_du_bad. Nothing was removed. They are Eitri's own: remove them yourself if you no longer need them, then re-run to remove the rest"
	fi
	acquire_lock

	# One `rm -rf -- "$p"` per path.
	_du_n=0
	for _du_p do
		if [ -e "$_du_p" ] || [ -L "$_du_p" ]; then
			say "removing $_du_p"
			run rm -rf -- "$_du_p"
			_du_n=$((_du_n + 1))
		fi
	done
	if [ "$OPT_DRY_RUN" != 1 ]; then
		if [ "$_du_ours" = 1 ]; then
			for _du_p in "$NV_SIDECAR_ROOT" "$NV_DATA/eitri"; do
				if [ -d "$_du_p" ]; then rmdir -- "$_du_p" 2>/dev/null || :; fi
			done
		fi
		if [ -d "$NV_DATA/licenses" ]; then rmdir -- "$NV_DATA/licenses" 2>/dev/null || :; fi
		# $NV_DATA/gnome-shell/extensions is the shell's own directory for every extension of the user:
		# it, and gnome-shell above it, go only when Eitri's was the last thing in them (rmdir removes an
		# empty directory and nothing else, and never a symlink).
		rmdir -- "$NV_DATA/gnome-shell/extensions" 2>/dev/null || :
		rmdir -- "$NV_DATA/gnome-shell" 2>/dev/null || :
		tidy_icon_dirs
	fi
	if [ -n "$_du_desktop_note" ]; then say "$_du_desktop_note"; fi
	if [ -n "$_du_launcher_note" ]; then say "$_du_launcher_note"; fi
	if [ -n "$_du_link_note" ]; then say "$_du_link_note"; fi
	if [ -n "$_du_sidecar_link_note" ]; then say "$_du_sidecar_link_note"; fi
	if [ -n "$_du_ext_note" ]; then say "$_du_ext_note"; fi
	if [ -n "$_du_cfg_note" ]; then say "$_du_cfg_note"; fi
	# A profile this install's steps put under /etc/apparmor.d stays (this installer never runs
	# sudo): it names a path nothing runs from any more, and removing it takes root.
	_du_uid=$(id -u 2>/dev/null) || _du_uid=
	if [ -n "$_du_uid" ] && [ -f "$NV_APPARMOR_D/eitri-user-$_du_uid" ]; then
		_du_aa=$(sh_quote "$NV_APPARMOR_D/eitri-user-$_du_uid")
		say "the AppArmor profile $NV_APPARMOR_D/eitri-user-$_du_uid stays: remove it with: sudo apparmor_parser -R $_du_aa && sudo rm $_du_aa"
	fi
	if [ "$OPT_DRY_RUN" = 1 ]; then
		say "dry run: nothing was changed"
	elif [ "$_du_n" = 0 ]; then
		say "nothing to remove: Eitri is not installed here"
	else
		say "Eitri is uninstalled"
	fi
	if [ "$OPT_PURGE" != 1 ]; then
		say "kept your settings ($NV_CONFIG) and state ($NV_STATE/eitri); --uninstall --purge removes them too"
	fi
}

# ---------------------------------------------------------------------------------------------

main() {
	parse_args "$@"
	if [ "$OPT_HELP" = 1 ]; then
		usage
		return 0
	fi
	check_home
	set_paths
	check_root
	# check_data_home's own scope (its doc comment above) is every mode that writes under NV_DATA:
	# do_install, do_from_source, do_sidecar_only, do_sidecar_update, do_sidecar_use, do_nvim_only,
	# do_nvim_offer. --uninstall is exempt on purpose (its own comment explains why); --sidecar-status
	# writes nothing, and a tab reads that directory wherever it is; --build-sidecar-into is exempt too --
	# neither reads nor writes anything under NV_DATA/XDG_DATA_HOME (its workdir is under
	# NV_CACHE_NV, its output goes to the caller's own --build-sidecar-into DIR) -- it is the AUR
	# package's build() entry point, which can run in a sandbox that sets XDG_DATA_HOME
	# outside HOME; this mode does not use it.
	case $OPT_MODE in
	uninstall | build-sidecar-into | sidecar-status) ;;
	*) check_data_home ;;
	esac
	trap on_exit EXIT
	trap 'exit 129' HUP
	trap 'exit 130' INT
	trap 'exit 143' TERM
	case $OPT_MODE in
	uninstall) do_uninstall ;;
	sidecar-only) do_sidecar_only ;;
	sidecar-update) do_sidecar_update ;;
	sidecar-check) do_sidecar_check ;;
	sidecar-status) do_sidecar_status ;;
	sidecar-use) do_sidecar_use ;;
	build-sidecar-into) do_build_sidecar_into ;;
	nvim-only) do_nvim_only ;;
	nvim-offer) do_nvim_offer ;;
	from-source) do_from_source ;;
	*) do_install ;;
	esac
}
main "$@" </dev/null
}
