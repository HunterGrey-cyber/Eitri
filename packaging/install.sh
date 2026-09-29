#!/bin/sh
# neovibe's installer: download a release, verify it, install it for this user, upgrade it, or
# uninstall it (spec docs/superpowers/specs/2026-09-27-v1-dist-design.md §6). Nothing here runs sudo
# or a package manager: when something is missing it prints the command and stops.
#
#   curl --proto '=https' --tlsv1.2 -fsSL https://github.com/HunterGrey-cyber/neovibe/releases/latest/download/install.sh | sh
#   curl --proto '=https' --tlsv1.2 -fsSL .../install.sh | sh -s -- --version 1.0.0 --yes
#   neovibe setup        (this same file, installed as lib/neovibe/neovibe-setup)
#
# `sh install.sh --help` lists the options.
#
# Three rules plain `set -eu` does not give (spec §6.1); packaging/tests/install/ holds each:
#
# 1. Nothing runs until the whole script has arrived. Everything below is one `{ ... }` group
#    whose last command is `main "$@"` and whose `}` is the file's last line. A shell parses a
#    whole compound command before running any of it, so a truncated `curl | sh` download -- cut
#    anywhere before that `}` -- is a syntax error that runs nothing at all: this file holds
#    `rm -rf`, `mv` swaps and uninstall logic. (With `}; main "$@"` as the last line, a cut right
#    after `}; main` ran main with none of its arguments: Task 9 review.)
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
{
set -eu

NV_DEFAULT_BASE_URL='https://github.com/HunterGrey-cyber/neovibe'
NV_LAUNCHER_MARKER='# neovibe-launcher v1'
# The second line of the launcher the old root install.sh wrote (spec §6.5): recognised as
# neovibe's own and replaced, although it predates the marker.
NV_OLD_LAUNCHER_LINE2='# neovibe, installed by install.sh. Everything it decides is printed before the window opens.'
NV_SIGNER_IDENTITY='release@neovibe'
NV_SIGNATURE_NAMESPACE='neovibe-release'
NV_GTK_FLOOR_MINOR=14
NV_GLIBC_FLOOR='2.39'
NV_NVIM_FLOOR='0.10.0'
NV_BINARIES='shell neovibe-supervisor neovibe-tmux-shim neovibe-claude-handoff'
NV_LICENCE_FILES='LICENSE THIRD-PARTY-LICENSES SOURCE'
NV_VERSION_ERE='[0-9]+\.[0-9]+\.[0-9]+(-rc\.[0-9]+)?'
NV_ISSUES='https://github.com/HunterGrey-cyber/neovibe/issues'
# The sidecar build (spec §5.3). The protocol major matches Verdandi's own
# apps/claude-sidecar/src/runtimeServiceImpl.ts PROTOCOL_MAJOR, checked against a built artifact's
# own --version rather than assumed. Node's dist base is fixed in production; test mode replaces it
# (node_dist_base) so a sandboxed run can never reach the real nodejs.org by accident.
NV_SIDECAR_PROTOCOL_MAJOR=3
NV_NODE_DIST_BASE=https://nodejs.org/dist
NV_SIDECAR_BUILD_MIN_KIB=614400
# The nvim offer (spec §7). Never neovibe's own release server -- this is neovim's own GitHub
# releases. nvim_dist_base's own test-mode default (a refused loopback port) keeps every test that
# forgets to redirect it from reaching the real host, the same discipline as node_dist_base.
NV_NVIM_DIST_BASE=https://github.com/neovim/neovim/releases/download
# --from-source (spec §6.2): the public repositories it clones. Overridable only in test mode
# (NEOVIBE_INSTALL_TEST_REPO_URL / NEOVIBE_INSTALL_TEST_VERDANDI_REPO_URL), which the
# NEOVIBE_INSTALL_TEST_* pattern in run-in-env.sh's allowlist already covers -- no new entry there.
NV_NEOVIBE_REPO_URL=https://github.com/HunterGrey-cyber/neovibe.git
NV_VERDANDI_REPO_URL=https://github.com/HunterGrey-cyber/verdandi.git
NV_MIN_RUSTC=1.96.0
NV_NL='
'
# The AppArmor user-namespace restriction (apparmor_note, below): where the kernel says whether it
# is on, and where a profile is installed. Test mode replaces both (set_paths).
NV_PROC=/proc
NV_APPARMOR_D=/etc/apparmor.d

# Test mode (packaging/tests/install/ only). With NEOVIBE_INSTALL_TEST=1 -- and only then -- three
# more variables are read, each standing in for a file of the host the harness must not depend on:
#   NEOVIBE_INSTALL_TEST_LIBDIRS          colon-separated directories searched for the GTK and
#                                         WebKit sonames instead of `ldconfig -p` and the usual lib
#                                         directories; a directory's `ldconfig-p.txt` is read as
#                                         fake `ldconfig -p` output;
#   NEOVIBE_INSTALL_TEST_SYSTEM_RELEASE   read instead of /usr/lib/neovibe/RELEASE;
#   NEOVIBE_INSTALL_TEST_OS_RELEASE       read instead of /etc/os-release;
#   NEOVIBE_INSTALL_TEST_PROC             a directory read instead of /proc (only its
#                                         sys/kernel/apparmor_restrict_unprivileged_userns);
#   NEOVIBE_INSTALL_TEST_APPARMOR_D       a directory standing in for /etc/apparmor.d.
# The last two default, in test mode, to directories that do not exist, so a test that does not set
# them never reads this host's own restriction or profiles.
# None of them touches the trust anchor: the release key is never taken from the environment (only
# --release-signers replaces it, with a warning). At worst they make a system check pass or keep
# one more sidecar.
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
OPT_YES=0
OPT_ALLOW_VERDANDI_REV_MISMATCH=0

LOCK_HELD=0
NV_LOCK=
NV_NEW_CREATED=0
NV_CACHE_CREATED=0
NV_LOOPBACK=0
NV_SYS_REV7=
NV_SIDECAR_WORKDIR=
NV_SIDECAR_TMP=
NV_NVIM_TMP_DEST=
NV_VERDANDI_SOURCE_TARBALL_OVERRIDE=
NV_SIDECAR_ACTUAL_VERDANDI_REV=

say() { printf 'neovibe: %s\n' "$*"; }
warn() { printf 'neovibe: warning: %s\n' "$*" >&2; }
die() {
	printf 'neovibe: error: %s\n' "$*" >&2
	exit 1
}

usage() {
	cat <<'EOF'
neovibe installer -- installs neovibe for the user running it, into ~/.local.

usage: sh install.sh [options]

  (no option)                  install or upgrade to the latest release
  --version X                  a specific release, e.g. 1.0.0 (also the only way to install an
                               older version than the one installed)
  --yes, --no-nvim, --with-nvim
                               non-interactive answers to the prompt offering to install a private
                               copy of nvim; --yes is the generic affirmative, --no-nvim/--with-nvim
                               answer it by name
  --nvim-only                  the nvim offer alone, for an already-installed neovibe (reads RELEASE
                               beside this script, or --release-file FILE); always fetches unless
                               that exact nvim is already installed privately, and a download
                               failure is fatal
  --nvim-offer                 the same nvim offer a normal install runs, for an already-installed
                               neovibe -- skipped when the nvim already on PATH is new enough, when
                               the release's nvim is already installed privately, or with --no-nvim;
                               a download failure only warns
  --from-source [--checkout DIR] [--verdandi-checkout DIR [--allow-verdandi-rev-mismatch]]
                               build neovibe from source, then install exactly like a prebuilt
                               release. Without --checkout: clone the public repo at the chosen
                               version's tag and refuse unless its HEAD matches the verified
                               release's NEOVIBE_COMMIT. With --checkout DIR: build DIR as it is,
                               synthesizing a RELEASE from its own version, HEAD, agent/Cargo.toml's
                               Verdandi rev and packaging/pins.env (a local development build).
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
  --allow-root                 run as root anyway (containers)
  --uninstall [--purge]        remove neovibe; --purge also removes ~/.config/neovibe and its state
  --dry-run                    print every action without doing it
  --keep-build                 keep the sidecar build directory
  --sidecar-only [--release-file FILE]
                               build the sidecar alone, for an already-installed neovibe; reads
                               RELEASE from beside this script, or FILE. `neovibe setup` with no
                               mode flag runs this, then --nvim-offer above
  --build-sidecar-into DIR --node TARBALL --verdandi-source TARBALL [--release-file FILE]
                               build the sidecar from files already on disk and install it into DIR
                               instead of the per-user path (the neovibe-bin AUR package's build())
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
			--checkout | --verdandi-checkout | --node | --verdandi-source | --build-sidecar-into)
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
			esac
			shift
			;;
		-h | --help) OPT_HELP=1 ;;
		# The nvim offer's own non-interactive "yes" (spec §6.2): --with-nvim/--no-nvim answer the
		# offer by name, --yes is the generic affirmative this installer's only prompt also honours.
		--yes) OPT_YES=1 ;;
		--keep-build) OPT_KEEP_BUILD=1 ;;
		--no-nvim) OPT_NVIM=no ;;
		--with-nvim) OPT_NVIM=yes ;;
		--uninstall) set_mode uninstall ;;
		--sidecar-only) set_mode sidecar-only ;;
		--nvim-only) set_mode nvim-only ;;
		--nvim-offer) set_mode nvim-offer ;;
		--from-source) set_mode from-source ;;
		--allow-verdandi-rev-mismatch) OPT_ALLOW_VERDANDI_REV_MISMATCH=1 ;;
		--purge) OPT_PURGE=1 ;;
		--allow-root) OPT_ALLOW_ROOT=1 ;;
		--dry-run) OPT_DRY_RUN=1 ;;
		*) die "unknown option: $1 (see --help)" ;;
		esac
		shift
	done
	if [ "$OPT_PURGE" = 1 ] && [ "$OPT_MODE" != uninstall ]; then
		die "--purge only goes with --uninstall"
	fi
	if [ -n "$OPT_VERSION" ] && ! printf '%s\n' "$OPT_VERSION" | grep -Eqx "$NV_VERSION_ERE"; then
		die "--version $OPT_VERSION: expected a release version such as 1.0.0 or 1.0.0-rc.1"
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

# xdg_dir data|cache|state -- spec §6.4. Sets XD_DIR. An unset, empty or relative XDG_DATA_HOME/
# XDG_CACHE_HOME/XDG_STATE_HOME means $HOME/.local/share, $HOME/.cache or $HOME/.local/state: the rule
# the Rust side uses (core/src/layout/persist.rs::state_subdir, behind state_dir, and plan Task 3's
# user_sidecar_path), so `neovibe setup` and `neovibe` never disagree about where the sidecar is. A
# plain ${VAR:-default} would pass a relative value through. Defaults are spelled with $HOME, never
# `~`, which is not expanded inside quotes and would create a directory literally named `~`.
# A value holding a newline is refused, as check_home refuses one in HOME: every path built from it
# would hold one too, and one once split the uninstall's target list so that
# XDG_DATA_HOME="$HOME/Documents<newline>..." removed ~/Documents (Task 9 review). It is never
# replaced by the default instead, which the Rust side (taking an absolute value as given) would
# not agree with. A variable, not output, and a plain statement: rule 2 above.
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

set_paths() {
	NV_LIBROOT=$NV_HOME/.local/lib
	NV_LIB=$NV_LIBROOT/neovibe
	NV_BINDIR=$NV_HOME/.local/bin
	xdg_dir data
	NV_DATA=$XD_DIR
	xdg_dir cache
	NV_CACHE=$XD_DIR
	xdg_dir state
	NV_STATE=$XD_DIR
	NV_CACHE_NV=$NV_CACHE/neovibe
	NV_DL=$NV_CACHE_NV/download
	NV_STAGE=$NV_CACHE_NV/unpack
	NV_SIDECAR_ROOT=$NV_DATA/neovibe/sidecar
	NV_SYSTEM_RELEASE=/usr/lib/neovibe/RELEASE
	# What rev_of_release tells the user to do about a system RELEASE it cannot use: that file is a
	# package's, which this installer never touches.
	NV_SYSTEM_REMEDY='reinstall or remove the neovibe package that owns it (this installer never changes it)'
	NV_OS_RELEASE=/etc/os-release
	NV_TEST_LIBDIRS=
	if [ "${NEOVIBE_INSTALL_TEST-}" = 1 ]; then
		NV_TEST_MODE=1
		NV_SYSTEM_RELEASE=${NEOVIBE_INSTALL_TEST_SYSTEM_RELEASE-$NV_SYSTEM_RELEASE}
		NV_OS_RELEASE=${NEOVIBE_INSTALL_TEST_OS_RELEASE-$NV_OS_RELEASE}
		NV_TEST_LIBDIRS=${NEOVIBE_INSTALL_TEST_LIBDIRS-}
		NV_PROC=${NEOVIBE_INSTALL_TEST_PROC-/nonexistent/neovibe-test-proc}
		NV_APPARMOR_D=${NEOVIBE_INSTALL_TEST_APPARMOR_D-/nonexistent/neovibe-test-apparmor.d}
	fi
}

# check_data_home: spec §6.4 -- installer-claude-9. An absolute XDG_DATA_HOME outside $HOME makes
# install write the desktop entry, licences, private nvim and sidecar somewhere --uninstall can
# never remove: under_home (below) refuses every target outside $HOME as a whole, so the in-$HOME
# parts (NV_LIB, the cache) were left uninstallable too, with only a by-hand remedy. Refused here
# instead, before anything is written, for every mode that writes under NV_DATA (do_install,
# do_from_source, do_sidecar_only, do_nvim_only). --uninstall itself is deliberately exempt: an
# install made before this check existed must still be removable by its own existing refusal (which
# names exactly what to delete by hand).
check_data_home() {
	_cdh_ok=$(under_home "$NV_DATA")
	if [ "$_cdh_ok" != 1 ]; then
		die "XDG_DATA_HOME resolves to $NV_DATA, which is not inside $NV_HOME: neovibe writes its desktop entry, licences, private nvim and sidecar there, and --uninstall can only ever remove paths inside \$HOME. Set XDG_DATA_HOME to a directory under \$HOME (or unset it) and re-run"
	fi
}

# ---------------------------------------------------------------------------------------------
# Small pure helpers. Each prints its answer; none changes anything.

# kv_get FILE KEY: the value of the first KEY=value line. KEY=value files (RELEASE, BUILD,
# os-release) are read by a case loop, never sourced or eval'd (spec §4.3).
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

# rev_of_release FILE REMEDY: sets RR_REV7 to the 7-hex prefix of the VERDANDI_REV a RELEASE
# names, or to nothing when there is no such file. A file that exists but cannot be read, or names
# no bare lowercase-hex VERDANDI_REV (missing, empty, quoted, anything else), stops the run, and
# REMEDY is the message's "what to do": the safe direction, since which sidecar its install uses
# cannot be told, and a sidecar is never pruned or uninstalled on a guess (spec §6.5 step 4, §6.6).
# release.sh and the packages write bare values, so only a damaged file gets here; reading one as
# "names no sidecar" once let --uninstall remove that install's sidecar (Task 9 review). It sets a
# variable and runs as a plain statement, because kv_get can die (rule 2 above): run inside $(...),
# `bash install.sh` lost that die and removed the sidecar (Task 9 review).
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
# Reading a RELEASE (spec §4.3)

# parse_release FILE: sets REL_VERSION, REL_VERDANDI_REV, REL_REV7 and the sidecar-build fields
# below; dies naming the file when any of them is missing or malformed.
parse_release() {
	_pl_text=$(cat -- "$1") || die "cannot open $1"
	parse_release_text "$_pl_text" "$1"
}

# parse_release_text TEXT NAME: parse_release, for a RELEASE already read; NAME is what messages
# call it. Also sets the fields plan Task 10's sidecar build reads (spec §4.3, §5.3): REL_VERDANDI_SOURCE
# (checked against REL_REV7's own prefix, since a mismatch there means the release was assembled
# wrong), REL_VERDANDI_SOURCE_SHA256, REL_NODE_VERSION, REL_NODE_SHA256_LINUX_X64 and
# REL_NODE_SHA256_LINUX_ARM64 -- copied from packaging/pins.env into every real RELEASE (release.sh,
# plan Task 12) -- and plan Task 11's own fields: REL_NEOVIBE_COMMIT, REL_NEOVIDE_FORK_COMMIT (spec
# §6.2's own clone check reads the first), REL_NVIM_VERSION and REL_NVIM_SHA256_LINUX_X86_64 (spec
# §7's offer). A RELEASE missing any of them is as damaged as one missing VERDANDI_REV.
parse_release_text() {
	REL_VERSION=$(kv_value "$1" NEOVIBE_VERSION)
	REL_NEOVIBE_COMMIT=$(kv_value "$1" NEOVIBE_COMMIT)
	REL_NEOVIDE_FORK_COMMIT=$(kv_value "$1" NEOVIDE_FORK_COMMIT)
	REL_VERDANDI_REV=$(kv_value "$1" VERDANDI_REV)
	if ! printf '%s\n' "$REL_VERSION" | grep -Eqx "$NV_VERSION_ERE"; then
		die "$2 names no valid NEOVIBE_VERSION: the release is damaged. Report it at $NV_ISSUES"
	fi
	if ! printf '%s\n' "$REL_NEOVIBE_COMMIT" | grep -Eqx '[0-9a-f]{40}'; then
		die "$2 names no valid NEOVIBE_COMMIT (40 hex): the release is damaged. Report it at $NV_ISSUES"
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
	# installer-claude-7: optional, and absent from every real (downloaded) RELEASE -- release.sh
	# never writes it. Only synthesize_release_from_checkout sets it, to mark a RELEASE whose own
	# VERDANDI_SOURCE_SHA256 is a `git archive` hash rather than the real release asset's, so a
	# later, separate `neovibe setup` run (which has no override tarball of its own) can tell why it
	# cannot fetch that asset instead of trying and reporting a checksum mismatch as tampering.
	REL_BUILT_FROM_CHECKOUT=$(kv_value "$1" NEOVIBE_BUILT_FROM_CHECKOUT)
}

