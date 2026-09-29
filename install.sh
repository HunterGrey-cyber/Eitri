#!/bin/sh
# Builds and installs neovibe from this checkout as it is on disk, uncommitted changes included:
# the development loop for working on neovibe itself. A release is installed with
# packaging/install.sh instead (the same script a release publishes as install.sh). This is a thin
# wrapper over `packaging/install.sh --from-source --checkout`, not a second implementation: a
# separate build-and-install script drifts from packaging/install.sh's checks and defaults (binary
# lists, sidecar-artifact detection, signature and checksum checks). One installer behind
# `curl | sh`, `neovibe setup` and this script keeps every route the same.
#
#   ./install.sh                          build and install this checkout
#   ./install.sh --allow-verdandi-rev-mismatch --verdandi-checkout DIR
#                                          build the sidecar from a Verdandi checkout not at the
#                                          pin (see packaging/install.sh --help)
#   ./install.sh --uninstall (or any other mode of packaging/install.sh: --nvim-only,
#                --nvim-offer, --sidecar-only, --build-sidecar-into DIR ...)
#                                          runs packaging/install.sh with that mode instead
#   ./install.sh --help                   says this, then prints packaging/install.sh's options
#
# There is no default Verdandi checkout here: a fresh clone has no such directory, and
# packaging/install.sh's --checkout path falls back to the pinned public Verdandi source when
# NEOVIBE_VERDANDI_CHECKOUT names nothing. When it is set, it is passed through explicitly (an `if`, never a
# `:-` default), so this script's behaviour never depends on which shell has it exported.
set -eu
here=$(cd "$(dirname "$0")" && pwd) || exit 1

# Every mode of packaging/install.sh other than --from-source is its own mode, and its
# set_mode refuses a second one -- forcing "--from-source --checkout $here" onto any of them died
# with "--from-source and --uninstall cannot be combined" (and the same for the others). Pass one of
# these straight through instead; the default (none of them given, or --from-source itself, which
# set_mode accepts twice) is unchanged: build and install this checkout from source.
# packaging/tests/install/test_root_wrapper.sh reads the list of modes out of packaging/install.sh
# itself, so a mode added there and not here fails a test.
# -h/--help first says what this script does with no option, since packaging/install.sh's own usage
# describes a release install there.
for _nv_a in "$@"; do
	case $_nv_a in
	-h | --help)
		cat <<'EOF'
./install.sh is for working on neovibe itself.
With no option, it builds and installs this checkout from source, as it is on disk now
(packaging/install.sh --from-source --checkout <this checkout>). NEOVIBE_VERDANDI_CHECKOUT, when
set, names a local Verdandi checkout to build the sidecar from. With one of packaging/install.sh's
other modes (--uninstall, --nvim-only, --nvim-offer, --sidecar-only, --build-sidecar-into), it runs
packaging/install.sh with that mode instead.

The options, all passed on to packaging/install.sh (its "(no option)" line describes installing a
release, which this script does not do):

EOF
		exec sh "$here/packaging/install.sh" "$@"
		;;
	--uninstall | --nvim-only | --nvim-offer | --sidecar-only | --build-sidecar-into)
		exec sh "$here/packaging/install.sh" "$@"
		;;
	esac
done
if [ -n "${NEOVIBE_VERDANDI_CHECKOUT:-}" ]; then
	exec sh "$here/packaging/install.sh" --from-source --checkout "$here" \
		--verdandi-checkout "$NEOVIBE_VERDANDI_CHECKOUT" "$@"
fi
exec sh "$here/packaging/install.sh" --from-source --checkout "$here" "$@"
