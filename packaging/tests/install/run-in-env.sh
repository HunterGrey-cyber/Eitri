#!/bin/sh
# Runs one installer invocation in an allowlisted environment.
#
#   run-in-env.sh --home DIR --stubs DIR [--stubs DIR]... [--path-tail PATH] [--cwd DIR]
#                 [--set NAME=VALUE]... [--unset NAME]... [--guard-real-home DIR] -- COMMAND [ARG]...
#
# The command runs under `env -i`. PATH contains the --stubs directories in order, then
# --path-tail (default /usr/bin:/bin). The harness supplies forbidden-command and `claude` stubs.
# HOME is --home, with XDG_DATA_HOME / XDG_CACHE_HOME / XDG_STATE_HOME / XDG_CONFIG_HOME under it.
# Each XDG directory can be overridden with --set or dropped with --unset.
# LANG is the caller's value, or C.UTF-8. --set accepts only allowlisted variables.
# CARGO_TARGET_DIR can be set to test that --from-source still builds in <src>/target/release.
# Nothing else is inherited, so unrelated CLI paths and credentials cannot reach the installer.
#
# Two guards fail with exit 98 / 97:
#   - `claude` must resolve to a stub under the new PATH, preventing a real CLI invocation.
#   - With --guard-real-home DIR, a listing of the application's state, config and data directories
#     under DIR is hashed before and after the command and must not change. The listing contains
#     names and mtimes, never contents. Only the two hashes are kept in memory.
#
# stdin passes through for piped installs. The wrapper uses POSIX sh syntax.
set -u

die() {
	printf 'run-in-env: %s\n' "$*" >&2
	exit 96
}

NL='
'
W_HOME=
W_STUBS=
W_TAIL=/usr/bin:/bin
W_CWD=
W_GUARD=
W_SETS=
W_UNSETS=

while [ $# -gt 0 ]; do
	case $1 in
	--home | --stubs | --path-tail | --cwd | --set | --unset | --guard-real-home)
		[ $# -ge 2 ] || die "$1 needs a value"
		case $2 in *"$NL"*) die "$1: a value may not contain a newline" ;; esac
		case $1 in
		--home) W_HOME=$2 ;;
		--stubs)
			case $2 in /*) ;; *) die "--stubs must be absolute" ;; esac
			case $2 in *:*) die "a --stubs directory may not contain ':'" ;; esac
			if [ -z "$W_STUBS" ]; then W_STUBS=$2; else W_STUBS=$W_STUBS:$2; fi
			;;
		--path-tail) W_TAIL=$2 ;;
		--cwd) W_CWD=$2 ;;
		--set)
			case $2 in
			EITRI_INSTALL_TEST=* | EITRI_INSTALL_TEST_*=* | XDG_DATA_HOME=* | XDG_CACHE_HOME=* | \
				XDG_STATE_HOME=* | XDG_CONFIG_HOME=* | LANG=* | EITRI_CONFIG_DIR=* | CARGO_TARGET_DIR=*) ;;
			*) die "--set $2: not an allowlisted variable" ;;
			esac
			W_SETS=$W_SETS$2$NL
			;;
		--unset) W_UNSETS=$W_UNSETS$2$NL ;;
		--guard-real-home) W_GUARD=$2 ;;
		esac
		shift 2
		;;
	--)
		shift
		break
		;;
	*) die "unknown option $1" ;;
	esac
done
[ $# -gt 0 ] || die "no command after --"
case $W_HOME in /*) ;; *) die "--home must be an absolute path" ;; esac
[ -n "$W_STUBS" ] || die "at least one --stubs directory is required"

W_PATH=$W_STUBS:$W_TAIL

# The host's claude must be unreachable: `claude` under the new PATH has to be a stub's own.
W_CLAUDE=$(env -i PATH="$W_PATH" sh -c 'command -v claude' 2>/dev/null) || W_CLAUDE=
W_CLAUDE_OK=0
W_IFS=$IFS
IFS=:
for W_D in $W_STUBS; do
	if [ "$W_CLAUDE" = "$W_D/claude" ]; then W_CLAUDE_OK=1; fi
done
IFS=$W_IFS
if [ "$W_CLAUDE_OK" != 1 ]; then
	printf 'run-in-env: claude resolves to "%s", not a stub: refusing to run\n' "$W_CLAUDE" >&2
	exit 98
fi

real_home_digest() {
	for W_R in "$W_GUARD/.local/state/eitri" "$W_GUARD/.config/eitri" "$W_GUARD/.local/share/eitri"; do
		if [ -e "$W_R" ] || [ -L "$W_R" ]; then
			find "$W_R" -printf '%p %T@\n' 2>&1
		else
			printf 'absent %s\n' "$W_R"
		fi
	done | LC_ALL=C sort | sha256sum
}

# The assignments, in order: the defaults, then --set (env lets a later assignment win), minus
# every --unset name.
W_ASSIGNS="PATH=$W_PATH${NL}HOME=$W_HOME${NL}LANG=${LANG:-C.UTF-8}${NL}"
W_ASSIGNS="${W_ASSIGNS}XDG_DATA_HOME=$W_HOME/.local/share${NL}XDG_CACHE_HOME=$W_HOME/.cache${NL}"
W_ASSIGNS="${W_ASSIGNS}XDG_STATE_HOME=$W_HOME/.local/state${NL}XDG_CONFIG_HOME=$W_HOME/.config${NL}"
W_ASSIGNS=$W_ASSIGNS$W_SETS

# Append the assignments after the command, then rotate the command's words to the end, which
# leaves "$@" as: assignments, command, arguments -- without arrays, and with spaces intact.
W_CMD_COUNT=$#
IFS=$NL
set -f
for W_A in $W_ASSIGNS; do
	case "$NL$W_UNSETS" in *"$NL${W_A%%=*}$NL"*) continue ;; esac
	set -- "$@" "$W_A"
done
set +f
IFS=$W_IFS
W_I=0
while [ "$W_I" -lt "$W_CMD_COUNT" ]; do
	W_A=$1
	shift
	set -- "$@" "$W_A"
	W_I=$((W_I + 1))
done

if [ -n "$W_GUARD" ]; then W_BEFORE=$(real_home_digest); fi
if [ -n "$W_CWD" ]; then cd "$W_CWD" || die "cannot cd to $W_CWD"; fi
env -i "$@"
W_RC=$?
if [ -n "$W_GUARD" ]; then
	W_AFTER=$(real_home_digest)
	if [ "$W_BEFORE" != "$W_AFTER" ]; then
		printf "run-in-env: the real HOME's eitri directories changed during this run\n" >&2
		exit 97
	fi
fi
exit "$W_RC"