# ---------------------------------------------------------------------------------------------
# System checks (spec §6.3). Each dies with the command to run, never runs it.

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

# runtime_hint: how to get GTK 4 and WebKitGTK 6.0 on this distribution (spec §6.3's table).
runtime_hint() {
	case $OS_FAMILY in
	debian) printf '%s\n' "install them with: sudo apt install libgtk-4-1 libwebkitgtk-6.0-4" ;;
	fedora) printf '%s\n' "install them with: sudo dnf install gtk4 webkitgtk6.0 (on RHEL 10, from EPEL)" ;;
	arch) printf '%s\n' "install them with: sudo pacman -S gtk4 webkitgtk-6.0 (or use the AUR package neovibe-bin)" ;;
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
	die "do not run this as root: run it as the user who will run neovibe (the sidecar is built per user into \$XDG_DATA_HOME, and as root npm would run every dependency's install script as root). In a container, pass --allow-root."
}

check_platform() {
	_cp=$(uname -sm) || die "uname failed"
	case $_cp in
	'Linux x86_64') ;;
	# F4 (v1-dist whole-branch review, 2026-09-28): this used to send a non-x86_64 user to
	# --from-source, which always failed later at the Skia fetch (the only pinned archive is
	# x86_64) -- v1 builds x86_64 only, full stop, and do_from_source now refuses the same
	# architectures up front for the same reason, so this no longer points anywhere that works.
	*) die "prebuilt neovibe releases are for x86_64 Linux only, and this is $_cp. v1 builds x86_64 only: there is no working install path for another architecture yet" ;;
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
		*) die "neovibe's prebuilt binaries need glibc $NV_GLIBC_FLOOR or newer, and this system does not appear to use glibc: build from source instead (sh install.sh --from-source)" ;;
		esac
		;;
	esac
	_cg_c=$(dotted_cmp "$_cg" "$NV_GLIBC_FLOOR")
	if [ "$_cg_c" = -1 ]; then
		die "neovibe's prebuilt binaries need glibc $NV_GLIBC_FLOOR or newer (they are built on Ubuntu 24.04), and this system has $_cg. Releases that have it: Ubuntu 24.04, Debian 13, Fedora 40, RHEL 10, Arch. Upgrade, or build from source (sh install.sh --from-source)."
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

# lib_dirs_for SONAME: sets LD_DIRS to the directories that hold SONAME, one per line --
# `ldconfig -p`'s own architecture entries first (what the dynamic linker uses), then the usual lib
# directories for that architecture. A variable, not output, since ldconfig_output can die (rule 2
# above). installer-codex-4: this used to hardcode the x86-64 marker `ldconfig -p` prints and the
# Debian/Ubuntu x86_64 lib directory, so it found nothing at all on aarch64 (ldconfig prints
# "AArch64" there, not "x86-64"). F4 (v1-dist whole-branch review, 2026-09-28) made this aarch64
# branch unreachable altogether: check_platform already refused a prebuilt install off x86_64
# before this was ever called, and do_from_source now refuses --from-source off x86_64 too (no
# aarch64 Skia archive is pinned), so nothing in this installer can reach check_gtk_webkit on
# aarch64 any more. Left in rather than deleted: it costs nothing to keep correct, and the day an
# aarch64 Skia archive is pinned this is the branch that already works.
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
		die "GTK 4 (libgtk-4.so.1) was not found; neovibe needs GTK 4.14 or newer and WebKitGTK 6.0: $_gw_hint"
	fi
	_gw_minor=${_gw_ver#4.}
	_gw_minor=${_gw_minor%%.*}
	if [ "$_gw_minor" -lt "$NV_GTK_FLOOR_MINOR" ]; then
		_gw_old=$(gtk_too_old_hint "$_gw_ver")
		die "this system has GTK $_gw_ver, and neovibe needs GTK 4.$NV_GTK_FLOOR_MINOR or newer: its binaries are built against 4.$NV_GTK_FLOOR_MINOR's API on Ubuntu 24.04. $_gw_old."
	fi
	lib_dirs_for libwebkitgtk-6.0.so.4
	_gw_wk=$LD_DIRS
	if [ -z "$_gw_wk" ]; then
		die "WebKitGTK 6.0 (libwebkitgtk-6.0.so.4) was not found; neovibe's agent panel needs it: $_gw_hint"
	fi
	say "GTK $_gw_ver and WebKitGTK 6.0: ok"
}

# nvim_check: spec §6.3 -- reports which nvim is on PATH and why, and sets NV_NVIM_OK=1 when it
# already satisfies NV_NVIM_FLOOR (0.10, the fork's own floor). maybe_offer_nvim reads NV_NVIM_OK to
# decide whether spec §7's offer runs at all -- an adequate PATH nvim always wins, whatever flag was
# given, so --with-nvim never *replaces* a perfectly good system nvim.
nvim_check() {
	NV_NVIM_OK=0
	if ! command -v nvim >/dev/null 2>&1; then
		warn "nvim was not found on PATH. neovibe needs nvim $NV_NVIM_FLOOR or newer: install it from your distribution or https://github.com/neovim/neovim/releases"
		return 0
	fi
	_rn=$(nvim --version 2>/dev/null | head -n 1) || _rn=
	_rn_v=${_rn#NVIM v}
	_rn_v=${_rn_v%%[!0-9.]*}
	if [ -z "$_rn_v" ]; then
		warn "could not parse nvim's version ($_rn); neovibe needs nvim $NV_NVIM_FLOOR or newer"
		return 0
	fi
	_rn_c=$(dotted_cmp "$_rn_v" "$NV_NVIM_FLOOR")
	if [ "$_rn_c" = -1 ]; then
		warn "nvim $_rn_v is older than $NV_NVIM_FLOOR, which neovibe needs (its editor refuses older ones): install a newer nvim from https://github.com/neovim/neovim/releases"
	else
		say "nvim $_rn_v: ok"
		NV_NVIM_OK=1
	fi
}

# nvim_dist_base: where the pinned nvim release tarball is fetched from -- neovim's own GitHub
# releases, never neovibe's release server (spec §7). Fixed in production; test mode redirects it to
# a fixture server, and -- deliberately, the same discipline as node_dist_base -- an unset override
# does NOT fall back to the real github.com: a test that forgets to set it must fail fast against a
# refused loopback port, never spend real time downloading a real nvim release.
nvim_dist_base() {
	if [ "$NV_TEST_MODE" = 1 ]; then
		printf '%s\n' "${NEOVIBE_INSTALL_TEST_NVIM_BASE_URL:-http://127.0.0.1:1}"
	else
		printf '%s\n' "$NV_NVIM_DIST_BASE"
	fi
}

# nvim_private_state VERSION: sets NV_NVIM_PRIVATE_PRESENT=1 when
# $XDG_DATA_HOME/neovibe/nvim/VERSION/bin/nvim already exists and is executable (the layout contract
# spec §7 shares with plan Task 6's resolver) -- --nvim-only and a re-run of the offer are both
# idempotent against an already-fetched version.
nvim_private_state() {
	NV_NVIM_PRIVATE_PRESENT=0
	NV_NVIM_PRIVATE_BIN=$NV_DATA/neovibe/nvim/$1/bin/nvim
	if [ -n "$1" ] && [ -x "$NV_NVIM_PRIVATE_BIN" ]; then NV_NVIM_PRIVATE_PRESENT=1; fi
}

# install_nvim_version VERSION SHA256 TOLERANT: spec §7 -- download the official
# nvim-linux-x86_64.tar.gz of VERSION, verify it against SHA256 (which the caller always reads off a
# checked RELEASE, never re-derives here), and extract it with --strip-components=1 (the tarball's
# own top directory is nvim-linux-x86_64/) into $XDG_DATA_HOME/neovibe/nvim/VERSION/. Never placed on
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
	curl_get "$(nvim_dist_base)/v$_inv_version/nvim-linux-x86_64.tar.gz" "$_inv_tar.part"
	if [ "$CG_OK" != 1 ]; then
		if [ "$_inv_tolerant" = 1 ]; then
			warn "could not download nvim $_inv_version ($CG_ERR): neovibe is installed without a private copy; run \"neovibe setup --nvim-only\" once you have network access"
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
	tar -xzf "$_inv_tar" --strip-components=1 -C "$_inv_extract" || die "could not unpack nvim-linux-x86_64.tar.gz: report it at $NV_ISSUES"
	if [ ! -x "$_inv_extract/bin/nvim" ]; then
		die "nvim-linux-x86_64.tar.gz did not extract to bin/nvim: report it at $NV_ISSUES"
	fi
	mkdir -p -- "$NV_DATA/neovibe/nvim" || die "cannot create $NV_DATA/neovibe/nvim"
	_inv_dest=$NV_DATA/neovibe/nvim/$_inv_version
	_inv_tmp=$_inv_dest.tmp.$$
	NV_NVIM_TMP_DEST=$_inv_tmp
	rm -rf -- "$_inv_tmp" || die "cannot remove the stale $_inv_tmp"
	mv -- "$_inv_extract" "$_inv_tmp" || die "cannot move the extracted nvim into place"
	rm -rf -- "$_inv_dest" || die "cannot remove the stale $_inv_dest"
	mv -- "$_inv_tmp" "$_inv_dest" || die "cannot move the extracted nvim into $_inv_dest"
	NV_NVIM_TMP_DEST=
	rm -rf -- "$_inv_workdir" || :
	say "installed nvim $_inv_version into $_inv_dest (never on PATH; neovibe finds it on its own)"
}

# maybe_offer_nvim: spec §7's actual offer, run once the new release's RELEASE is known (after
# unpack_new / from_source_stage) -- unlike nvim_check's early report, this needs REL_NVIM_VERSION
# and REL_NVIM_SHA256_LINUX_X86_64, which only exist once a RELEASE has been parsed. Skipped whole
# when the PATH nvim is already adequate (NV_NVIM_OK) or --no-nvim was given; --with-nvim or --yes
# answer it without asking; otherwise a tty is tried (rule: "stdin is the script", so this never
# tests stdin -- only whether /dev/tty can be opened), and with none, it is reported only (spec
# §6.2's own words for this exact case).
maybe_offer_nvim() {
	if [ "$NV_NVIM_OK" = 1 ]; then return 0; fi
	if [ "$OPT_NVIM" = no ]; then return 0; fi
	# installer-codex-5: the pinned nvim is x86_64 only (install_nvim_version always fetches
	# nvim-linux-x86_64.tar.gz); do_nvim_only's own explicit ask guards with check_platform, but
	# this best-effort offer did not, so --with-nvim or --yes on aarch64 installed the wrong
	# architecture's binary and nvim_bin.rs then preferred it over a perfectly good system nvim. F4
	# (v1-dist whole-branch review, 2026-09-28) made the --from-source route here unreachable off
	# x86_64 too (do_from_source now refuses non-x86_64 up front, before this is ever called), so
	# --nvim-offer (a real caller: neovibe's own post-install "run neovibe setup" path) is the only
	# way left to reach this function on another architecture -- this check still guards it.
	_mon_uname=$(uname -m) || _mon_uname=
	if [ "$_mon_uname" != x86_64 ]; then
		say "no prebuilt nvim for $_mon_uname (only x86_64 is offered): install nvim $NV_NVIM_FLOOR or newer yourself"
		return 0
	fi
	# Review-2 fix round 2: the pinned version already installed privately (an upgrade whose new
	# RELEASE pins the same nvim, or a second `neovibe setup`) is nothing to offer -- neovibe's own
	# resolver (core/src/nvim_bin.rs) already prefers it over the inadequate PATH nvim that got us
	# here. Without this, --with-nvim/--yes re-downloaded and replaced the very same version and a
	# tty was asked again, the idempotence do_nvim_only has always had. A dry run that could not read
	# the new RELEASE (NV_NEW_REV7 empty, see below) has no version to look for.
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
	# A real `( ... )` subshell, never a `{ ...; }` brace group: with no controlling terminal at
	# all (this container, a bare `curl | sh`), dash treats the open failure on /dev/tty as fatal
	# to the CURRENT shell -- exit 2, no message, `set -e`'s if/elif exemption does not save it --
	# and takes the whole script down with it, `2>/dev/null` and all (reproduced: dash 0.5.12,
	# under `docker run … neovibe-install-dash`). A subshell dying the same way only ends the
	# subshell; its exit status is all `elif` ever sees.
	elif ( : </dev/tty ) 2>/dev/null; then
		printf 'neovibe: fetch the pinned nvim release into XDG_DATA_HOME/neovibe/nvim/ (never put on PATH)? [y/N] ' >&2
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
		say "would install nvim $REL_NVIM_VERSION into $NV_DATA/neovibe/nvim/$REL_NVIM_VERSION"
		return 0
	fi
	install_nvim_version "$REL_NVIM_VERSION" "$REL_NVIM_SHA256_LINUX_X86_64" 1
}

# report_claude: the claude CLI is never installed here, only reported, and compared with the range
# the present sidecar says it supports.
report_claude() {
	if ! command -v claude >/dev/null 2>&1; then
		warn "the claude CLI was not found on PATH; neovibe's agent panel runs it. Install it with Anthropic's installer: curl -fsSL https://claude.ai/install.sh | bash"
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
# Downloads (spec §6.2 --base-url, §6.4)

# check_base_url URL: sets NV_BASE and NV_LOOPBACK. https only; plain http only for exactly
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
	https://?*) NV_LOOPBACK=0 ;;
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
		NV_LOOPBACK=1
		;;
	*) die "--base-url $1: the URL must start with https://" ;;
	esac
	NV_BASE=$_bu
}

# require_curl: every download goes through curl. Spec §6.2 also names `wget --https-only` for a
# system without curl, but wget's manual scopes --https-only to recursive downloads, and measured
# (GNU Wget 1.25.0, Task 9 review) it follows an https -> http redirect -- and fetches a plain
# http:// URL -- with exit 0. curl's --proto-redir holds every hop to HTTPS, and GitHub serves every
# release asset through a redirect, so wget is not used at all (neovibe-only deviation from §6.2).
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

# fetch URL DEST WHAT: download URL to DEST ('-' = stdout). HTTPS-only for every non-loopback URL,
# on every redirect too (--proto/--proto-redir); a loopback base follows no redirect at all.
fetch() {
	if [ "$NV_LOOPBACK" = 1 ]; then
		curl --proto '=http' -fsS -o "$2" -- "$1" || die "could not download $3 from $1: is the server at $NV_BASE running?"
	else
		curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL -o "$2" -- "$1" || die "could not download $3 from $1: check your network connection and re-run"
	fi
}

# sums_hash NAME: the hash of the one SHA256SUMS line whose second field is exactly NAME (spec
# §6.4). Not `sha256sum -c` over the whole list: that fails on the files not downloaded.
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

# tarball_version: the version named by the one tarball line of SHA256SUMS (spec §6.4). The
# version comes from here, not from any API call.
tarball_version() {
	_tv=$(printf '%s\n' "$NV_SUMS_TEXT" | grep -E "^[0-9a-f]{64}  neovibe-$NV_VERSION_ERE-x86_64-linux\\.tar\\.gz\$") || _tv=
	case $_tv in
	'') die "SHA256SUMS lists no neovibe-<version>-x86_64-linux.tar.gz: this is not a neovibe release (check --base-url or --version)" ;;
	*"$NV_NL"*) die "SHA256SUMS lists more than one neovibe tarball, so which version it is cannot be told: refusing. Report it at $NV_ISSUES" ;;
	esac
	_tv=${_tv#*  neovibe-}
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

# download_verified NAME: fetch NAME from the release into NAME.part, check it, and only then
# rename it. Nothing is unpacked before this has passed.
download_verified() {
	fetch "$NV_REL_URL/$1" "$NV_DL/$1.part" "$1"
	verify_file "$NV_DL/$1.part" "$1"
	mv -- "$NV_DL/$1.part" "$NV_DL/$1" || die "cannot rename $NV_DL/$1.part"
}

# curl_get URL DEST: like fetch, but reports failure through CG_OK/CG_ERR instead of dying (rule 2:
# a predicate reports through a variable), because the sidecar build's own first download (Node) is
# the one place in this installer where "the network is not reachable" is not fatal (spec §5.2: a
# release with no sidecar still installs; the panel says how to get one). HTTPS is held across every
# redirect, exactly like fetch; a bare http:// URL (only ever a loopback test override) follows none.
curl_get() {
	CG_OK=0
	CG_ERR=
	case $1 in
	https://*)
		if _cg_out=$(curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL -o "$2" -- "$1" 2>&1); then
			CG_OK=1
		else
			CG_ERR=$_cg_out
		fi
		;;
	http://*)
		if _cg_out=$(curl --proto '=http' -fsS -o "$2" -- "$1" 2>&1); then
			CG_OK=1
		else
			CG_ERR=$_cg_out
		fi
		;;
	*) die "curl_get: $1 is not an http(s) URL" ;;
	esac
}

# node_arch: sets NV_NODE_ARCH from uname -m, matching how packaging/pins.env and buildBinary.mjs
# name the two Node platforms neovibe pins (x64/arm64, not uname's own x86_64/aarch64).
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
# (NV_NODE_DIST_BASE); in test mode, NEOVIBE_INSTALL_TEST_NODE_BASE_URL redirects it to a fixture
# server, and -- deliberately -- an unset override does NOT fall back to the real nodejs.org: this
# host (like the harness's own) can reach the real internet, and a test suite that forgets to set
# the override must fail fast against a refused loopback port, never spend real time downloading a
# real Node tarball from every test that reaches this point (plan Task 10 review).
node_dist_base() {
	if [ "$NV_TEST_MODE" = 1 ]; then
		printf '%s\n' "${NEOVIBE_INSTALL_TEST_NODE_BASE_URL:-http://127.0.0.1:1}"
	else
		printf '%s\n' "$NV_NODE_DIST_BASE"
	fi
}

