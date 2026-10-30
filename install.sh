#!/bin/sh
# Builds and installs Eitri from this checkout, including uncommitted changes, through
# `packaging/install.sh --from-source --checkout`. Releases use packaging/install.sh directly.
# Sharing one installer keeps binary lists, sidecar detection and verification checks consistent
# across this development wrapper, `eitri setup` and release installs.
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
# No local Verdandi checkout is assumed: packaging/install.sh falls back to pinned public source
# when EITRI_VERDANDI_CHECKOUT names nothing. Pass a non-empty value explicitly instead of relying
# on the child process inheriting it from the environment.
set -eu
here=$(cd "$(dirname "$0")" && pwd) || exit 1

# The installer's set_mode refuses conflicting modes, so pass other modes through without adding
# --from-source. With no mode, or --from-source itself (which set_mode accepts twice), build this
# checkout. The root-wrapper test reads the installer's mode list so an omitted mode fails it.
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
