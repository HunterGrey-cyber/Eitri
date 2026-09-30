#!/bin/sh
# Builds and installs Eitri from THIS checkout, exactly as it is on disk right now (uncommitted
# changes included) -- the development loop for working on Eitri itself; a release is installed
# with packaging/install.sh (the same script a release publishes as install.sh) instead. This is a
# thin wrapper over `packaging/install.sh --from-source --checkout`, not a
# second implementation: this repo used to carry its own separate build-and-install script here, and
# it drifted from packaging/install.sh's own checks and defaults more than once (different binary
# lists, different sidecar-artifact detection, no signature/checksum discipline at all). One
# installer that everyone -- a stranger running `curl | sh`, `eitri setup`, and this repo's own
# owner -- goes through is what keeps that from happening again.
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
# There is deliberately no default Verdandi checkout here (the old script's own default, a path
# under the original developer's home directory, is gone): a stranger cloning this repository has no
# such directory, and packaging/install.sh's own --checkout path already falls back to the pinned public
# Verdandi source when EITRI_VERDANDI_CHECKOUT names nothing -- silently trying a path that only
# exists on the owner's own machine would be the wrong default for everyone else. When it IS set,
# it is passed through explicitly (an `if`, never a `:-` default), so this script's behaviour never
# depends on which shell happens to have it exported.
set -eu
here=$(cd "$(dirname "$0")" && pwd) || exit 1

# docs-codex-3: every mode of packaging/install.sh other than --from-source is its own mode, and its
# set_mode refuses a second one -- forcing "--from-source --checkout $here" onto any of them died
# with "--from-source and --uninstall cannot be combined" (and the same for the others). Pass one of
# these straight through instead; the default (none of them given, or --from-source itself, which
# set_mode accepts twice) is unchanged: build and install this checkout from source.
# packaging/tests/install/test_root_wrapper.sh reads the list of modes out of packaging/install.sh
# itself, so a mode added there and not here fails a test (fix round 1 added --nvim-offer without it).
# -h/--help first says what this script does with no option, since packaging/install.sh's own usage
# describes a release install there.
for _nv_a in "$@"; do
	case $_nv_a in
	-h | --help)
		cat <<'EOF'
./install.sh is for working on Eitri itself.
With no option, it builds and installs this checkout from source, as it is on disk now
(packaging/install.sh --from-source --checkout <this checkout>). EITRI_VERDANDI_CHECKOUT, when
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
if [ -n "${EITRI_VERDANDI_CHECKOUT:-}" ]; then
	exec sh "$here/packaging/install.sh" --from-source --checkout "$here" \
		--verdandi-checkout "$EITRI_VERDANDI_CHECKOUT" "$@"
fi
exec sh "$here/packaging/install.sh" --from-source --checkout "$here" "$@"