# check_sidecar_build_space DIR: spec §5.3 -- about 600 MiB free before starting (npm ci's
# node_modules measured at 356 MB, its own cache at 123 MB, the built artifact at 110 MB, all in
# DIR's filesystem). A df that cannot be read degrades rather than blocking a build over it.
check_sidecar_build_space() {
	_csb_kib=$(df -Pk "$1" 2>/dev/null | awk 'NR==2 { print $4 }') || _csb_kib=
	case $_csb_kib in
	'' | *[!0-9]*) return 0 ;;
	esac
	if [ "$_csb_kib" -lt "$NV_SIDECAR_BUILD_MIN_KIB" ]; then
		die "only $((_csb_kib / 1024)) MiB free in $1's filesystem; building the sidecar needs about 600 MiB (npm ci's node_modules measured at 356 MB, its own cache at 123 MB, the built artifact at 110 MB): free some space and re-run"
	fi
}

# ---------------------------------------------------------------------------------------------
# The release key (spec §6.4, D5, D13)

# embedded_release_signers: packaging/release-signers, verbatim, between the two marker lines
# (the heredoc's own). packaging/tests/install/ asserts the block is byte-equal to that file.
embedded_release_signers() {
	cat <<'NEOVIBE_RELEASE_SIGNERS'
# neovibe release signers (spec 2026-09-27-v1-dist-design.md §6.4, decisions D5 and D13).
#
# ssh-keygen's allowed_signers format. packaging/install.sh embeds this file byte for byte (a test
# holds the two equal) and checks a release's SHA256SUMS.sig with exactly:
#
#   ssh-keygen -Y verify -f <this file> -I release@neovibe -n neovibe-release -s SHA256SUMS.sig < SHA256SUMS
#
# A key line looks like this, with no comment field after the key:
#
#   release@neovibe namespaces="neovibe-release" ssh-ed25519 AAAA...
#
# There is no key line yet. The owner adds the dedicated release key's public half here before the
# first final release (spec §4.4); it must not be an everyday login key, and release.sh refuses one
# that matches any ~/.ssh/id_*.pub. Once a key is listed, every installer built from this tree
# refuses a release whose SHA256SUMS.sig is missing or does not verify (D13). Until then an
# installer built from this tree says it carries no release key, and its checksums only detect
# corruption.
NEOVIBE_RELEASE_SIGNERS
}

# prepare_signers: sets NV_SIGNERS (a file) and NV_SIGNERS_HAVE_KEY. A signers file that cannot
# be read stops the run rather than reading as "no key": grep's error (exit 2) once did, and an
# unreadable --release-signers then switched signature checking off -- even in an installer with a
# key built in, even with a --sig given (Task 9 review). A --release-signers that lists no key is
# refused too: it would switch checking off the same way, and supplying a key is the option's only
# use (neovibe-only: spec §6.2 does not say).
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
		die "--release-signers $OPT_SIGNERS lists no key, so it would switch signature checking off: give a file holding the release key's line (release@neovibe namespaces=\"neovibe-release\" ssh-ed25519 ...), or leave the option out"
	fi
}

# verify_signature SUMS SIG: D13 -- with a key listed, a missing or unfetchable SIG refuses and a
# bad one refuses; only a missing ssh-keygen degrades, with a message. The command is exactly the
# spec's; ssh-keygen's no-validate check mode is never used (it accepts a signature by any key).
verify_signature() {
	if [ "$NV_SIGNERS_HAVE_KEY" != 1 ]; then
		warn "this installer carries no release key: checksums only detect corruption, they do not show who made the release"
		return 0
	fi
	if [ ! -f "$2" ]; then
		die "this release has no SHA256SUMS.sig, but this installer carries a release key and so requires one: refusing, because an unsigned release could have been altered. Report it at $NV_ISSUES"
	fi
	if ! command -v ssh-keygen >/dev/null 2>&1; then
		warn "ssh-keygen was not found, so the release signature cannot be checked: checksums only detect corruption. Install OpenSSH's client (openssh-client / openssh) to have it checked"
		return 0
	fi
	if [ "$OPT_DRY_RUN" = 1 ] && [ -z "$OPT_SIGNERS" ]; then
		# prepare_signers writes the embedded key to $NV_SIGNERS only in a real run, and a dry run
		# writes nothing: ssh-keygen would find no file and report a good signature as a bad one
		# (Task 9 review). Said as the network dry run says it.
		say "would check $2 with ssh-keygen -Y verify against the release key built into this installer (not checked in a dry run)"
		return 0
	fi
	if ! _vs_out=$(ssh-keygen -Y verify -f "$NV_SIGNERS" -I "$NV_SIGNER_IDENTITY" -n "$NV_SIGNATURE_NAMESPACE" -s "$2" <"$1" 2>&1); then
		die "the signature on SHA256SUMS does not verify against the release key ($_vs_out): refusing to install. Do not use this download; report it at $NV_ISSUES"
	fi
	say "signature on SHA256SUMS: good ($NV_SIGNER_IDENTITY)"
}

# ---------------------------------------------------------------------------------------------
# Choosing and fetching the release

# obtain_release: sets NV_VERSION, NV_TARBALL_NAME and NV_SUMS_TEXT, from the network (spec §6.4) or
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
			# The files are copied into this run's own download directory (made empty and 0700 by
			# recover_interrupted), and only the copies are checked and used. Read where they lay,
			# the signature was checked on one read of --sums and the hashes taken from another, so
			# a SHA256SUMS swapped in between -- by anyone able to rename files in that directory,
			# or a network mount serving each open differently -- passed the signature and
			# installed what it listed (Task 9 review). do_install copies the tarball the same way.
			_or_sums=$NV_DL/SHA256SUMS
			_or_sig=$NV_DL/SHA256SUMS.sig
			cp -- "$OPT_SUMS" "$_or_sums" || die "cannot copy $OPT_SUMS into $NV_DL: check that it is readable and the disk is not full, then re-run"
			if [ -n "$OPT_SIG" ]; then
				cp -- "$OPT_SIG" "$_or_sig" || die "cannot copy $OPT_SIG into $NV_DL: check that it is readable and the disk is not full, then re-run"
			fi
		fi
		verify_signature "$_or_sums" "$_or_sig"
		NV_SUMS_TEXT=$(cat -- "$_or_sums") || die "cannot open $_or_sums"
		NV_VERSION=$(tarball_version)
		if [ -n "$OPT_VERSION" ] && [ "$OPT_VERSION" != "$NV_VERSION" ]; then
			die "--version $OPT_VERSION, but $OPT_SUMS is for $NV_VERSION"
		fi
		NV_TARBALL_NAME=neovibe-$NV_VERSION-x86_64-linux.tar.gz
		if [ "${OPT_TARBALL##*/}" != "$NV_TARBALL_NAME" ]; then
			die "--tarball must be the file named $NV_TARBALL_NAME that SHA256SUMS lists (it is ${OPT_TARBALL##*/})"
		fi
		# docs-claude-2: sidecar_download_verdandi_source reads NV_REL_URL under `set -u`, and
		# nothing on this branch used to set it -- a sidecar that was not already present crashed
		# with "NV_REL_URL: unbound variable" the moment a build was attempted, on a machine that
		# had already downloaded Node successfully. --base-url is refused together with --tarball
		# (parse_args), so this is always the default GitHub host. It is only a fallback: do_install
		# looks for verdandi-<rev7>-source.tar.gz beside --tarball first, once the tarball's own
		# RELEASE is known, and a genuinely offline --tarball install that carries that file beside
		# it never reaches this URL at all.
		NV_REL_URL=$NV_DEFAULT_BASE_URL/releases/download/v$NV_VERSION
		return 0
	fi

	if [ -n "$OPT_VERSION" ]; then
		_or_dir=releases/download/v$OPT_VERSION
	else
		_or_dir=releases/latest/download
	fi
	if [ "$OPT_DRY_RUN" = 1 ]; then
		NV_SUMS_TEXT=$(fetch "$NV_BASE/$_or_dir/SHA256SUMS" - SHA256SUMS)
	else
		fetch "$NV_BASE/$_or_dir/SHA256SUMS" "$NV_DL/SHA256SUMS.part" SHA256SUMS
		NV_SUMS_TEXT=$(cat -- "$NV_DL/SHA256SUMS.part") || die "cannot open $NV_DL/SHA256SUMS.part"
	fi
	NV_VERSION=$(tarball_version)
	if [ -n "$OPT_VERSION" ] && [ "$OPT_VERSION" != "$NV_VERSION" ]; then
		die "asked for $OPT_VERSION, but the release's SHA256SUMS is for $NV_VERSION: check --version and --base-url"
	fi
	NV_TARBALL_NAME=neovibe-$NV_VERSION-x86_64-linux.tar.gz
	# From here on every file comes from releases/download/v<X>/, so a release published between
	# two requests cannot mix files.
	NV_REL_URL=$NV_BASE/releases/download/v$NV_VERSION
	if [ "$OPT_DRY_RUN" = 1 ]; then
		if [ -z "$OPT_VERSION" ]; then
			NV_SUMS_TEXT=$(fetch "$NV_REL_URL/SHA256SUMS" - SHA256SUMS)
			_or_v=$(tarball_version)
			if [ "$_or_v" != "$NV_VERSION" ]; then die "the release changed while it was being fetched: re-run"; fi
		fi
		if [ "$NV_SIGNERS_HAVE_KEY" = 1 ]; then
			say "would download $NV_REL_URL/SHA256SUMS.sig and check it with ssh-keygen -Y verify (not checked in a dry run)"
		else
			verify_signature - -
		fi
		return 0
	fi
	if [ -z "$OPT_VERSION" ]; then
		rm -f -- "$NV_DL/SHA256SUMS.part" || die "cannot remove $NV_DL/SHA256SUMS.part"
		fetch "$NV_REL_URL/SHA256SUMS" "$NV_DL/SHA256SUMS.part" SHA256SUMS
		NV_SUMS_TEXT=$(cat -- "$NV_DL/SHA256SUMS.part") || die "cannot open $NV_DL/SHA256SUMS.part"
		_or_v=$(tarball_version)
		if [ "$_or_v" != "$NV_VERSION" ]; then die "the release changed while it was being fetched (latest was $NV_VERSION, v$NV_VERSION's SHA256SUMS names $_or_v): re-run"; fi
	fi
	mv -- "$NV_DL/SHA256SUMS.part" "$NV_DL/SHA256SUMS" || die "cannot rename $NV_DL/SHA256SUMS.part"
	if [ "$NV_SIGNERS_HAVE_KEY" = 1 ]; then
		fetch "$NV_REL_URL/SHA256SUMS.sig" "$NV_DL/SHA256SUMS.sig" "SHA256SUMS.sig (this installer carries a release key, so a release without a signature is refused)"
	fi
	verify_signature "$NV_DL/SHA256SUMS" "$NV_DL/SHA256SUMS.sig"
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
		printf 'neovibe: would run:'
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
		printf 'neovibe: would run (in %s):' "$_ri_dir"
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

# acquire_lock: a mkdir lock in <cache>/neovibe/ (spec §6.5 step 1), so two runs never share
# neovibe.new, the download directory or a sidecar build. A lock left by a dead pid is taken over.
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
	[ -n "$(find -H "$1" -maxdepth 0 -gid "$_opg_gid" -print 2>/dev/null)" ] || return 0
	# An extended ACL can give a named user or group write access that the mode bits do not show (the
	# group bits then report the ACL mask), so a directory with one never gets the allowance. GNU ls
	# marks it with a '+' after the mode; getfacl, where it is installed, lists the entries themselves.
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
	_opg_gcount=$(getent group 2>/dev/null | awk -F: -v g="$_opg_gid" '$3 == g { n++ } END { print n + 0 }') || return 0
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

