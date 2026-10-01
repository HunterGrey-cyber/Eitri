#!/bin/sh
# What an installer run could have changed in a real HOME, as one sorted listing -- names, types,
# sizes and mtimes, never contents -- so a before/after pair that differs means the run reached the
# real HOME. Used by run-in-env.sh (around every host installer run) and packaging/test_install.py
# (around the container run, which cannot see the real HOME), and tested there.
#
#   real-home-state.sh HOME
#
# Exactly the paths packaging/install.sh creates, replaces or removes, and nothing else, because a
# person using Eitri in the same HOME while the tests run changes the application's own files all
# the time (its prompt history under ~/.local/state/eitri, say) and that must not fail a run:
#   - ~/.local/lib/eitri, with the .new and .old names an upgrade swaps through
#   - ~/.local/bin/eitri, the .bak- copy of a launcher it replaced, and its temporary names
#   - $XDG_DATA_HOME/applications: the desktop entry, the one older releases installed, and the
#     temporary name
#   - $XDG_DATA_HOME/icons/hicolor: the icon files by name (never icon-theme.cache, which
#     gtk-update-icon-cache rewrites but so does every other application that installs an icon)
#   - $XDG_DATA_HOME/licenses/eitri, and $XDG_DATA_HOME/eitri (the private nvim, the sidecars, the
#     AppArmor profile)
#   - $XDG_CACHE_HOME/eitri: the downloads, the unpack and build directories, the lock
#   - ~/.config/eitri and $XDG_STATE_HOME/eitri only as far as `--uninstall --purge` can reach
#     them, which is deleting each whole: so whether the directory is there and what kind of entry
#     it is, but not what is inside (the application writes there itself)
# XDG_DATA_HOME, XDG_CACHE_HOME and XDG_STATE_HOME are read from the caller's environment by
# install.sh's own rule: an absolute value, else the default under HOME.
#
# POSIX sh and GNU find (-printf), as run-in-env.sh already needs.
set -u

case ${1-} in
/*) ;;
*)
	echo "real-home-state: usage: real-home-state.sh ABSOLUTE-HOME" >&2
	exit 2
	;;
esac
H=$1
while :; do
	case $H in
	?*/) H=${H%/} ;;
	*) break ;;
	esac
done

DATA=$H/.local/share
CACHE=$H/.cache
STATE=$H/.local/state
case ${XDG_DATA_HOME-} in /*) DATA=$XDG_DATA_HOME ;; esac
case ${XDG_CACHE_HOME-} in /*) CACHE=$XDG_CACHE_HOME ;; esac
case ${XDG_STATE_HOME-} in /*) STATE=$XDG_STATE_HOME ;; esac

FMT='%p %y %s %T@\n'

# everything at and below each path; one "absent" line for a path that is not there. find does not
# follow a symlink given on its command line, so a link is listed as itself.
tree() {
	for P do
		if [ -e "$P" ] || [ -L "$P" ]; then
			find "$P" -printf "$FMT" 2>&1
		else
			printf 'absent %s\n' "$P"
		fi
	done
}

# only the entry itself: its kind, not its size or mtime (the directory's own change whenever
# something is added inside it).
exists() {
	for P do
		if [ -e "$P" ] || [ -L "$P" ]; then
			find "$P" -maxdepth 0 -printf '%p %y\n' 2>&1
		else
			printf 'absent %s\n' "$P"
		fi
	done
}

{
	tree "$H/.local/lib/eitri" "$H/.local/lib/eitri.new" "$H/.local/lib/eitri.old"
	if [ -d "$H/.local/bin" ]; then
		find "$H/.local/bin" -maxdepth 1 \
			\( -name eitri -o -name 'eitri.bak-*' -o -name '.eitri.tmp.*' \) -printf "$FMT" 2>&1
	fi
	if [ -d "$DATA/applications" ]; then
		find "$DATA/applications" -maxdepth 1 \
			\( -name cn.huntergrey.eitri.desktop -o -name eitri.desktop -o -name '.cn.huntergrey.eitri.desktop.tmp.*' \) \
			-printf "$FMT" 2>&1
	fi
	if [ -d "$DATA/icons/hicolor" ]; then
		find "$DATA/icons/hicolor" \( -name 'cn.huntergrey.eitri.*' -o -name '.cn.huntergrey.eitri.*' \) \
			-printf "$FMT" 2>&1
	fi
	tree "$DATA/licenses/eitri" "$DATA/eitri" "$CACHE/eitri"
	exists "$H/.config/eitri" "$STATE/eitri"
} | LC_ALL=C sort
