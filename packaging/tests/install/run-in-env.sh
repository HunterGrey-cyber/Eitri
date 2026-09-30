#!/bin/sh
# Runs one installer invocation in an allowlisted environment (plan 2026-09-27-v1-dist, Task 9;
# Global Constraints: "every installer run, harness or real, goes through this wrapper").
#
#   run-in-env.sh --home DIR --stubs DIR [--stubs DIR]... [--path-tail PATH] [--cwd DIR]
#                 [--set NAME=VALUE]... [--unset NAME]... [--guard-real-home DIR] -- COMMAND [ARG]...
#
# The command runs under `env -i` with only:
#   PATH       the --stubs directories (in the order given; the harness's first one holds the
#              forbidden-command and `claude` stubs), then --path-tail (default /usr/bin:/bin);
#   HOME       --home, and XDG_DATA_HOME / XDG_CACHE_HOME / XDG_STATE_HOME / XDG_CONFIG_HOME under
#              it (each overridable with --set, or dropped with --unset);
#   LANG       the caller's, else C.UTF-8;
#   whatever --set names, which must be EITRI_INSTALL_TEST or an EITRI_INSTALL_TEST_* variable,
#              an XDG_*_HOME, LANG, EITRI_CONFIG_DIR (the purge test's), or CARGO_TARGET_DIR (F3's
#              own reproduction: a caller's CARGO_TARGET_DIR must not move where --from-source's
#              build lands, since staging always reads <src>/target/release).
# Nothing else is inherited: this host exports VERDANDI_CLAUDE_CLI_PATH and VERDANDI_CLAUDE_ACCOUNT,
# and neither may reach an installer run.
#
# Two guards, each fatal (exit 98 / 97) rather than a failure a caller could overlook:
#   - `claude` must resolve to a stub under the new PATH, so the host's real claude can never run;
#   - with --guard-real-home DIR, a find listing (names and mtimes, never contents) of DIR's
#     .local/state/eitri, .config/eitri and .local/share/eitri is hashed before and after the
#     command and must not change. Only the two hashes are kept, in memory.
#
# stdin passes through (the piped-install test feeds the script on it). POSIX sh: it runs under
# bash on the host and dash in the Ubuntu image.
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
		printf "run-in-env: the real HOME's Eitri directories changed during this run\n" >&2
		exit 97
	fi
fi
exit "$W_RC"