acquire_lock() {
	NV_LOCK=$NV_CACHE_NV/lock
	if [ "$OPT_DRY_RUN" = 1 ]; then
		say "would take the lock $NV_LOCK"
		return 0
	fi
	# installer-claude-8 (+installer-codex-1): a symlinked <cache>/neovibe is refused outright,
	# before `mkdir -p` (which is a silent no-op on one already pointing at an existing directory,
	# so it would otherwise never even reach a permission check). Another user, in a shared sticky
	# directory such as /tmp, can plant one and repoint it after any check that only looked at the
	# permissions of whatever it resolved to at the time.
	if [ -L "$NV_CACHE_NV" ]; then
		die "$NV_CACHE_NV is a symlink: a download checked there could be replaced by repointing it, and this installer refuses to use it. Remove it, or set XDG_CACHE_HOME to a directory of your own, and re-run"
	fi
	# A cache directory this run creates is removed again with its contents (on_exit), so a run
	# that installs nothing leaves nothing.
	if [ ! -d "$NV_CACHE" ]; then NV_CACHE_CREATED=1; fi
	# 0700 when this run creates it; the cache directory above keeps the user's umask.
	# shellcheck disable=SC2174 # -m for the deepest directory only, deliberately
	mkdir -p -m 0700 -- "$NV_CACHE_NV" || die "cannot create $NV_CACHE_NV: check that $NV_CACHE is writable"
	_al_uid=$(id -u) || die "id -u failed"
	# M2 (v1-dist whole-branch review, 2026-09-28): `mkdir -p` is a silent no-op on a directory that
	# already exists, so a $NV_CACHE_NV planted in advance by another user (in a shared
	# XDG_CACHE_HOME) was never checked for who owns it -- only for its own write bits, which say
	# nothing about whether its owner can already read or replace what a download writes inside it.
	# Skipped for uid 0: --allow-root already documents a broader trust reduction (every
	# state-changing step, npm's own install scripts included, runs as root), and ownership alone
	# is not the boundary root's own unrestricted read/write access is checked against.
	if [ "$_al_uid" != 0 ]; then
		_al_nv_owner_bad=$(find -H "$NV_CACHE_NV" -maxdepth 0 ! -uid "$_al_uid" -print 2>/dev/null) || _al_nv_owner_bad=
		if [ -n "$_al_nv_owner_bad" ]; then
			die "$NV_CACHE_NV is owned by another user, so a download checked there could already be under their control: remove it (or set XDG_CACHE_HOME to a directory of your own) and re-run"
		fi
	fi
	# Downloads are checked and then used in here: in a directory other users can write to (one made
	# in advance under a shared XDG_CACHE_HOME such as /tmp), they could swap the download directory
	# for their own between the two (Task 9 review). Both the other- and group-write bits are
	# checked now (installer-claude-8: `-perm -0002` alone missed a group-writable 0770 directory).
	_al_priv=$(find -H "$NV_CACHE_NV" -prune ! -perm -0002 ! -perm -0020 -print 2>/dev/null) || _al_priv=
	if [ -z "$_al_priv" ]; then
		# Group-write alone, on this user's own private group (own_private_group), lets no one else in.
		_al_upg=$(find -H "$NV_CACHE_NV" -prune ! -perm -0002 -perm -0020 -print 2>/dev/null) || _al_upg=
		OPG=0
		if [ -n "$_al_upg" ]; then own_private_group "$NV_CACHE_NV"; fi
		if [ "$OPG" = 1 ]; then _al_priv=$_al_upg; fi
	fi
	if [ -z "$_al_priv" ]; then
		die "other users can write to $NV_CACHE_NV, so a download checked there could be replaced before it is used: run chmod go-w '$NV_CACHE_NV' (or set XDG_CACHE_HOME to a directory of your own) and re-run"
	fi
	# A parent (<cache> itself) that anyone can write to, without the sticky bit, lets another user
	# rename this directory away and put their own "neovibe" in its place between any two checks --
	# the two checks above only ever looked at NV_CACHE_NV's own permissions (installer-claude-8).
	# M2 (v1-dist whole-branch review, 2026-09-28): that check alone still missed three cases --
	# a group-writable (0770) non-sticky <cache> (only ever checked -perm -0002, other-write); a
	# sticky 1777 <cache> owned by ANOTHER user, whose owner can rename our directory away regardless
	# of the sticky bit (sticky only stops non-owners, never the directory's own owner); and this
	# case, folded into the NV_CACHE_NV check above rather than repeated here, of $NV_CACHE_NV itself
	# being owned by someone else. Ownership and write-bits are now both checked, independently: even
	# a root-owned, perfectly private-looking <cache> owned by someone else is refused, and even a
	# <cache> we own but left group/other-writable without the sticky bit is refused.
	if [ -d "$NV_CACHE" ]; then
		# Skipped for uid 0, the same reason as $NV_CACHE_NV's own ownership check above.
		if [ "$_al_uid" != 0 ]; then
			_al_parent_owner_bad=$(find -H "$NV_CACHE" -maxdepth 0 ! -uid "$_al_uid" ! -uid 0 -print 2>/dev/null) || _al_parent_owner_bad=
			if [ -n "$_al_parent_owner_bad" ]; then
				die "$NV_CACHE is owned by another user (not you, and not root), so its owner could replace $NV_CACHE_NV between checks no matter its permissions: set XDG_CACHE_HOME to a directory of your own and re-run"
			fi
		fi
		_al_parent_bad=$(find -H "$NV_CACHE" -maxdepth 0 \( -perm -0002 -o -perm -0020 \) ! -perm -1000 -print 2>/dev/null) || _al_parent_bad=
		if [ -n "$_al_parent_bad" ]; then
			# rc.2's e2e (2026-09-28): refusing every group-writable <cache> refused a default Ubuntu
			# user's own 0775 ~/.cache, so `neovibe setup` failed after a plain .deb install. Group-write
			# alone, on this user's own private group, lets no one else in (own_private_group).
			_al_parent_upg=$(find -H "$NV_CACHE" -maxdepth 0 ! -perm -0002 -perm -0020 -print 2>/dev/null) || _al_parent_upg=
			OPG=0
			if [ -n "$_al_parent_upg" ]; then own_private_group "$NV_CACHE"; fi
			if [ "$OPG" = 1 ]; then _al_parent_bad=; fi
		fi
		if [ -n "$_al_parent_bad" ]; then
			# M2 follow-up (v1-dist whole-branch review, fix round 2, 2026-09-28): this check applies
			# regardless of who owns $NV_CACHE (the ownership check above already requires it be us
			# or root), and "chmod +t" was the only remedy offered even when $NV_CACHE is our own --
			# a user-private-group system's umask 002 routinely leaves a self-owned ~/.cache at 0775,
			# where the natural fix is removing the group-write bit we do not need, not adding a
			# sticky bit meant for a directory shared with others.
			die "$NV_CACHE is writable by its group or by anyone, and not sticky, so another user could replace $NV_CACHE_NV between checks: if you own $NV_CACHE and do not need to share write access to it, run chmod g-w '$NV_CACHE'; if it is meant to be shared, run chmod +t '$NV_CACHE' instead (or set XDG_CACHE_HOME to a directory of your own) and re-run"
		fi
	fi
	if ! mkdir -- "$NV_LOCK" 2>/dev/null; then
		# M4 (v1-dist whole-branch review, 2026-09-28): the stale-lock takeover below is now
		# serialized by its own mkdir-based sub-lock, so at most one run at a time ever reaches it.
		# Before this, two runs that both saw the same dead pid could both reach it at once: A takes
		# over and mkdirs a fresh, still-pid-less NV_LOCK; B -- mid-takeover itself, nothing yet
		# stopping it -- renames THAT fresh lock aside (thinking it is still the original stale
		# one), finds no pid in it, and puts it back; meanwhile C's own plain `mkdir NV_LOCK`
		# succeeded in the gap B's own rename had opened up, so B's "put back" (GNU mv's own rule of
		# moving INTO an existing directory rather than replacing it) lands inside C's live lock
		# directory instead of restoring anything -- and A and C now both believe they alone hold
		# the lock (Task 9 review; installer-codex-3).
		_al_takeover=$NV_LOCK.takeover
		if ! mkdir -- "$_al_takeover" 2>/dev/null; then
			_al_tk_pid=$(cat -- "$_al_takeover/pid" 2>/dev/null) || _al_tk_pid=
			if [ -z "$_al_tk_pid" ]; then
				# M4 follow-up (v1-dist whole-branch review, fix round 2, 2026-09-28): the holder may
				# have just mkdir'd $_al_takeover and not yet written its pid -- the same allowance
				# NV_LOCK's own pid read gets below, extended here so a run that loses this exact
				# race is not misdiagnosed as permanently stuck.
				sleep 1
				_al_tk_pid=$(cat -- "$_al_takeover/pid" 2>/dev/null) || _al_tk_pid=
			fi
			if [ -z "$_al_tk_pid" ]; then
				# M4 follow-up: this used to fall through to the generic "already recovering"
				# message below, with no way to tell a permanently stuck sub-lock (its own holder
				# died between mkdir and the pid write, so no future run will ever see a pid here
				# either) from ordinary contention, and never named $_al_takeover as the thing a
				# human would have to remove.
				die "$_al_takeover holds no pid, so whether another neovibe installer is using or recovering the lock $NV_LOCK cannot be told: if none is running, remove $_al_takeover and re-run"
			fi
			_al_tk_dead=0
			_al_tk_live=$(pid_alive "$_al_tk_pid")
			if [ "$_al_tk_live" != 1 ]; then _al_tk_dead=1; fi
			if [ "$_al_tk_dead" != 1 ]; then
				# M4 follow-up: reworded -- this fires whenever two runs contend for $_al_takeover
				# while NV_LOCK merely exists, whether NV_LOCK itself later turns out to be live (the
				# ordinary case: two installs racing) or stale. Calling NV_LOCK "the stale lock" here
				# presupposed an answer this run has not actually checked yet.
				die "another neovibe installer (pid $_al_tk_pid) is already checking the lock $NV_LOCK: wait a moment and re-run"
			fi
			if ! rm -rf -- "$_al_takeover" 2>/dev/null || ! mkdir -- "$_al_takeover" 2>/dev/null; then
				die "another neovibe installer is already checking the lock $NV_LOCK: wait a moment and re-run"
			fi
		fi
		printf '%s\n' "$$" >"$_al_takeover/pid" 2>/dev/null || :
		# M4 follow-up (v1-dist whole-branch review, fix round 2, 2026-09-28): the recovery above
		# still has a narrow window of its own -- two runs that both found the same abandoned
		# $_al_takeover dead can both pass the checks above and both rm -rf + mkdir it in turn
		# (Task 9's own rejected pattern for NV_LOCK, reapplied one level down, since a sub-lock has
		# nothing lower to serialize its own recovery with). Reading the pid back does not make that
		# mkdir atomic across the two runs, but only the write that lands last is what is on disk
		# afterwards: whichever run does NOT see its own pid here bails below, instead of proceeding
		# to touch NV_LOCK under a false belief that it alone holds $_al_takeover. This does not
		# close the race outright -- a read landing before the other run's later write would still
		# miss it -- it shrinks the window from the whole block above down to the gap between this
		# write and this read, turning a rare double-hold into a rare, safe, spurious failure.
		_al_tk_owner=$(cat -- "$_al_takeover/pid" 2>/dev/null) || _al_tk_owner=
		if [ "$_al_tk_owner" != "$$" ]; then
			# Worded like the other collisions on $_al_takeover above, not "recovering the stale
			# lock": NV_LOCK's own pid has not been read yet at this point (that happens below), so
			# whether it is actually stale is not yet known here either.
			die "another neovibe installer is also checking the lock $NV_LOCK right now: wait a moment and re-run"
		fi
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
			die "the lock $NV_LOCK holds no pid, so whether another neovibe installer is using it cannot be told: if none is running, remove $NV_LOCK and re-run"
		fi
		_al_live=$(pid_alive "$_al_pid")
		if [ "$_al_live" = 1 ]; then
			rm -rf -- "$_al_takeover" || :
			die "another neovibe installer (pid $_al_pid) is running: wait for it to finish and re-run. If none is running, remove $NV_LOCK and re-run"
		fi
		warn "taking over a lock left by an installer that is no longer running (pid $_al_pid)"
		# Removed directly, never renamed aside first: nothing else can be modifying NV_LOCK while
		# this run holds the takeover sub-lock (every other path that would touch a stale-looking
		# NV_LOCK needs that same sub-lock first), so there is nothing left to protect a rename
		# against.
		rm -rf -- "$NV_LOCK" || die "cannot remove the stale lock $NV_LOCK: remove it by hand and re-run"
		if ! mkdir -- "$NV_LOCK" 2>/dev/null; then
			# The one race this cannot close: an entirely separate, fresh run's own very first
			# `mkdir NV_LOCK` (which needs no sub-lock at all, since to it NV_LOCK simply did not
			# exist) landing in the instant between the rm -rf above and this mkdir. Reported the
			# same way an ordinary lock collision always is, never treated as corruption.
			rm -rf -- "$_al_takeover" || :
			die "another neovibe installer took the lock $NV_LOCK just now: wait for it to finish and re-run"
		fi
		rm -rf -- "$_al_takeover" || :
	fi
	# Held only once the pid is in it, so a run that cannot write one removes only its own, still
	# empty, lock directory. The write can fail after the pid file itself was created (installer-
	# codex-7: e.g. the filesystem filled up between open() and write()) -- rm it first, or rmdir
	# fails on a non-empty directory and this run's own lock is stuck stale for the next one.
	if ! printf '%s\n' "$$" >"$NV_LOCK/pid"; then
		rm -f -- "$NV_LOCK/pid" 2>/dev/null || :
		rmdir -- "$NV_LOCK" 2>/dev/null || :
		die "cannot write $NV_LOCK/pid: check that $NV_CACHE_NV is writable and re-run"
	fi
	LOCK_HELD=1
}

# on_exit: runs on every exit once the lock is held. A failed run puts back whatever it had moved
# (the same recovery a later run would do) and never leaves its downloads or lock behind.
on_exit() {
	_oe_rc=$?
	if [ "$LOCK_HELD" = 1 ]; then
		if [ "$_oe_rc" != 0 ]; then
			if [ "$NV_NEW_CREATED" = 1 ]; then rm -rf -- "$NV_LIB.new" 2>/dev/null || :; fi
			if [ ! -e "$NV_LIB" ] && [ -d "$NV_LIB.old" ]; then mv -- "$NV_LIB.old" "$NV_LIB" 2>/dev/null || :; fi
			# unpack_new's own `mkdir -p -- "$NV_LIBROOT"` can be the only thing that ever touched a
			# fresh $HOME before a later step (now including a fatal sidecar-build failure, plan Task
			# 10 review) died: rmdir only succeeds while it is still empty, so an install (or
			# neovibe.old) genuinely there is never touched.
			rmdir -- "$NV_LIBROOT" 2>/dev/null || :
		fi
		# The temporary names a write goes through before its `mv` (none is left on success).
		for _oe_t in "$NV_BINDIR/.neovibe.tmp.$$" "$NV_DATA/applications/.neovibe.desktop.tmp.$$" \
			"$NV_DATA/licenses/neovibe/.LICENSE.tmp.$$" "$NV_DATA/licenses/neovibe/.THIRD-PARTY-LICENSES.tmp.$$" \
			"$NV_DATA/licenses/neovibe/.SOURCE.tmp.$$" "$NV_DATA/licenses/neovibe/.installed-version.tmp.$$"; do
			rm -f -- "$_oe_t" 2>/dev/null || :
		done
		if [ -n "${NV_SIDECAR_TMP-}" ]; then rm -f -- "$NV_SIDECAR_TMP" 2>/dev/null || :; fi
		if [ -n "${NV_NVIM_TMP_DEST-}" ]; then rm -rf -- "$NV_NVIM_TMP_DEST" 2>/dev/null || :; fi
		rm -rf -- "$NV_DL" "$NV_STAGE" 2>/dev/null || :
		# install_nvim_version's own workdir (always transient, spec §7: never kept, unlike the
		# sidecar's) and --from-source's own scratch (the cloned neovibe source, the Verdandi source
		# it archived or cloned, the downloaded Node and Skia archives): kept with --keep-build, the
		# same knob the sidecar build honours. installer-claude-6 (+installer-codex-8): from-source-
		# node (about 200 MB) and from-source-skia used to be removed only by an ad-hoc `rm -rf` at
		# the very end of a successful do_from_source, so any earlier death (including inside
		# finish_install) left them behind for good; every exit path is covered here now, and the
		# synthesized RELEASE a --checkout run writes (installer-claude-6's own "written, never
		# deleted" finding) is this same run's own, named by its own pid.
		if [ "$OPT_KEEP_BUILD" != 1 ]; then
			rm -rf -- "$NV_CACHE_NV/nvim-build" "$NV_CACHE_NV/src" "$NV_CACHE_NV/verdandi-src" \
				"$NV_CACHE_NV/verdandi-src-build" "$NV_CACHE_NV/from-source-node" \
				"$NV_CACHE_NV/from-source-skia" 2>/dev/null || :
			rm -f -- "$NV_CACHE_NV/from-source-RELEASE.$$" 2>/dev/null || :
		fi
		# The sidecar build's own work directory (spec §5.3 step 7: removed after success, kept with
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

# recover_interrupted: spec §6.5 step 1. A stale neovibe.new goes; a missing neovibe with a
# neovibe.old comes back; with both present, neovibe.old goes -- otherwise the swap's
# `mv neovibe neovibe.old` would move the tree inside it.
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

# sidecar_state REV7: NV_SIDECAR_PRESENT=1 when that rev's sidecar is *present* in spec §5.3 step
# 6's sense: the binary and BUILD both exist, and the binary's first --version line equals BUILD's
# SIDECAR_VERSION_LINE.
sidecar_state() {
	NV_SIDECAR_PRESENT=0
	NV_SIDECAR_BIN=$NV_SIDECAR_ROOT/$1/verdandi-claude-sidecar
	_ss_build=$NV_SIDECAR_ROOT/$1/BUILD
	if [ -z "$1" ] || [ ! -x "$NV_SIDECAR_BIN" ] || [ ! -f "$_ss_build" ]; then return 0; fi
	_ss_want=$(kv_get "$_ss_build" SIDECAR_VERSION_LINE)
	_ss_have=$("$NV_SIDECAR_BIN" --version 2>/dev/null | head -n 1) || _ss_have=
	if [ -n "$_ss_want" ] && [ "$_ss_have" = "$_ss_want" ]; then NV_SIDECAR_PRESENT=1; fi
}

# ---------------------------------------------------------------------------------------------
# Building the sidecar (spec §5.3), run before the swap so a failed build leaves the old install
# intact (spec §6.5 step 2). Every state-changing step below dies on failure EXCEPT the very first
# network call (Node's own download), and even that is tolerant only on a FIRST install (nothing
# working is being replaced): "the network to nodejs.org is not reachable" is the one failure
# ensure_sidecar's own best-effort call tolerates then (a warning, the install continues), because a
# release with no sidecar is still strictly better than no release at all, and §5.2's own runtime
# failure ("no sidecar for verdandi <rev7> -- run neovibe setup") already tells the user how to
# finish the job later. On an UPGRADE that is replacing a working sidecar, the same Node failure is
# fatal (TOLERANT=0): spec §6.5 step 2 promises "a failed build leaves the old install intact", and
# swapping in a new install with no sidecar when the old one had one is not that -- it silently turns
# a working agent panel into "no sidecar" (F1, v1-dist whole-branch review, 2026-09-28). Once the Node
# download has succeeded -- Node is real, on disk, checksum-verified -- every later failure (a bad
# Verdandi-source checksum, a failed npm ci, a build that does not produce an acceptable artifact) is
# fatal regardless of upgrade-vs-first-install, because at that point a real attempt is in flight and
# a silent partial one is worse than stopping (plan Task 10 review, SH-2: a failing npm inside an
# upgrade must give a non-zero exit and no swap, not a warning).
# --sidecar-only and --build-sidecar-into have no such tolerance anywhere: building the sidecar is
# their whole job, so even the first download failing is fatal there (install_sidecar_for_rev's own
# TOLERANT parameter, sidecar_download_node's).

# sidecar_download_node WORKDIR TOLERANT: spec §5.3 step 2. Sets SDN_TARBALL and SDN_OK (rule 2:
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
	curl_get "$_sdn_url" "$_sdn_out.part"
	if [ "$CG_OK" != 1 ]; then
		if [ "$2" = 1 ]; then
			warn "could not download the pinned Node runtime ($_sdn_url), needed to build the sidecar: $CG_ERR. neovibe is installed without one; run \"neovibe setup\" once you have network access to nodejs.org"
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

# sidecar_download_verdandi_source WORKDIR: spec §5.3 step 4, from the release's own asset URL
# (NV_REL_URL, set by whichever caller resolved a release). Reached only once Node is already down
# (see above), so this is always fatal on failure -- never tolerant.
sidecar_download_verdandi_source() {
	# --from-source (plan Task 11, spec §6.2) has no release page to fetch this from: resolve_verdandi_source
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
	# installer-claude-7: a RELEASE marked NEOVIBE_BUILT_FROM_CHECKOUT reached here with no override
	# -- a separate, later run (`neovibe setup` beside a --checkout install's own RELEASE) rather
	# than the same one that built it, so resolve_verdandi_source never ran this time. Its
	# VERDANDI_SOURCE_SHA256 is a `git archive` hash, which will never equal the real release
	# asset's bytes: downloading and reporting that as a checksum mismatch would read as tampering,
	# when the real reason is simply that this RELEASE cannot be resolved this way any more.
	if [ "$REL_BUILT_FROM_CHECKOUT" = 1 ]; then
		die "$NV_RELEASE_PATH was built from a --checkout tree (NEOVIBE_BUILT_FROM_CHECKOUT=1): its VERDANDI_SOURCE_SHA256 is a git-archive hash, not the real release asset's, so this cannot be fetched from $NV_REL_URL. Re-run sh install.sh --from-source --checkout DIR (the same tree, or one at the same commit) to rebuild it instead"
	fi
	_sdv_url=$NV_REL_URL/$REL_VERDANDI_SOURCE
	_sdv_out=$1/$REL_VERDANDI_SOURCE
	curl_get "$_sdv_url" "$_sdv_out.part"
	if [ "$CG_OK" != 1 ]; then die "could not download the Verdandi source ($_sdv_url): $CG_ERR"; fi
	_sdv_have=$(file_sha256 "$_sdv_out.part")
	if [ "$_sdv_have" != "$REL_VERDANDI_SOURCE_SHA256" ]; then
		rm -f -- "$_sdv_out.part" || :
		die "checksum mismatch for $REL_VERDANDI_SOURCE: expected $REL_VERDANDI_SOURCE_SHA256, got $_sdv_have. Refusing to build the sidecar from unverified source; re-run, and report it at $NV_ISSUES if it persists"
	fi
	mv -- "$_sdv_out.part" "$_sdv_out" || die "cannot rename $_sdv_out.part"
	SDV_TARBALL=$_sdv_out
}

# sidecar_sdk_notice REPO: spec §5.3 step 5, after npm ci and before the build -- the SDK's own
# version and what its LICENSE.md states, both read from the files npm ci just wrote rather than
# hard-coded: the user is downloading Anthropic's non-open-source SDK to their own machine (I2) and
# should be told before the build runs.
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

# accept_sidecar_artifact PATH: spec §5.3 step 6 -- refuses unless the artifact's own --version says
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

# sidecar_build_core WORKDIR NODE_TARBALL VERDANDI_SOURCE_TARBALL: spec §5.3 steps 1 (layout), 3
# (node-cache placement), 5 (npm ci, the SDK notice, npm run build:binary) and 6's artifact pick +
# acceptance. Both callers below hand it already-downloaded-and-verified tarballs, whether fetched
# over the network or given as local files (--build-sidecar-into). Sets SB_ARTIFACT, SB_VERSION_LINE,
# SB_SDK_LICENSE and SB_NODE_LICENSE.
sidecar_build_core() {
	_sbc_workdir=$1
	_sbc_node_tar=$2
	_sbc_src_tar=$3
	_sbc_node_dir=$_sbc_workdir/node
	_sbc_repo=$_sbc_workdir/repo
	mkdir -p -- "$_sbc_node_dir" "$_sbc_repo" || die "cannot create $_sbc_workdir's build layout"
	tar -xf "$_sbc_node_tar" -C "$_sbc_node_dir" || die "could not unpack $_sbc_node_tar: report it at $NV_ISSUES"
	_sbc_node_top=$_sbc_node_dir/node-$REL_NODE_VERSION-linux-$NV_NODE_ARCH
	_sbc_node_bin=$_sbc_node_top/bin
	if [ ! -x "$_sbc_node_bin/node" ] || [ ! -x "$_sbc_node_bin/npm" ]; then
		die "$_sbc_node_tar does not contain bin/node and bin/npm at $_sbc_node_top: not a Node distribution tarball. Report it at $NV_ISSUES"
	fi
	tar -xzf "$_sbc_src_tar" -C "$_sbc_repo" || die "could not unpack $_sbc_src_tar: report it at $NV_ISSUES"
	if [ ! -f "$_sbc_repo/package.json" ] || [ ! -f "$_sbc_repo/apps/claude-sidecar/package.json" ]; then
		die "$_sbc_src_tar does not look like the Verdandi source (no package.json / apps/claude-sidecar/package.json): report it at $NV_ISSUES"
	fi
	# buildBinary.mjs reuses this exact cache and re-verifies it against its own pinned checksum -- a
	# second, independent check (spec §5.3 step 3).
	_sbc_cache_dir=$_sbc_repo/apps/claude-sidecar/build/node-cache
	mkdir -p -- "$_sbc_cache_dir" || die "cannot create $_sbc_cache_dir"
	cp -- "$_sbc_node_tar" "$_sbc_cache_dir/" || die "cannot copy $_sbc_node_tar into $_sbc_cache_dir"

	_sbc_npm_cache=$_sbc_workdir/npm-cache
	run_in "$_sbc_repo" env "PATH=$_sbc_node_bin:$PATH" "npm_config_cache=$_sbc_npm_cache" \
		"$_sbc_node_bin/npm" ci --no-audit --no-fund
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
}

# install_sidecar_for_rev REV7 TOLERANT: the per-user path (spec §5.2), used by both ensure_sidecar
# (TOLERANT=1) and --sidecar-only (TOLERANT=0). NV_SIDECAR_WORKDIR is read by on_exit, which removes
# it unless --keep-build (spec §5.3 step 7).
install_sidecar_for_rev() {
	_isr_rev7=$1
	_isr_tolerant=$2
	_isr_dest=$NV_SIDECAR_ROOT/$_isr_rev7
	_isr_workdir=$NV_CACHE_NV/sidecar-build/$_isr_rev7
	rm -rf -- "$_isr_workdir" || die "cannot remove the stale sidecar build directory $_isr_workdir"
	mkdir -p -- "$_isr_workdir" || die "cannot create $_isr_workdir"
	NV_SIDECAR_WORKDIR=$_isr_workdir
	check_sidecar_build_space "$_isr_workdir"
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
	# BUILD is written last (spec §5.3 step 6): sidecar_state only counts a rev present once both
	# the binary and this file exist and agree. Its own VERDANDI_REV line names the rev actually
	# built when --allow-verdandi-rev-mismatch (plan Task 11) let resolve_verdandi_source diverge
	# from the pin -- the directory is still keyed by REL_REV7 (what agent::EXPECTED_VERDANDI_REVISION
	# looks for), but this file says honestly what is inside it.
	printf 'VERDANDI_REV=%s\nSIDECAR_VERSION_LINE=%s\nBUILT_AT=%s\n' \
		"${NV_SIDECAR_ACTUAL_VERDANDI_REV:-$REL_VERDANDI_REV}" "$SB_VERSION_LINE" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >"$_isr_dest/BUILD" ||
		die "cannot write $_isr_dest/BUILD"
	say "built the sidecar for verdandi $_isr_rev7: $SB_VERSION_LINE"
}

# ensure_sidecar: called from do_install, before the swap (spec §6.5 step 2).
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
		say "would build the sidecar for verdandi $NV_NEW_REV7 into $NV_SIDECAR_ROOT/$NV_NEW_REV7 (needs network access to nodejs.org and the release server; run \"neovibe setup\" later if it cannot)"
		return 0
	fi
	# F1 (v1-dist whole-branch review, 2026-09-28): TOLERANT is 1 only when no WORKING install is
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

# resolve_setup_release: --sidecar-only's and --build-sidecar-into's RELEASE (spec §6.2, §9): from
# beside the running script (dirname "$0" -- a .deb/.rpm/tarball install's own neovibe-setup, or the
# neovibe-bin PKGBUILD's copy of it), or --release-file. Deliberately never a hard-coded
# ~/.local/lib/neovibe or /usr/lib/neovibe: a .deb install's neovibe-setup must never build against a
# tarball install's RELEASE lying around in ~/.local, or the reverse (spec: "it never searches
# ~/.local and then /usr"). Sets REL_* (parse_release).
resolve_setup_release() {
	if [ -n "$OPT_RELEASE_FILE" ]; then
		NV_RELEASE_PATH=$OPT_RELEASE_FILE
	else
		_rsr_dir=$(cd -- "$(dirname -- "$0")" 2>/dev/null && pwd -P) || _rsr_dir=
		NV_RELEASE_PATH=
		if [ -n "$_rsr_dir" ] && [ -f "$_rsr_dir/RELEASE" ]; then NV_RELEASE_PATH=$_rsr_dir/RELEASE; fi
	fi
	if [ -z "$NV_RELEASE_PATH" ]; then
		die "no RELEASE found beside $0, and no --release-file given: pass --release-file PATH, or run this as the neovibe-setup installed beside a RELEASE (a .deb, .rpm or tarball install)"
	fi
	if [ ! -f "$NV_RELEASE_PATH" ]; then die "--release-file $NV_RELEASE_PATH: no such file"; fi
	parse_release "$NV_RELEASE_PATH"
}

# do_sidecar_only: spec §6.2 --sidecar-only / `neovibe setup`. No SHA256SUMS is fetched: every input
# is checked against the resolved RELEASE alone.
do_sidecar_only() {
	resolve_setup_release
	if [ "$OPT_DRY_RUN" = 1 ]; then
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

# do_nvim_only: spec §6.2 --nvim-only -- the nvim offer alone, for an already-installed neovibe.
# Reuses resolve_setup_release (--sidecar-only's own "RELEASE beside this script, or --release-file"
# rule): a .deb/.rpm/tarball install's own neovibe-setup never reads another install's RELEASE lying
# around. Unlike the install path's own best-effort offer, this is the explicit ask: an already
# -adequate PATH nvim does not suppress it (plan Task 11: "works on an existing install"), and a
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
		say "would install nvim $REL_NVIM_VERSION into $NV_DATA/neovibe/nvim/$REL_NVIM_VERSION (from $NV_RELEASE_PATH)"
		return 0
	fi
	require_curl
	acquire_lock
	install_nvim_version "$REL_NVIM_VERSION" "$REL_NVIM_SHA256_LINUX_X86_64" 0
}

# do_nvim_offer: spec §6.1's plain `neovibe setup` ("build the sidecar (and offer nvim)") -- the
# conditional half the launcher runs after do_sidecar_only. installer-claude-1 (+installer-codex-3):
# the launcher used to run --nvim-only here instead, which is do_nvim_only above -- the explicit,
# unconditional ask that ignores an already-adequate PATH nvim, skips the tty/--yes/--with-nvim
# prompt entirely and dies on a download failure. This reruns nvim_check (so an adequate PATH nvim
# suppresses the offer, exactly as it does on a fresh install) and then maybe_offer_nvim itself --
# same tty/--yes/--with-nvim/--no-nvim rules, same tolerance of a network failure -- the identical
# offer finish_install runs, pointed at an already-installed neovibe instead of a fresh unpack.
# require_curl/acquire_lock are skipped entirely when nvim_check or --no-nvim already means nothing
# will be fetched, the same "cheap checks before the lock" order do_nvim_only and do_sidecar_only
# both hold to above; acquire_lock is --dry-run-safe on its own besides.
do_nvim_offer() {
	resolve_setup_release
	# Review-2 fix round 2: checked first, as do_nvim_only does -- with the pinned version already
	# installed privately there is nothing to offer, and nvim_check's "install a newer nvim" warning
	# about an older PATH nvim would be wrong (neovibe uses the private copy instead of that one).
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

# do_build_sidecar_into: spec §9, the AUR entry point -- `neovibe-setup --build-sidecar-into DIR
# --node TARBALL --verdandi-source TARBALL`. Both inputs are already on disk (makepkg's own
# source=/sha256sums already fetched them); this still checks them against the resolved RELEASE
# before using either, the same discipline as every other input this installer trusts.
do_build_sidecar_into() {
	resolve_setup_release
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
	# M1 (v1-dist whole-branch review, 2026-09-28): copied into the work directory first, then
	# hashed and unpacked from the copies alone -- the same discipline --tarball already holds
	# (obtain_release: hashed where it lay and unpacked from a second read is a TOCTOU; hashed and
	# used from one copy is not). Consistent with --tarball, not a new boundary: the AUR `build()`
	# caller's archives and this running script both sit in the same makepkg $srcdir, so anyone able
	# to swap the archives between the hash and the extract can already rewrite the script itself.
	# M1 fix-round-1 regression (v1-dist whole-branch review, fix round 2, 2026-09-28): the copy used
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
# Building from source (plan 2026-09-27-v1-dist, Task 11, spec §6.2)

# build_hint: the per-distro build-dependency command (spec §6.3's "build" column), the twin of
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

# check_build_tools: spec §6.3's "build (--from-source)" row. Refuses with the distro hint (and,
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

# neovibe_repo_url / verdandi_repo_url: the public repositories --from-source clones. Test-only
# overrides (matched by run-in-env.sh's NEOVIBE_INSTALL_TEST_* allowlist pattern already, no new
# entry needed there) stand in for a local bare repo fixture; production always uses the real URL.
neovibe_repo_url() {
	if [ "$NV_TEST_MODE" = 1 ] && [ -n "${NEOVIBE_INSTALL_TEST_REPO_URL-}" ]; then
		printf '%s\n' "$NEOVIBE_INSTALL_TEST_REPO_URL"
	else
		printf '%s\n' "$NV_NEOVIBE_REPO_URL"
	fi
}
verdandi_repo_url() {
	if [ "$NV_TEST_MODE" = 1 ] && [ -n "${NEOVIBE_INSTALL_TEST_VERDANDI_REPO_URL-}" ]; then
		printf '%s\n' "$NEOVIBE_INSTALL_TEST_VERDANDI_REPO_URL"
	else
		printf '%s\n' "$NV_VERDANDI_REPO_URL"
	fi
}

# checkout_version DIR: the version DIR's own tree builds (spec §6.2 --checkout): shell/Cargo.toml's
# own `version = "..."` line, or -- as the real tree has had since plan Task 1 landed
# `version.workspace = true` there -- the root Cargo.toml's [workspace.package] version, whichever
# the tree actually uses. installer-codex-2: the first grep used to match only the plain
# `version = "..."` string form, so `version.workspace = true` (no `=` right after `version`, a `.`
# instead) was never even captured into _cv_line, and the *workspace* dispatch below -- which does
# fire correctly for the inline-table form `version = { workspace = true }`, since that line DOES
# start with `version *=` -- never got the chance to run. Reproduced on this repo's own
# shell/Cargo.toml. A plain statement (rule 2: this can die).
checkout_version() {
	_cv_shell=$1/shell/Cargo.toml
	if [ ! -f "$_cv_shell" ]; then die "$1 has no shell/Cargo.toml: not a neovibe checkout"; fi
	_cv_line=$(grep -E '^version[[:space:]]*=|^version\.workspace[[:space:]]*=' "$_cv_shell" | head -n 1) || _cv_line=
	case $_cv_line in
	*workspace*) _cv_line=$(grep -E '^version[[:space:]]*=' "$1/Cargo.toml" | head -n 1) || _cv_line= ;;
	esac
	CV_VERSION=$(printf '%s\n' "$_cv_line" | sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p')
	if ! printf '%s\n' "$CV_VERSION" | grep -Eqx "$NV_VERSION_ERE"; then
		die "could not determine a version from $1 (shell/Cargo.toml, or [workspace.package] in its root Cargo.toml): not a neovibe checkout, or its version field is not a plain quoted string"
	fi
}

# lockfile_rev DIR PACKAGE: the 40-hex commit DIR/Cargo.lock's `source = "git+...#<sha>"` line names
# for PACKAGE -- the same technique plan Task 1's shell/build.rs uses for its own --version line,
# reproduced here because this runs before any binary of DIR's own tree exists to ask. A plain
# statement (rule 2: this can die).
lockfile_rev() {
	_lr_lock=$1/Cargo.lock
	if [ ! -f "$_lr_lock" ]; then die "$1 has no Cargo.lock: not a neovibe checkout, or it has not been resolved (run cargo generate-lockfile first)"; fi
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
		die "could not determine $2's commit from $_lr_lock: not a neovibe checkout with a git dependency on $2, or Cargo.lock is stale"
	fi
}

# checkout_fork_rev DIR: sets CFR_REV to the Neovide fork commit a --from-source --checkout build of DIR
# links. Two shapes exist (v1-dist Task 12 sub-plan Task 5, the M20 check): the private tree pins the
# fork as a git dependency, whose Cargo.lock `source` line carries the commit (lockfile_rev); the
# public tree (publish/export.sh) makes it the `neovide/` submodule, a path dependency Cargo.lock
# records with no `source` line at all. For the latter, the commit HEAD records for the submodule
# (`git ls-tree HEAD neovide`, a gitlink) is the one, and the checked-out submodule must be at it --
# the build compiles whatever `neovide/` holds, so a different checkout would make RELEASE lie.
# shell/build.rs resolves its own `--version` fork commit in the same order (lockfile, then the
# `neovide/` checkout). A plain statement (rule 2: this can die).
checkout_fork_rev() {
	_cfr_dir=$1
	_cfr_lock=$_cfr_dir/Cargo.lock
	if [ ! -f "$_cfr_lock" ]; then die "$_cfr_dir has no Cargo.lock: not a neovibe checkout, or it has not been resolved (run cargo generate-lockfile first)"; fi
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
	*) die "could not determine neovide's commit in $_cfr_dir: Cargo.lock names no git source for it and HEAD records no neovide submodule -- not a neovibe checkout" ;;
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

# agent_toml_verdandi DIR: sets AT_GIT_URL and AT_REV from DIR/agent/Cargo.toml's own
# claude-runtime-protocol dependency line. Asserts AT_REV is 40 hex (spec §6.2: "a 7-char rev in
# agent/Cargo.toml refuses") -- a synthesized RELEASE has nothing else to check VERDANDI_REV
# against, so a short prefix here is not enough to trust. A plain statement (rule 2: this can die).
agent_toml_verdandi() {
	_atv_file=$1/agent/Cargo.toml
	if [ ! -f "$_atv_file" ]; then die "$1 has no agent/Cargo.toml: not a neovibe checkout"; fi
	_atv_line=$(grep -E '^claude-runtime-protocol[[:space:]]*=' "$_atv_file" | head -n 1) || _atv_line=
	if [ -z "$_atv_line" ]; then
		die "$_atv_file names no claude-runtime-protocol dependency: not a neovibe checkout, or it no longer pins Verdandi the way this installer expects"
	fi
	AT_GIT_URL=$(printf '%s\n' "$_atv_line" | sed -n 's/.*git[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p')
	AT_REV=$(printf '%s\n' "$_atv_line" | sed -n 's/.*rev[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p')
	if [ -z "$AT_GIT_URL" ] || [ -z "$AT_REV" ]; then
		die "could not parse a git URL and rev out of $_atv_file's claude-runtime-protocol dependency: $_atv_line"
	fi
	if ! printf '%s\n' "$AT_REV" | grep -Eqx '[0-9a-f]{40}'; then
		die "$_atv_file pins claude-runtime-protocol at rev \"$AT_REV\", which is not a full 40-hex commit: --from-source --checkout needs the full commit, not a short prefix, to synthesize a trustworthy RELEASE. Pin the full commit in agent/Cargo.toml and re-run"
	fi
}

# resolve_verdandi_source: sets NV_VERDANDI_SOURCE_TARBALL_OVERRIDE and (when it built the archive)
# REL_VERDANDI_SOURCE_SHA256 to what this build is actually building the sidecar from (spec §6.2).
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
		# Archived from DIR without ever building in place (spec §6.2: "never in place, because
		# buildBinary.mjs writes build/, dist/ and dist-bin/ and npm ci replaces node_modules"). git
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

# use_verdandi_source_beside_tarball: docs-claude-2's other half -- do_install's own --tarball path
# (called once REL_VERDANDI_SOURCE is known, i.e. after unpack_new has parsed the tarball's own
# RELEASE). A genuinely offline install carries verdandi-<rev7>-source.tar.gz next to --tarball
# itself (the same file `neovibe setup`/mk_release ships beside a release's other assets, spec
# §4.3); when it is there, sidecar_download_verdandi_source's own existing check (its sha256 must
# equal the trusted RELEASE's REL_VERDANDI_SOURCE_SHA256) is reused unchanged by pointing
# NV_VERDANDI_SOURCE_TARBALL_OVERRIDE at it, and the sidecar build never touches the network for
# this asset at all. Without it, sidecar_download_verdandi_source falls back to NV_REL_URL, which
# obtain_release's --tarball branch now always sets.
#
# Copied into this run's own 0700 $NV_DL before use, the same discipline obtain_release and
# do_install already hold --sums/--sig/--tarball itself to: sidecar_download_verdandi_source hashes
# the override in place, and sidecar_build_core opens it a second time to extract it -- hashed where
# it lay beside --tarball and reread from that same, possibly shared, directory, a swap between the
# two runs unverified source and its npm scripts despite every checksum passing (review-2 finding;
# the Task 9 review closed the identical gap for --tarball/--sums/--sig themselves).
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

# synthesize_release_from_checkout DIR: spec §6.2 --from-source --checkout (the owner's dev loop,
# D11) -- builds a RELEASE this installer trusts the same way it trusts a downloaded one (every
# field validated identically by parse_release_text), from DIR's own tree instead of a checksummed
# release asset. Sets NV_VERSION and NV_RELEASE_PATH; parse_release has been called on the result by
# the time this returns, so every REL_* field (Task 11's new ones included) is set exactly as it
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
		die "agent/Cargo.toml in $_src_dir pins Verdandi from a private source ($AT_GIT_URL), so its source cannot be fetched publicly: pass --verdandi-checkout DIR (a local checkout of it), or set NEOVIBE_VERDANDI_CHECKOUT"
	fi
	_src_pins=$_src_dir/packaging/pins.env
	if [ ! -f "$_src_pins" ]; then die "$_src_dir has no packaging/pins.env: not a neovibe checkout"; fi
	REL_VERDANDI_REV=$AT_REV
	resolve_verdandi_source
	if [ -z "$NV_VERDANDI_SOURCE_TARBALL_OVERRIDE" ]; then
		die "could not resolve where to build the sidecar's source from -- report it at $NV_ISSUES"
	fi
	_src_rev7=$(first7 "$AT_REV")
	# Read out to plain statements before the group below (rule 2): kv_get can die, and a
	# command substitution inside a `{ ...; } >file || die` group has its own errexit suspended
	# under a non-POSIX-mode bash.
	_src_pins_node_version=$(kv_get "$_src_pins" NODE_VERSION)
	_src_pins_node_x64=$(kv_get "$_src_pins" NODE_SHA256_linux_x64)
	_src_pins_node_arm64=$(kv_get "$_src_pins" NODE_SHA256_linux_arm64)
	_src_pins_nvim_version=$(kv_get "$_src_pins" NVIM_VERSION)
	_src_pins_nvim_sha=$(kv_get "$_src_pins" NVIM_SHA256_linux_x86_64)
	mkdir -p -- "$NV_CACHE_NV" || die "cannot create $NV_CACHE_NV"
	_src_release=$NV_CACHE_NV/from-source-RELEASE.$$
	{
		printf 'NEOVIBE_VERSION=%s\n' "$NV_VERSION"
		printf 'NEOVIBE_COMMIT=%s\n' "$_src_head"
		printf 'NEOVIDE_FORK_COMMIT=%s\n' "$_src_fork"
		printf 'VERDANDI_REV=%s\n' "$AT_REV"
		printf 'VERDANDI_SOURCE=verdandi-%s-source.tar.gz\n' "$_src_rev7"
		printf 'VERDANDI_SOURCE_SHA256=%s\n' "$REL_VERDANDI_SOURCE_SHA256"
		printf 'NODE_VERSION=%s\n' "$_src_pins_node_version"
		printf 'NODE_SHA256_linux_x64=%s\n' "$_src_pins_node_x64"
		printf 'NODE_SHA256_linux_arm64=%s\n' "$_src_pins_node_arm64"
		printf 'NVIM_VERSION=%s\n' "$_src_pins_nvim_version"
		printf 'NVIM_SHA256_linux_x86_64=%s\n' "$_src_pins_nvim_sha"
		# installer-claude-7: this RELEASE's own VERDANDI_SOURCE_SHA256 is a `git archive` hash
		# (resolve_verdandi_source, below), not the real release asset's -- a later, separate
		# `neovibe setup` run has no way to reproduce those exact bytes, and must not be told its
		# download "changed" or was "tampered" when it is simply the wrong kind of RELEASE to ask.
		printf 'NEOVIBE_BUILT_FROM_CHECKOUT=1\n'
	} >"$_src_release" || die "cannot write $_src_release"
	NV_RELEASE_PATH=$_src_release
	parse_release "$_src_release"
}

# clone_public_neovibe VERSION: spec §6.2 --from-source (without --checkout), steps 1-2 --
# obtain_release has already fetched and verified SHA256SUMS/.sig and REL_* comes from a checked
# RELEASE asset (fetch_verified_release, below). Clones the public repo with submodules at tag
# v<VERSION> into <cache>/neovibe/src/, and refuses unless HEAD equals REL_NEOVIBE_COMMIT -- the
# release the checksums vouch for and the source tree being built must be the same commit.
clone_public_neovibe() {
	_cpn_url=$(neovibe_repo_url)
	_cpn_dir=$NV_CACHE_NV/src
	rm -rf -- "$_cpn_dir" || die "cannot remove the stale $_cpn_dir"
	git clone --quiet --branch "v$1" --recurse-submodules "$_cpn_url" "$_cpn_dir" ||
		die "could not clone $_cpn_url at v$1: check your network connection and re-run"
	_cpn_head=$(git -C "$_cpn_dir" rev-parse HEAD 2>/dev/null) || die "cannot determine HEAD in the cloned $_cpn_dir"
	if [ "$_cpn_head" != "$REL_NEOVIBE_COMMIT" ]; then
		die "the clone of $_cpn_url at v$1 is at $_cpn_head, not $REL_NEOVIBE_COMMIT (this release's own RELEASE): refusing to build from a tag that does not match the release it is signed and checksummed with. Report it at $NV_ISSUES"
	fi
	NV_FROM_SOURCE_DIR=$_cpn_dir
}

# fetch_verified_release: spec §6.2 --from-source (without --checkout) -- obtain_release has already
# set NV_SUMS_TEXT/NV_VERSION/NV_REL_URL; this fetches and checks the RELEASE asset itself the same
# way dry_run_new_rev's network branch does for a dry run, then parses it for real.
fetch_verified_release() {
	_fvr_want=$(sums_hash RELEASE)
	fetch "$NV_REL_URL/RELEASE" "$NV_DL/RELEASE.part" RELEASE
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

# from_source_stage SRC RELEASE_PATH: SRC is the tree that was just built (target/release/ holds the
# four binaries); RELEASE_PATH is the RELEASE this build matches. Builds the same on-disk shape
# unpack_new produces from a downloaded tarball (NV_STAGE_TOP, NV_LIB.new), so ensure_sidecar,
# maybe_offer_nvim, stage_files, swap_in, commit_files and prune_sidecars -- finish_install's own
# body -- are the one path both a prebuilt install and a from-source one share from here on. There
# is no THIRD-PARTY-LICENSES/SOURCE generation here (spec §4.2 steps 6-7 belong to release.sh, plan
# Task 12, and need the Skia pin that task adds): a from-source build says plainly what it is rather
# than inventing licence attribution it cannot yet produce correctly.
from_source_stage() {
	_fss_src=$1
	_fss_release=$2
	rm -rf -- "$NV_STAGE" || die "cannot remove the stale $NV_STAGE"
	_fss_top=$NV_STAGE/neovibe-$NV_VERSION-x86_64-linux
	NV_STAGE_TOP=$_fss_top
	mkdir -p -- "$_fss_top/bin" "$_fss_top/lib/neovibe" "$_fss_top/share/applications" "$_fss_top/share/licenses/neovibe" ||
		die "cannot create $_fss_top"
	cp -- "$_fss_src/packaging/neovibe.launcher.sh" "$_fss_top/bin/neovibe" || die "cannot stage the launcher"
	chmod 0755 "$_fss_top/bin/neovibe" || die "cannot chmod the staged launcher"
	for _fss_b in $NV_BINARIES; do
		if [ ! -x "$_fss_src/target/release/$_fss_b" ]; then
			die "the build did not produce target/release/$_fss_b: report it at $NV_ISSUES"
		fi
		cp -- "$_fss_src/target/release/$_fss_b" "$_fss_top/lib/neovibe/$_fss_b" || die "cannot stage $_fss_b"
		chmod 0755 "$_fss_top/lib/neovibe/$_fss_b" || die "cannot chmod $_fss_b"
	done
	cp -- "$_fss_src/packaging/install.sh" "$_fss_top/lib/neovibe/neovibe-setup" || die "cannot stage neovibe-setup"
	chmod 0755 "$_fss_top/lib/neovibe/neovibe-setup" || die "cannot chmod neovibe-setup"
	cp -- "$_fss_release" "$_fss_top/lib/neovibe/RELEASE" || die "cannot stage RELEASE"
	cp -- "$_fss_src/packaging/neovibe.desktop" "$_fss_top/share/applications/neovibe.desktop" || die "cannot stage the desktop entry"
	if [ -f "$_fss_src/LICENSE" ]; then
		cp -- "$_fss_src/LICENSE" "$_fss_top/share/licenses/neovibe/LICENSE" || die "cannot stage LICENSE"
	else
		printf 'No LICENSE file was found in %s.\n' "$_fss_src" >"$_fss_top/share/licenses/neovibe/LICENSE" ||
			die "cannot write a placeholder LICENSE"
	fi
	{
		printf 'This copy of neovibe was built directly from source (sh install.sh --from-source),\n'
		printf 'not from a signed release build, so this file is a placeholder rather than the\n'
		printf 'generated third-party licence text a release build carries.\n\n'
		printf 'source tree:          %s\n' "$_fss_src"
		printf 'commit:               %s\n' "$REL_NEOVIBE_COMMIT"
		printf 'neovide fork commit:  %s\n\n' "$REL_NEOVIDE_FORK_COMMIT"
		printf 'To generate the real text, run packaging/collect-licenses.py in that tree once its\n'
		printf 'own build has produced target/release and agent-ui/web/node_modules -- see that\n'
		printf 'script'"'"'s own --help.\n'
	} >"$_fss_top/share/licenses/neovibe/THIRD-PARTY-LICENSES" || die "cannot write THIRD-PARTY-LICENSES"
	cp -- "$_fss_top/share/licenses/neovibe/THIRD-PARTY-LICENSES" "$_fss_top/share/licenses/neovibe/SOURCE" ||
		die "cannot write SOURCE"
	parse_release "$_fss_top/lib/neovibe/RELEASE"
	if [ "$REL_VERSION" != "$NV_VERSION" ]; then
		die "the RELEASE for this build says $REL_VERSION, not $NV_VERSION: refusing. Report it at $NV_ISSUES"
	fi
	NV_NEW_REV7=$REL_REV7
	mkdir -p -- "$NV_LIBROOT" || die "cannot create $NV_LIBROOT"
	NV_NEW_CREATED=1
	cp -R -- "$_fss_top/lib/neovibe" "$NV_LIB.new" || die "cannot copy the new install to $NV_LIB.new (is the disk full?)"
}

# finish_install: shared by do_install and do_from_source from the point each has produced NV_LIB.new
# (spec §6.5 step 2 on). Factored out so the two never drift on what happens after the binaries
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
	say "installed neovibe $NV_VERSION into $NV_LIB; run it with: neovibe [project directory]"
	if [ -n "$NV_INSTALLED_VERSION" ]; then
		say "restart open neovibe windows: they keep running, but new tabs and the handoff need the new install"
	fi
	# Last, so the one-time sudo steps are the last thing on screen.
	apparmor_note
}

# ---------------------------------------------------------------------------------------------
# The AppArmor user-namespace restriction (docs/superpowers/plans/2026-09-28-v1-dist-ubuntu-userns.md)
#
# Ubuntu 23.10+ sets kernel.apparmor_restrict_unprivileged_userns=1: a program may create a user
# namespace only while an AppArmor profile granting `userns` confines it, and WebKitGTK's bwrap
# sandbox needs one. Without it neovibe shows the fix in the agent panel's place instead of the
# panel (shell/src/webkit_sandbox.rs). This installer never runs sudo, so it writes the profile for
# this install under its own data directory and prints the two commands that install and load it.
#
# The profile is packaging/apparmor/neovibe with this install's own resolved path, always quoted and
# with AppArmor's glob characters escaped, and a name of its own -- neovibe-user-<uid>, so no two
# users' per-user profiles replace each other in the kernel. A literal path rather than an @{HOME}
# glob (which works too): the grant covers this user's install and nobody else's. neovibe itself
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

# apparmor_plan EXE UID: the profile's name for EXE (the package's own for /usr/lib/neovibe/shell,
# else neovibe-user-UID), where this installer writes it (AA_SRC), where it is installed
# (AA_TARGET), and the two commands (AA_CMD1 installs it, AA_CMD2 loads it).
apparmor_plan() {
	if [ "$1" = /usr/lib/neovibe/shell ]; then AA_NAME=neovibe; else AA_NAME=neovibe-user-$2; fi
	AA_SRC=$NV_DATA/neovibe/apparmor/$AA_NAME
	AA_TARGET=$NV_APPARMOR_D/$AA_NAME
	_ap_src=$(sh_quote "$AA_SRC")
	_ap_dst=$(sh_quote "$AA_TARGET")
	AA_CMD1="sudo install -m 0644 $_ap_src $_ap_dst"
	AA_CMD2="sudo apparmor_parser -r $_ap_dst"
}

# apparmor_render NAME EXE: the profile text (packaging/apparmor/neovibe, with NAME and EXE).
apparmor_render() {
	_arn_path=$(apparmor_escape "$2")
	cat <<'EOF'
# AppArmor profile for neovibe's `shell` (packaging/apparmor/neovibe).
#
# Ubuntu 23.10 and later set kernel.apparmor_restrict_unprivileged_userns=1: a program may create a
# user namespace only while an AppArmor profile that grants `userns` confines it. WebKitGTK 6.0 runs
# its web and network processes in a bwrap sandbox, which needs one, so without this profile the
# agent panel cannot start. The profile grants that and nothing else: it is unconfined otherwise,
# the same shape as the profiles Ubuntu's own apparmor package ships for its WebKit applications
# (epiphany, for one). Every process neovibe starts inherits it.
#
# The .deb installs this file as /etc/apparmor.d/neovibe and loads nothing (it has no maintainer
# scripts): `sudo apparmor_parser -r /etc/apparmor.d/neovibe`, or the next restart, loads it. For
# any other install, the curl installer and neovibe itself write this same text with that install's
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
# complete by now, and neovibe writes the same file and shows the same steps itself.
apparmor_note() {
	apparmor_restricted
	if [ "$AA_RESTRICTED" != 1 ]; then return 0; fi
	# The path AppArmor attaches by is the resolved one, the path `shell` runs from.
	_an_lib=$(cd -P -- "$NV_LIB" 2>/dev/null && pwd -P) || _an_lib=$NV_LIB
	_an_exe=$_an_lib/shell
	_an_uid=$(id -u 2>/dev/null) || _an_uid=
	_an_why="this system restricts unprivileged user namespaces (Ubuntu's AppArmor rule), so the agent panel's sandbox (WebKit's bwrap) needs an AppArmor profile for this install"
	_an_hatch="until then neovibe shows these steps in the agent panel's place; starting it with WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1 also works, but that removes the operating system's sandbox from the process that renders the model's output"
	case $_an_uid in
	'' | *[!0-9]*)
		warn "$_an_why, and \`id -u\` gave no user id to name it by: neovibe shows the steps in the agent panel's place"
		return 0
		;;
	esac
	case $_an_exe in
	*[[:cntrl:]]*)
		warn "$_an_why, and $NV_LIB holds a control character, which no profile can name: install neovibe under a plainer HOME, or $_an_hatch"
		return 0
		;;
	esac
	apparmor_plan "$_an_exe" "$_an_uid"
	if [ "$OPT_DRY_RUN" = 1 ]; then
		say "$_an_why: would write it to $AA_SRC"
		return 0
	fi
	_an_text=$(apparmor_render "$AA_NAME" "$_an_exe")
	if ! mkdir -p -- "$NV_DATA/neovibe/apparmor" ||
		! printf '%s\n' "$_an_text" >"$AA_SRC.tmp.$$" ||
		! mv -- "$AA_SRC.tmp.$$" "$AA_SRC"; then
		rm -f -- "$AA_SRC.tmp.$$"
		warn "$_an_why, and it could not be written to $AA_SRC (see the message above): neovibe writes it itself and shows the steps in the agent panel's place"
		return 0
	fi
	_an_have=
	if [ -f "$AA_TARGET" ]; then _an_have=$(cat -- "$AA_TARGET" 2>/dev/null) || _an_have=; fi
	if [ -n "$_an_have" ] && [ "$_an_have" = "$_an_text" ]; then
		say "the AppArmor profile $AA_TARGET for this install is in place; it loads at boot, or now with: $AA_CMD2"
		return 0
	fi
	say "$_an_why. It is written to $AA_SRC; install and load it once:"
	say "    $AA_CMD1"
	say "    $AA_CMD2"
	say "$_an_hatch"
}

# fetch_skia_binaries WORKDIR PINS_FILE: spec §4.1/§15 -- installer-claude-4. `cargo build` on a
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
		die "$_fsb_pins names no SKIA_BINARIES_URL_UPSTREAM/SKIA_BINARIES_SHA256: not a neovibe checkout's pins.env. Report it at $NV_ISSUES"
	fi
	_fsb_name=${_fsb_url##*/}
	# In test mode, redirected to a fixture server the same way node_dist_base is -- deliberately
	# never falling back to the real GitHub URL when the override is unset, so a test that forgets
	# to set it fails fast against a refused loopback port instead of downloading the real archive.
	if [ "$NV_TEST_MODE" = 1 ]; then
		_fsb_get=${NEOVIBE_INSTALL_TEST_SKIA_BASE_URL:-http://127.0.0.1:1}/$_fsb_name
	else
		_fsb_get=$_fsb_url
	fi
	_fsb_out=$_fsb_workdir/$_fsb_name
	curl_get "$_fsb_get" "$_fsb_out.part"
	if [ "$CG_OK" != 1 ]; then die "could not download the pinned Skia binaries ($_fsb_get): $CG_ERR"; fi
	_fsb_have=$(file_sha256 "$_fsb_out.part")
	if [ "$_fsb_have" != "$_fsb_sha" ]; then
		rm -f -- "$_fsb_out.part" || :
		die "checksum mismatch for $_fsb_name: expected $_fsb_sha, got $_fsb_have. Refusing to build against an unverified Skia archive; re-run, and report it at $NV_ISSUES if it persists"
	fi
	mv -- "$_fsb_out.part" "$_fsb_out" || die "cannot rename $_fsb_out.part"
	NV_SKIA_ARCHIVE=$_fsb_out
}

# do_from_source: spec §6.2 --from-source. check_glibc is deliberately not called -- building from
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
	# F4 (v1-dist whole-branch review, 2026-09-28): fetch_skia_binaries always feeds SKIA_BINARIES_URL
	# the one pinned Skia archive, which is x86_64, and skia-bindings 0.153.3's try_prepare_download
	# unpacks whatever it is given without checking the target -- so a from-source build on any other
	# architecture always failed anyway, just much later (after check_build_tools, check_gtk_webkit, a
	# full clone and most of a `cargo build`) and with a confusing linker error instead of a clear one.
	# Refusing here, up front, loses no working capability: no aarch64 Skia archive has ever been
	# pinned. Pinning one (selected by uname -m, in pins.env) is post-v1 -- nobody on this project can
	# test an ARM build.
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
		NV_INSTALLED_VERSION=$(kv_get "$NV_LIB/RELEASE" NEOVIBE_VERSION)
		if ! printf '%s\n' "$NV_INSTALLED_VERSION" | grep -Eqx "$NV_VERSION_ERE"; then NV_INSTALLED_VERSION=; fi
		rev_of_release "$NV_LIB/RELEASE" "remove $NV_LIB (only the program: your settings and state live elsewhere)"
		NV_OLD_REV7=$RR_REV7
	fi
	acquire_lock
	recover_interrupted

	if [ -n "$OPT_CHECKOUT" ]; then
		synthesize_release_from_checkout "$OPT_CHECKOUT"
		# F3 fix-round-1 regression (v1-dist whole-branch review, fix round 2, 2026-09-28):
		# NV_FROM_SOURCE_DIR used to stay exactly what --checkout was given, so a relative --checkout
		# resolved against run_in's post-cd cwd (the checkout itself) at every later use, not against
		# the caller's own cwd -- "--checkout neovibe", run from neovibe's parent directory,
		# expanded "$NV_FROM_SOURCE_DIR/target" to the plain string "neovibe/target" here, but by the
		# time run_in's cargo call actually saw that string it had already cd'd into neovibe, so the
		# build landed at neovibe/neovibe/target while the rm loop and from_source_stage (which run
		# outside run_in, still against the caller's own cwd) kept reading neovibe/target: a working
		# invocation started failing staging with "did not produce target/release/shell" once
		# --target-dir started pinning that path (this same F3). synthesize_release_from_checkout
		# above already confirmed $OPT_CHECKOUT/.git exists, so this cannot fail on a directory that
		# is not there.
		NV_FROM_SOURCE_DIR=$(CDPATH='' cd -P -- "$OPT_CHECKOUT" && pwd -P) || die "cannot resolve --checkout $OPT_CHECKOUT to an absolute path"
	else
		check_base_url "${OPT_BASE_URL:-$NV_DEFAULT_BASE_URL}"
		obtain_release
		# F2 (v1-dist whole-branch review, 2026-09-28): do_install's own downgrade refusal
		# (semver_cmp, below) used to be the only caller -- --from-source never refused a downgrade
		# at all, so a mirror (--base-url) serving an older, genuinely signed release rolled a
		# from-source user silently backward, which is exactly the rollback case spec §6.4 says a
		# signature does not cover. --checkout has no release server to roll back from, so this sits
		# only in the else branch above.
		if [ -n "$NV_INSTALLED_VERSION" ]; then
			_dfs_c=$(semver_cmp "$NV_VERSION" "$NV_INSTALLED_VERSION")
			if [ "$_dfs_c" = -1 ] && [ -z "$OPT_VERSION" ]; then
				die "neovibe $NV_INSTALLED_VERSION is installed, and the release offered is older ($NV_VERSION): refusing to downgrade silently (a stale mirror or a withdrawn release would look like this). To install $NV_VERSION anyway: --version $NV_VERSION"
			fi
		fi
		fetch_verified_release
		clone_public_neovibe "$NV_VERSION"
		if [ -n "$OPT_VERDANDI_CHECKOUT" ]; then resolve_verdandi_source; fi
	fi

	_dfs_node_workdir=$NV_CACHE_NV/from-source-node
	rm -rf -- "$_dfs_node_workdir" || die "cannot remove the stale $_dfs_node_workdir"
	mkdir -p -- "$_dfs_node_workdir" || die "cannot create $_dfs_node_workdir"
	sidecar_download_node "$_dfs_node_workdir" 0
	_dfs_node_top=$_dfs_node_workdir/node-$REL_NODE_VERSION-linux-$NV_NODE_ARCH
	tar -xf "$SDN_TARBALL" -C "$_dfs_node_workdir" || die "could not unpack $SDN_TARBALL: report it at $NV_ISSUES"
	if [ ! -x "$_dfs_node_top/bin/node" ] || [ ! -x "$_dfs_node_top/bin/npm" ]; then
		die "$SDN_TARBALL does not contain bin/node and bin/npm at $_dfs_node_top: report it at $NV_ISSUES"
	fi

	_dfs_skia_workdir=$NV_CACHE_NV/from-source-skia
	rm -rf -- "$_dfs_skia_workdir" || die "cannot remove the stale $_dfs_skia_workdir"
	mkdir -p -- "$_dfs_skia_workdir" || die "cannot create $_dfs_skia_workdir"
	fetch_skia_binaries "$_dfs_skia_workdir" "$NV_FROM_SOURCE_DIR/packaging/pins.env"

	say "building neovibe $NV_VERSION from source in $NV_FROM_SOURCE_DIR"
	# F3 (v1-dist whole-branch review, 2026-09-28): from_source_stage always reads
	# <src>/target/release, but cargo honours the caller's own CARGO_TARGET_DIR, [build] target-dir
	# or target -- a fresh clone with any of those set died staging ("did not produce
	# target/release/shell"), and a --checkout tree with an OLDER target/release already present
	# (e.g. the owner's own dev loop, ./install.sh run twice against two different pins) silently
	# staged the stale binaries under a RELEASE naming the CURRENT HEAD. --target-dir here beats both
	# the env var and the config key (cargo's own precedence), so staging's own assumption holds for
	# both of those; removing each shipped binary first means a build that is skipped entirely
	# (nothing rebuilt, cargo exits 0) can never leave a stale one for staging to find.
	# F3 fix-round-1 follow-up (v1-dist whole-branch review, fix round 2, 2026-09-28): "always true"
	# above overstated it -- a caller's own `[build] target` (in cargo's config, not this script's)
	# or a CARGO_BUILD_TARGET in the environment is neither the env var nor the config key
	# --target-dir beats; cargo still nests a target/<triple>/release under whatever --target-dir
	# names once a target triple is in play, which this from-source build never passes. On such a
	# host the rm loop above still does its job (no stale binary can survive to be staged), but the
	# build itself is not neutralised: staging dies honestly with "did not produce
	# target/release/shell" rather than shipping anything wrong. Not fixed here: pinning an explicit
	# `--target x86_64-unknown-linux-gnu` (matching the x86_64-only refusal above) and staging from
	# target/<triple>/release instead would neutralise it, at the cost of changing the layout this
	# from-source build produces for every caller, not only the ones with build.target set; left as
	# a known, narrow gap rather than risked here.
	for _dfs_b in $NV_BINARIES; do
		rm -f -- "$NV_FROM_SOURCE_DIR/target/release/$_dfs_b" || die "cannot remove the stale $NV_FROM_SOURCE_DIR/target/release/$_dfs_b"
	done
	run_in "$NV_FROM_SOURCE_DIR" env "PATH=$_dfs_node_top/bin:$PATH" "SKIA_BINARIES_URL=file://$NV_SKIA_ARCHIVE" \
		cargo build --release --locked --target-dir "$NV_FROM_SOURCE_DIR/target" -p shell -p agent -p supervisor --bins

	from_source_stage "$NV_FROM_SOURCE_DIR" "$NV_RELEASE_PATH"
	finish_install
	# installer-claude-6 (+installer-codex-8): on_exit now removes this run's own leftovers (this
	# workdir and from-source-node included) on every exit path, success or not, unless
	# --keep-build -- the ad-hoc cleanup this line alone used to do only ran on success, so a death
	# anywhere above (including inside finish_install) left both behind for good.
}

# dry_run_new_rev: a dry run unpacks nothing, but which sidecar the new install uses decides what
# ensure_sidecar and prune_sidecars report, so the same RELEASE is read without writing anything:
# out of the --tarball file (already checked against --sums), or as the release's own RELEASE asset
# checked against SHA256SUMS -- spec §4.3: the installer trusts a RELEASE that passed SHA256SUMS,
# and the asset is the file the tarball carries. Sets NV_NEW_REV7; it stays empty, and the reports
# say so, only when SHA256SUMS lists no RELEASE or the tarball cannot be read that way.
dry_run_new_rev() {
	NV_NEW_REV7=
	_dr_text=
	_dr_top=neovibe-$NV_VERSION-x86_64-linux
	if [ -n "$OPT_TARBALL" ]; then
		_dr_from="read from $NV_TARBALL_NAME"
		for _dr_m in "$_dr_top/lib/neovibe/RELEASE" "./$_dr_top/lib/neovibe/RELEASE"; do
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
				fetch "$NV_REL_URL/RELEASE" - RELEASE
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

# unpack_new: the verified tarball, unpacked in the cache and checked, becomes $NV_LIB.new.
unpack_new() {
	_un_top=$NV_STAGE/neovibe-$NV_VERSION-x86_64-linux
	NV_STAGE_TOP=$_un_top
	if [ "$OPT_DRY_RUN" = 1 ]; then
		say "would unpack $NV_TARBALL_NAME into $NV_STAGE, check its layout, and copy its lib/neovibe to $NV_LIB.new"
		dry_run_new_rev
		return 0
	fi
	mkdir -p -- "$NV_STAGE" || die "cannot create $NV_STAGE"
	tar -xzf "$NV_TARBALL" -C "$NV_STAGE" || die "could not unpack $NV_TARBALL_NAME: it is not a valid neovibe release. Nothing was changed and your current install is untouched; report it at $NV_ISSUES"
	for _un_f in bin/neovibe share/applications/neovibe.desktop; do
		if [ ! -f "$_un_top/$_un_f" ]; then die "$NV_TARBALL_NAME has no $_un_f: it is not a valid neovibe release. Nothing was changed; report it at $NV_ISSUES"; fi
	done
	for _un_f in $NV_BINARIES neovibe-setup; do
		if [ ! -f "$_un_top/lib/neovibe/$_un_f" ] || [ ! -x "$_un_top/lib/neovibe/$_un_f" ]; then
			die "$NV_TARBALL_NAME has no executable lib/neovibe/$_un_f: it is not a valid neovibe release. Nothing was changed; report it at $NV_ISSUES"
		fi
	done
	for _un_f in $NV_LICENCE_FILES; do
		if [ ! -f "$_un_top/share/licenses/neovibe/$_un_f" ]; then die "$NV_TARBALL_NAME has no share/licenses/neovibe/$_un_f: it is not a valid neovibe release. Nothing was changed; report it at $NV_ISSUES"; fi
	done
	if [ ! -f "$_un_top/lib/neovibe/RELEASE" ]; then die "$NV_TARBALL_NAME has no lib/neovibe/RELEASE: it is not a valid neovibe release. Nothing was changed; report it at $NV_ISSUES"; fi
	parse_release "$_un_top/lib/neovibe/RELEASE"
	if [ "$REL_VERSION" != "$NV_VERSION" ]; then
		die "$NV_TARBALL_NAME's RELEASE says $REL_VERSION, not $NV_VERSION: refusing. Report it at $NV_ISSUES"
	fi
	NV_NEW_REV7=$REL_REV7
	mkdir -p -- "$NV_LIBROOT" || die "cannot create $NV_LIBROOT"
	NV_NEW_CREATED=1
	cp -R -- "$_un_top/lib/neovibe" "$NV_LIB.new" || die "cannot copy the new install to $NV_LIB.new (is the disk full?)"
}

# swap_in: spec §6.5 step 3.
swap_in() {
	if [ -e "$NV_LIB.old" ] || [ -L "$NV_LIB.old" ]; then run rm -rf -- "$NV_LIB.old"; fi
	if [ -e "$NV_LIB" ] || [ -L "$NV_LIB" ]; then run mv -- "$NV_LIB" "$NV_LIB.old"; fi
	run mv -- "$NV_LIB.new" "$NV_LIB"
	NV_NEW_CREATED=0
	if [ -e "$NV_LIB.old" ] || [ -L "$NV_LIB.old" ]; then run rm -rf -- "$NV_LIB.old"; fi
}

# desktop_exec_quote PATH: PATH as one quoted Exec argument, by the Desktop Entry Specification's
# rules. Inside double quotes `"`, `` ` ``, `$` and `\` take a backslash; then the string-value
# escape rule applies on top, doubling every backslash (the spec's "\\\\" for a literal
# backslash). GLib refuses a single `\$` outright, measured: "Key file contains key Exec which has
# a value that cannot be interpreted". A literal `%` is `%%`.
desktop_exec_quote() {
	_dq=$(printf '%s\n' "$1" | sed -e 's/[\\"`$]/\\&/g' -e 's/\\/\\\\/g' -e 's/%/%%/g') || die "sed failed"
	printf '"%s"\n' "$_dq"
}

# stage_files: the launcher (with its marker line, spec §6.5, MIN-2), the desktop entry (its Exec
# escaped) and the licences, each written to a temporary name in its destination directory. This
# runs before the swap, so a directory that cannot be written stops the run while the old install
# is still in place and untouched: written after it, an unwritable ~/.local/bin left the new lib
# with the old launcher and licences, and no neovibe.old to go back to (Task 9 review).
# commit_files renames them into place after the swap; on_exit removes any left behind.
stage_files() {
	NV_TMP_LAUNCHER=$NV_BINDIR/.neovibe.tmp.$$
	_sf_apps=$NV_DATA/applications
	_sf_lic=$NV_DATA/licenses/neovibe
	_sf_exec=$(desktop_exec_quote "$NV_BINDIR/neovibe")
	run mkdir -p -- "$NV_BINDIR" "$_sf_apps" "$_sf_lic"
	if [ "$OPT_DRY_RUN" = 1 ]; then
		say "would write the launcher $NV_BINDIR/neovibe (from the tarball's bin/neovibe, with the line '$NV_LAUNCHER_MARKER')"
		say "would write $_sf_apps/neovibe.desktop with Exec=$_sf_exec --quiet %f"
		for _sf_f in $NV_LICENCE_FILES; do say "would write $_sf_lic/$_sf_f"; done
		return 0
	fi
	_sf_keep='nothing installed was changed: make it writable and re-run'
	cat -- "$NV_STAGE_TOP/bin/neovibe" >"$NV_TMP_LAUNCHER" || die "cannot write $NV_TMP_LAUNCHER; $_sf_keep"
	if ! grep -Fqx -- "$NV_LAUNCHER_MARKER" "$NV_TMP_LAUNCHER"; then
		# Appended, not inserted: the launcher's --help prints its own doc-comment lines near the
		# top by a fixed line range, and inserting the marker there would shift it.
		printf '\n%s\n' "$NV_LAUNCHER_MARKER" >>"$NV_TMP_LAUNCHER" || die "cannot write $NV_TMP_LAUNCHER; $_sf_keep"
	fi
	chmod 0755 "$NV_TMP_LAUNCHER" || die "cannot chmod $NV_TMP_LAUNCHER; $_sf_keep"
	NV_EXEC="$_sf_exec --quiet %f" awk '
		/^Exec=/ { print "Exec=" ENVIRON["NV_EXEC"]; next }
		/^TryExec=/ { next }
		{ print }' "$NV_STAGE_TOP/share/applications/neovibe.desktop" >"$_sf_apps/.neovibe.desktop.tmp.$$" ||
		die "cannot write $_sf_apps/.neovibe.desktop.tmp.$$; $_sf_keep"
	for _sf_f in $NV_LICENCE_FILES; do
		cp -- "$NV_STAGE_TOP/share/licenses/neovibe/$_sf_f" "$_sf_lic/.$_sf_f.tmp.$$" ||
			die "cannot write $_sf_lic/.$_sf_f.tmp.$$; $_sf_keep"
	done
}

# commit_files: after the swap, stage_files' files are renamed into place (every executable outside
# $NV_LIB goes in by temporary name, chmod, then mv: spec §6.5 step 3). A launcher that is not
# neovibe's -- no marker, not the old root install.sh's -- is moved aside, never overwritten.
commit_files() {
	_cf_dst=$NV_BINDIR/neovibe
	if [ -L "$_cf_dst" ] || [ -e "$_cf_dst" ]; then
		_cf_ours=0
		if [ ! -L "$_cf_dst" ] && [ -f "$_cf_dst" ]; then
			if grep -Fqx -- "$NV_LAUNCHER_MARKER" "$_cf_dst"; then _cf_ours=1; fi
			_cf_l2=$(sed -n '2p' "$_cf_dst") || _cf_l2=
			if [ "$_cf_l2" = "$NV_OLD_LAUNCHER_LINE2" ]; then _cf_ours=1; fi
		fi
		if [ "$_cf_ours" = 0 ]; then
			_cf_bak=$NV_BINDIR/neovibe.bak-$(date +%Y%m%d-%H%M%S)
			if [ -e "$_cf_bak" ] || [ -L "$_cf_bak" ]; then _cf_bak=$_cf_bak-$$; fi
			warn "$_cf_dst was not installed by this installer, so it is moved to $_cf_bak rather than overwritten; delete it once you no longer need it"
			run mv -- "$_cf_dst" "$_cf_bak"
		fi
	fi
	if [ "$OPT_DRY_RUN" = 1 ]; then return 0; fi
	# The new lib is in place from here on: a failure leaves the install unfinished, and a re-run
	# finishes it (install_complete keeps it from saying "up to date" instead).
	_cf_fin="neovibe $NV_VERSION is in $NV_LIB but not finished: fix this and re-run the installer to finish it"
	mv -- "$NV_TMP_LAUNCHER" "$_cf_dst" || die "cannot move the launcher into $_cf_dst; $_cf_fin"
	mv -- "$NV_DATA/applications/.neovibe.desktop.tmp.$$" "$NV_DATA/applications/neovibe.desktop" ||
		die "cannot write $NV_DATA/applications/neovibe.desktop; $_cf_fin"
	for _cf_f in $NV_LICENCE_FILES; do
		mv -- "$NV_DATA/licenses/neovibe/.$_cf_f.tmp.$$" "$NV_DATA/licenses/neovibe/$_cf_f" ||
			die "cannot write $NV_DATA/licenses/neovibe/$_cf_f; $_cf_fin"
	done
	# Written last, deliberately (installer-claude-5): an interrupt or a failed mv anywhere above
	# leaves this stamp naming an OLDER version (or missing outright, on a first install), so
	# install_complete below correctly reads the install as unfinished -- even though every
	# individual file it checks (the launcher, the desktop entry, the three licence files) can
	# already exist, from a previous, complete install of a *different* version, and would
	# otherwise pass every one of its existence checks on its own.
	printf '%s\n' "$NV_VERSION" >"$NV_DATA/licenses/neovibe/.installed-version.tmp.$$" ||
		die "cannot write $NV_DATA/licenses/neovibe/.installed-version.tmp.$$; $_cf_fin"
	mv -- "$NV_DATA/licenses/neovibe/.installed-version.tmp.$$" "$NV_DATA/licenses/neovibe/.installed-version" ||
		die "cannot write $NV_DATA/licenses/neovibe/.installed-version; $_cf_fin"
}

# install_complete: NV_COMPLETE=1 when everything a finished install writes outside $NV_LIB is in
# place: the launcher with its marker, the desktop entry, the three licence files, and a version
# stamp naming exactly $NV_VERSION. "Up to date" needs it as well as the sidecar: after a failure or
# an interrupt past the swap, the re-run the errors ask for once said "up to date" and left no
# launcher for good (Task 9 review). The stamp check closes a second way that could happen
# (installer-claude-5): every file above already existing, from a previous complete install of an
# OLDER version, while this run's own commit_files was interrupted (or one mv in it failed) before
# writing any of them -- without the stamp, every existence check above passed anyway, and the
# stale files (notably SOURCE, which names a specific release) were never replaced.
install_complete() {
	NV_COMPLETE=0
	if [ -L "$NV_BINDIR/neovibe" ] || [ ! -f "$NV_BINDIR/neovibe" ]; then return 0; fi
	if ! grep -Fqx -- "$NV_LAUNCHER_MARKER" "$NV_BINDIR/neovibe"; then return 0; fi
	if [ ! -f "$NV_DATA/applications/neovibe.desktop" ]; then return 0; fi
	for _ic_f in $NV_LICENCE_FILES; do
		if [ ! -f "$NV_DATA/licenses/neovibe/$_ic_f" ]; then return 0; fi
	done
	_ic_stamp=$(cat -- "$NV_DATA/licenses/neovibe/.installed-version" 2>/dev/null) || _ic_stamp=
	if [ "$_ic_stamp" != "$NV_VERSION" ]; then return 0; fi
	NV_COMPLETE=1
}

# prune_sidecars: spec §6.5 step 4. A sidecar <rev7> directory goes only when no installed RELEASE
# names it, it is not the new install's rev, and it is not the rev of the install just replaced
# (kept until the next successful upgrade: a .deb and a tarball install share this directory, and
# running windows still use the replaced one). Anything not named like a rev is not ours to judge.
prune_sidecars() {
	if [ ! -d "$NV_SIDECAR_ROOT" ]; then return 0; fi
	# M3 (v1-dist whole-branch review, 2026-09-28): a symlinked $NV_SIDECAR_ROOT (planted by the
	# user, in their own $XDG_DATA_HOME -- nothing this installer would make) used to be followed
	# with no check at all, so a 7-hex-named directory BEHIND the link -- a user's own, unrelated
	# directory that merely happens to be named like a short git sha -- was removed as if it were
	# one of neovibe's own sidecars. The same rule $NV_DATA/neovibe already holds in do_uninstall:
	# a link is fine as long as it leads to a real directory actually named "sidecar".
	_ps_real=$(cd -P -- "$NV_SIDECAR_ROOT" 2>/dev/null && pwd -P) || _ps_real=
	case $_ps_real in
	*/sidecar) ;;
	*)
		warn "$NV_SIDECAR_ROOT leads to ${_ps_real:-a directory that cannot be entered}, not to a directory named sidecar, so nothing behind it was removed: this installer never made that link. Remove what neovibe put there yourself if you no longer need it"
		return 0
		;;
	esac
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
			say "removing the sidecar for verdandi $_pr_n: no installed neovibe uses it"
		fi
		run rm -rf -- "$_pr_d"
	done
}

# path_warning: which `neovibe` this PATH runs (the old install.sh's /usr/bin warning, generalised).
path_warning() {
	_pw_first=
	_pw_ifs=$IFS
	IFS=:
	set -f
	for _pw_d in $PATH; do
		_pw_d=${_pw_d%/}
		case $_pw_d in /*) ;; *) continue ;; esac
		if [ -f "$_pw_d/neovibe" ] && [ -x "$_pw_d/neovibe" ]; then
			_pw_first=$_pw_d/neovibe
			break
		fi
	done
	set +f
	IFS=$_pw_ifs
	if [ -z "$_pw_first" ]; then
		warn "$NV_BINDIR is not on your PATH: add it (in your shell's profile: export PATH=\"\$HOME/.local/bin:\$PATH\"), or run $NV_BINDIR/neovibe"
	elif [ "$_pw_first" != "$NV_BINDIR/neovibe" ]; then
		warn "\`neovibe\` on this PATH runs $_pw_first, not the one just installed: put $NV_BINDIR before ${_pw_first%/neovibe} in PATH, remove the other install, or run $NV_BINDIR/neovibe"
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
	acquire_lock
	recover_interrupted
	# Read now, before anything is downloaded or changed: prune_sidecars keeps this rev, and a
	# system RELEASE that cannot be read stops the run here rather than after the swap.
	rev_of_release "$NV_SYSTEM_RELEASE" "$NV_SYSTEM_REMEDY"
	NV_SYS_REV7=$RR_REV7

	NV_INSTALLED_VERSION=
	NV_OLD_REV7=
	if [ -f "$NV_LIB/RELEASE" ]; then
		NV_INSTALLED_VERSION=$(kv_get "$NV_LIB/RELEASE" NEOVIBE_VERSION)
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
			die "neovibe $NV_INSTALLED_VERSION is installed, and the release offered is older ($NV_VERSION): refusing to downgrade silently (a stale mirror or a withdrawn release would look like this). To install $NV_VERSION anyway: --version $NV_VERSION"
		fi
		if [ "$_di_c" = 0 ]; then
			sidecar_state "$NV_OLD_REV7"
			install_complete
			if [ "$NV_SIDECAR_PRESENT" = 1 ] && [ "$NV_COMPLETE" = 1 ]; then
				say "neovibe $NV_VERSION is up to date"
				# M5 (v1-dist whole-branch review, 2026-09-28): this branch used to `return 0`
				# straight after, so re-running the same installer with --with-nvim/--yes after a
				# --no-nvim install (or simply once an adequate nvim's absence is noticed, or
				# network access to github.com came back) fetched nothing and said nothing --
				# do_nvim_offer already runs exactly this offer standalone (what plain
				# `neovibe setup`'s own second call does); the same offer now runs here too,
				# before the early return, from the already-read $NV_LIB/RELEASE (the installed
				# version and the one just offered are the same version here, so it is the right
				# RELEASE to read REL_NVIM_VERSION from).
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
				say "neovibe $NV_VERSION is installed but not finished (its launcher, desktop entry or licences are missing): installing it again"
			fi
		fi
	fi

	if [ -n "$OPT_TARBALL" ]; then
		if [ "$OPT_DRY_RUN" = 1 ]; then
			NV_TARBALL=$OPT_TARBALL
		else
			# Copied first, then hashed and unpacked from the copy alone: hashed where it lay and
			# unpacked from a second read, a tarball swapped in between was installed (see
			# obtain_release; Task 9 review).
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
		download_verified "$NV_TARBALL_NAME"
		NV_TARBALL=$NV_DL/$NV_TARBALL_NAME
	fi
	say "installing neovibe $NV_VERSION${NV_INSTALLED_VERSION:+ (replacing $NV_INSTALLED_VERSION)}"
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
# Uninstall (spec §6.6)

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
	rev_of_release "$NV_SYSTEM_RELEASE" "$NV_SYSTEM_REMEDY"
	_du_sys=$RR_REV7
	_du_launcher_note=
	_du_link_note=
	_du_sidecar_link_note=
	# The targets are this function's positional parameters, one path each: never joined into one
	# string and split again, which once let a newline in an XDG variable name any directory in
	# $HOME (Task 9 review; xdg_dir now refuses one as well).
	set -- "$NV_LIB" "$NV_LIB.new" "$NV_LIB.old"
	if [ -L "$NV_BINDIR/neovibe" ] || [ -e "$NV_BINDIR/neovibe" ]; then
		if [ ! -L "$NV_BINDIR/neovibe" ] && [ -f "$NV_BINDIR/neovibe" ] && grep -Fqx -- "$NV_LAUNCHER_MARKER" "$NV_BINDIR/neovibe"; then
			set -- "$@" "$NV_BINDIR/neovibe"
		else
			_du_launcher_note="$NV_BINDIR/neovibe was left alone: it was not installed by this installer (no '$NV_LAUNCHER_MARKER' line)"
		fi
	fi
	set -- "$@" "$NV_DATA/applications/neovibe.desktop" "$NV_DATA/licenses/neovibe"
	# What neovibe keeps under $NV_DATA/neovibe (a private nvim, the sidecars) goes only when that
	# really is a directory named neovibe, wherever a link puts it (moved to another disk, say). A
	# link to anything else -- ~/.config, once, whose nvim went with it -- was not made by this
	# installer, and nothing behind it is removed (spec §6.6: nothing named nvim outside neovibe's
	# own directory; Task 9 review).
	_du_ours=1
	if [ -d "$NV_DATA/neovibe" ]; then
		_du_real=$(cd -P -- "$NV_DATA/neovibe" 2>/dev/null && pwd -P) || _du_real=
		case $_du_real in
		*/neovibe) ;;
		*)
			_du_ours=0
			_du_link_note="$NV_DATA/neovibe leads to ${_du_real:-a directory that cannot be entered}, not to a directory named neovibe, so nothing under it (an nvim, sidecars) was removed: this installer never made that link. Remove what neovibe put there yourself if you no longer need it"
			;;
		esac
	fi
	if [ "$_du_ours" = 1 ]; then
		# The AppArmor profile apparmor_note wrote goes too; the copy installed under
		# /etc/apparmor.d is root's, and the note below says how to remove it.
		set -- "$@" "$NV_DATA/neovibe/nvim" "$NV_DATA/neovibe/apparmor"
		# M3 (v1-dist whole-branch review, 2026-09-28): the check just above only ever verified
		# $NV_DATA/neovibe itself, not $NV_SIDECAR_ROOT (its own "sidecar" subdirectory) -- a
		# symlinked sidecar root, planted by the user in their own $XDG_DATA_HOME, was followed
		# with no check at all, so a 7-hex-named directory BEHIND the link (a user's own,
		# unrelated directory merely named like a short git sha) was removed as if it were one of
		# neovibe's own sidecars. Same rule, same shape: a link is fine as long as it leads to a
		# real directory actually named "sidecar".
		_du_sidecar_ours=0
		if [ -d "$NV_SIDECAR_ROOT" ]; then
			_du_sidecar_real=$(cd -P -- "$NV_SIDECAR_ROOT" 2>/dev/null && pwd -P) || _du_sidecar_real=
			case $_du_sidecar_real in
			*/sidecar) _du_sidecar_ours=1 ;;
			*) _du_sidecar_link_note="$NV_SIDECAR_ROOT leads to ${_du_sidecar_real:-a directory that cannot be entered}, not to a directory named sidecar, so nothing behind it was removed: this installer never made that link. Remove what neovibe put there yourself if you no longer need it" ;;
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
		fi
	fi
	# installer-claude-6: nvim-build, verdandi-src, verdandi-src-build, from-source-node and
	# from-source-skia were missing from this list -- a --keep-build from-source run (or one that
	# died before on_exit's own cleanup, on an install this old) left them behind for good, and
	# --uninstall --purge reported nothing to remove.
	for _du_p in download unpack sidecar-build src nvim-build verdandi-src verdandi-src-build \
		from-source-node from-source-skia; do
		set -- "$@" "$NV_CACHE_NV/$_du_p"
	done
	if [ "$OPT_PURGE" = 1 ]; then
		# Exactly two (spec §6.6): what shell's config_dir falls back to -- never NEOVIBE_CONFIG_DIR,
		# which may point at another program's config, and never XDG_CONFIG_HOME, which shell does
		# not read -- and the state directory by the §6.4 rule.
		set -- "$@" "$NV_HOME/.config/neovibe" "$NV_STATE/neovibe"
	fi

	# Only what exists is removed, so only what exists is checked: a path not there needs nothing,
	# and once the user has removed an outside-$HOME one by hand, as the refusal asks, a re-run
	# goes through (it never could: Task 9 review).
	_du_i=$#
	while [ "$_du_i" -gt 0 ]; do
		_du_p=$1
		shift
		if [ -e "$_du_p" ] || [ -L "$_du_p" ]; then set -- "$@" "$_du_p"; fi
		_du_i=$((_du_i - 1))
	done

	# Every path is checked before any is removed -- and before the lock is taken, so a refusal
	# changes nothing at all: taken first, the lock's exit cleanup removed the very cache
	# directories just refused (Task 9 review).
	_du_bad=
	for _du_p do
		_du_ok=$(under_home "$_du_p")
		if [ "$_du_ok" != 1 ]; then _du_bad="$_du_bad '$_du_p'"; fi
	done
	if [ -n "$_du_bad" ]; then
		die "refusing to uninstall: these paths are not inside $NV_HOME, and this installer only removes paths inside it:$_du_bad. Nothing was removed. They are neovibe's own: remove them yourself if you no longer need them, then re-run to remove the rest"
	fi
	acquire_lock

	# One `rm -rf -- "$p"` per path (spec §6.6).
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
			for _du_p in "$NV_SIDECAR_ROOT" "$NV_DATA/neovibe"; do
				if [ -d "$_du_p" ]; then rmdir -- "$_du_p" 2>/dev/null || :; fi
			done
		fi
		if [ -d "$NV_DATA/licenses" ]; then rmdir -- "$NV_DATA/licenses" 2>/dev/null || :; fi
	fi
	if [ -n "$_du_launcher_note" ]; then say "$_du_launcher_note"; fi
	if [ -n "$_du_link_note" ]; then say "$_du_link_note"; fi
	if [ -n "$_du_sidecar_link_note" ]; then say "$_du_sidecar_link_note"; fi
	# A profile this install's steps put under /etc/apparmor.d stays (this installer never runs
	# sudo): it names a path nothing runs from any more, and removing it takes root.
	_du_uid=$(id -u 2>/dev/null) || _du_uid=
	if [ -n "$_du_uid" ] && [ -f "$NV_APPARMOR_D/neovibe-user-$_du_uid" ]; then
		_du_aa=$(sh_quote "$NV_APPARMOR_D/neovibe-user-$_du_uid")
		say "the AppArmor profile $NV_APPARMOR_D/neovibe-user-$_du_uid stays: remove it with: sudo apparmor_parser -R $_du_aa && sudo rm $_du_aa"
	fi
	if [ "$OPT_DRY_RUN" = 1 ]; then
		say "dry run: nothing was changed"
	elif [ "$_du_n" = 0 ]; then
		say "nothing to remove: neovibe is not installed here"
	else
		say "neovibe is uninstalled"
	fi
	if [ "$OPT_PURGE" != 1 ]; then
		say "kept your settings ($NV_HOME/.config/neovibe) and state ($NV_STATE/neovibe); --uninstall --purge removes them too"
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
	# do_install, do_from_source, do_sidecar_only, do_nvim_only, do_nvim_offer. --uninstall is
	# exempt on purpose (its own comment explains why); --build-sidecar-into is exempt too --
	# neither reads nor writes anything under NV_DATA/XDG_DATA_HOME (its workdir is under
	# NV_CACHE_NV, its output goes to the caller's own --build-sidecar-into DIR) -- it is the AUR
	# package's build() entry point (spec §9), which can run in a sandbox that sets XDG_DATA_HOME
	# outside $HOME for reasons this mode never needs to care about (installer-codex review-2).
	case $OPT_MODE in
	uninstall | build-sidecar-into) ;;
	*) check_data_home ;;
	esac
	trap on_exit EXIT
	trap 'exit 129' HUP
	trap 'exit 130' INT
	trap 'exit 143' TERM
	case $OPT_MODE in
	uninstall) do_uninstall ;;
	sidecar-only) do_sidecar_only ;;
	build-sidecar-into) do_build_sidecar_into ;;
	nvim-only) do_nvim_only ;;
	nvim-offer) do_nvim_offer ;;
	from-source) do_from_source ;;
	*) do_install ;;
	esac
}
main "$@" </dev/null
}
