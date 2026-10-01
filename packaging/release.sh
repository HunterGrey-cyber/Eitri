#!/usr/bin/env bash
# packaging/release.sh -- build one Eitri release inside the pinned build container.
#
#   packaging/release.sh <version> --source <public clone> (--sign KEY | --unsigned)
#                        [--release-signers FILE] [--out DIR] [--rehearsal] [--verdandi-mirror DIR]
#                        [--no-leak-scan]
#
#   Run it from the private checkout: publish/scan.sh, the host leak scan, lives there and nowhere
#   else, and the run refuses without it unless --no-leak-scan says so.
#
#   <version>              X.Y.Z or X.Y.Z-rc.N; must equal the clone's [workspace.package] version
#   --source DIR           a git clone of the PUBLIC repo whose HEAD is the commit publish/commit.sh
#                          made, with the neovide submodule initialised at its recorded commit and
#                          nothing untracked, modified or ignored in either
#   --sign KEY             sign SHA256SUMS with this ssh key (required for a final version, D13).
#                          Its public half (KEY.pub, else `ssh-keygen -y`) must be listed in the
#                          signers file and must not be any ~/.ssh/id_*.pub (field 2 compared)
#   --unsigned             release candidates only, and only while packaging/release-signers holds
#                          no key (or with --rehearsal); no SHA256SUMS.sig, with a warning
#   --release-signers FILE release candidates only: check --sign against FILE instead of the
#                          clone's packaging/release-signers, with a warning. Once that file holds a
#                          key, --sign's key must be listed there too, and SHA256SUMS.sig must
#                          verify against both files
#   --out DIR              the release root (default ~/.cache/eitri-release). The assets land in
#                          DIR/v<version>/; logs in DIR/logs/, the build tree in DIR/work/ (kept),
#                          caches in DIR/{cargo,target,npm,skia,verdandi.git}
#   --rehearsal            a trial run (release candidates, --unsigned only): writes DIR/v<version>-
#                          rehearsal/, marks RELEASE with REHEARSAL=1, and may run without the nvim
#                          pin, in which case RELEASE carries no NVIM_* fields and the run says so
#   --verdandi-mirror DIR  rehearsal only: fetch the pinned Verdandi revision from this local git
#                          repository instead of its public URL (for a public commit that is not
#                          pushed yet). The asset is still `git archive` of that exact revision
#   --no-leak-scan         release candidates only, and only where publish/scan.sh is absent (a
#                          public checkout reproducing a release): skip the host leak scan. The
#                          output is marked as never scanned; never publish it
#
# Order (spec docs/superpowers/specs/2026-09-27-v1-dist-design.md sec 4.2, and the Task 12
# pre-think's sec 3): the refusals, all on the host and before anything is built; the image, from the
# clone's packaging/container/build.Dockerfile; phase F in the container with the network on (copy the
# clone to /build/src, fetch crates, npm ci, the Skia and nvim downloads, the public Verdandi); phase
# B with --network none (the build, the ABI check, the licence notices, RELEASE, the packages, the
# tarball, both source assets, every content check over the extracted assets); the proof, in a fresh
# run with --network none, an empty CARGO_HOME and an npm that fails (the source asset rebuilds
# offline, and SOURCE's own relink recipe works); then on the host publish/scan.sh, SHA256SUMS, the
# signature and its verification, chmod a-w, and the rest of the release printed, never run.
#
# Every build input comes from the clone -- the Dockerfile, pins.env, the collector, the ABI check,
# the nfpm profile, install.sh, and this script itself, which the container runs from the clone's
# copy (the run refuses when that copy differs from this file). So the scripts that controlled the
# build are exactly the ones in the source asset.
set -euo pipefail

RS_GTK_FLOOR=4.14
RS_GLIBC_FLOOR=2.39
RS_SIGN_NAMESPACE=eitri-release
RS_SIGN_IDENTITY=release@eitri
RS_PUBLIC_REPO=https://github.com/HunterGrey-cyber/eitri
RS_IMAGE_REPO=eitri-release-build
RS_BINARIES=(shell eitri-supervisor eitri-tmux-shim eitri-claude-handoff)
RS_VERSION_ERE='^[0-9]+\.[0-9]+\.[0-9]+(-rc\.[0-9]+)?$'
RS_LEGACY_MESSAGE='the legacy backend is not in this build'
# The public identity every public commit carries (publish/commit.sh commits as it, publish/tag.sh
# tags as it); public_commits_check holds the released commit to it.
RS_PUBLIC_NAME="Hunter Grey"
RS_PUBLIC_EMAIL="71165939+HunterGrey-cyber@users.noreply.github.com"

die() { echo "release.sh: $*" >&2; exit 1; }
say() { echo "release.sh: $*" >&2; }
warn() { echo "release.sh: WARNING: $*" >&2; }
step() { echo "release.sh: [$(date -u +%H:%M:%S)] $*" >&2; }

# kv_get FILE KEY: the value of the first KEY=value line, or nothing. pins.env and RELEASE are read
# this way, never sourced (spec sec 4.1).
kv_get() {
	local line
	while IFS= read -r line || [ -n "$line" ]; do
		case "$line" in
			"$2="*) printf '%s\n' "${line#"$2="}"; return 0 ;;
		esac
	done < "$1"
	return 0
}

# workspace_version CARGO_TOML: `version` in its [workspace.package] table.
workspace_version() {
	awk '
		/^\[/ { in_pkg = ($0 == "[workspace.package]") ; next }
		in_pkg && /^version[[:space:]]*=/ {
			v = $0; sub(/^version[[:space:]]*=[[:space:]]*"/, "", v); sub(/".*$/, "", v); print v; exit
		}' "$1"
}

# verdandi_dep AGENT_CARGO_TOML: sets RS_VERDANDI_URL and RS_VERDANDI_REV from the
# claude-runtime-protocol dependency line.
verdandi_dep() {
	local line
	line="$(grep -E '^claude-runtime-protocol = \{ git = "[^"]+", rev = "[^"]+" \}' "$1" || true)"
	RS_VERDANDI_URL="$(printf '%s\n' "$line" | sed -nE 's/.*git = "([^"]+)".*/\1/p')"
	RS_VERDANDI_REV="$(printf '%s\n' "$line" | sed -nE 's/.*rev = "([^"]+)".*/\1/p')"
}

# legacy_default_offenders DIR: every tracked Cargo.toml whose [features] default enables anything
# named legacy-backend (M15: a default would put the legacy backend into a release build with no
# --features anywhere). The container re-checks the resolved graph with `cargo tree -e features`.
legacy_default_offenders() {
	git -C "$1" ls-files -z -- 'Cargo.toml' '*/Cargo.toml' | python3 -c '
import sys, tomllib
root = sys.argv[1]
for rel in sys.stdin.read().split("\0"):
    if not rel:
        continue
    with open(root + "/" + rel, "rb") as f:
        data = tomllib.load(f)
    default = data.get("features", {}).get("default", [])
    if any("legacy-backend" in item for item in default):
        print(rel)
' "$1"
}

# key_blob TEXT: field 2 of an OpenSSH public key line (the base64 key itself, never its comment).
key_blob() { printf '%s\n' "$1" | awk 'NF >= 2 { print $2; exit }'; }

# signers_list_blob FILE BLOB: does any non-comment line of an allowed_signers FILE carry BLOB?
signers_list_blob() {
	awk -v blob="$2" '
		/^[[:space:]]*(#|$)/ { next }
		{ for (i = 1; i <= NF; i++) if ($i == blob) found = 1 }
		END { exit(found ? 0 : 1) }' "$1"
}

# signers_has_key FILE: does FILE exist and hold a key line (anything but blank and comment lines)?
# The same test packaging/install.sh's prepare_signers makes of its embedded block.
signers_has_key() { [ -f "$1" ] && grep -Eq '^[[:space:]]*[^#[:space:]]' "$1"; }

# sign_sums KEY DIR: DIR/SHA256SUMS.sig, ssh-keygen's own signature format (spec sec 4.2 step 10).
# A passphrase-protected KEY is asked for on the controlling terminal (ssh-keygen reads /dev/tty, so
# piping this script's output through tee is fine), and ssh-keygen asks only once: a mistyped
# passphrase would end the run here, after the whole build, with SHA256SUMS already written -- which
# prepare_dirs then refuses to rebuild in place. So a failed signature is tried up to three times.
sign_sums() {
	local attempt=1
	rm -f -- "$2/SHA256SUMS.sig"
	until ssh-keygen -Y sign -q -f "$1" -n "$RS_SIGN_NAMESPACE" "$2/SHA256SUMS"; do
		[ "$attempt" -lt 3 ] || die "ssh-keygen could not sign $2/SHA256SUMS with $1 after 3 attempts (a passphrase-protected key needs a terminal on stdin, or the key loaded into ssh-agent with ssh-add)"
		warn "ssh-keygen could not sign $2/SHA256SUMS with $1: trying again (attempt $((attempt + 1)) of 3)"
		attempt=$((attempt + 1))
	done
	[ -s "$2/SHA256SUMS.sig" ] || die "ssh-keygen wrote no $2/SHA256SUMS.sig"
}

# verify_sums SIGNERS DIR: the exact check packaging/install.sh runs (spec sec 6.4).
verify_sums() {
	ssh-keygen -Y verify -f "$1" -I "$RS_SIGN_IDENTITY" -n "$RS_SIGN_NAMESPACE" \
		-s "$2/SHA256SUMS.sig" < "$2/SHA256SUMS"
}

# verify_release_signature DIR: after sign_sums. DIR/SHA256SUMS.sig must verify against RS_SIGNERS,
# the file --sign's key was checked against, and -- whenever RS_SOURCE's packaging/release-signers
# holds a key -- against that file too: preflight held it byte-equal to install.sh's embedded block,
# so that second check is exactly the one this build's shipped installer runs. preflight matches a
# --release-signers override to the tracked file by key blob alone (signers_list_blob), so an
# override listing the embedded key under the right principal and namespace, while the tracked file
# lists it under another, passes the first check and only the second catches it.
verify_release_signature() {
	local tracked="$RS_SOURCE/packaging/release-signers"
	verify_sums "$RS_SIGNERS" "$1" || die "SHA256SUMS.sig does not verify against $RS_SIGNERS"
	if signers_has_key "$tracked"; then
		verify_sums "$tracked" "$1" \
			|| die "SHA256SUMS.sig verifies against $RS_SIGNERS but not against $tracked, which this build's install.sh embeds: its own installer would refuse this release"
	fi
}

# write_sums DIR NAME...: DIR/SHA256SUMS as plain two-space sha256sum lines, in the order given.
write_sums() {
	local dir="$1"; shift
	(cd "$dir" && sha256sum -- "$@") > "$dir/SHA256SUMS.tmp"
	if grep -Evxq '[0-9a-f]{64}  [A-Za-z0-9._+-]+' "$dir/SHA256SUMS.tmp"; then
		die "SHA256SUMS has a line that is not '<sha256>  <name>'"
	fi
	mv -- "$dir/SHA256SUMS.tmp" "$dir/SHA256SUMS"
}

# write_release FILE: RELEASE (spec sec 4.3) from the RS_* values. The nvim pair is written only
# when known; a rehearsal carries REHEARSAL=1.
write_release() {
	{
		echo "EITRI_VERSION=$RS_VERSION"
		echo "EITRI_COMMIT=$RS_COMMIT"
		echo "NEOVIDE_FORK_COMMIT=$RS_FORK_COMMIT"
		echo "VERDANDI_REV=$RS_VERDANDI_REV"
		echo "VERDANDI_SOURCE=verdandi-${RS_VERDANDI_REV:0:7}-source.tar.gz"
		echo "VERDANDI_SOURCE_SHA256=$RS_VERDANDI_SOURCE_SHA256"
		echo "NODE_VERSION=$RS_NODE_VERSION"
		echo "NODE_SHA256_linux_x64=$RS_NODE_SHA256_X64"
		echo "NODE_SHA256_linux_arm64=$RS_NODE_SHA256_ARM64"
		if [ -n "$RS_NVIM_VERSION" ]; then
			echo "NVIM_VERSION=$RS_NVIM_VERSION"
			echo "NVIM_SHA256_linux_x86_64=$RS_NVIM_SHA256"
		fi
		echo "SKIA_BINARIES_ARCHIVE=$RS_SKIA_ARCHIVE"
		echo "SKIA_BINARIES_SHA256=$RS_SKIA_SHA256"
		echo "GTK_FLOOR=$RS_GTK_FLOOR"
		echo "BUILD_IMAGE=$RS_BUILD_IMAGE"
		if [ "$RS_REHEARSAL" = 1 ]; then echo "REHEARSAL=1"; fi
	} > "$1"
}

# read_pins PINS_ENV: the RS_* values pins.env holds.
read_pins() {
	RS_NODE_VERSION="$(kv_get "$1" NODE_VERSION)"
	RS_NODE_SHA256_X64="$(kv_get "$1" NODE_SHA256_linux_x64)"
	RS_NODE_SHA256_ARM64="$(kv_get "$1" NODE_SHA256_linux_arm64)"
	RS_NVIM_VERSION="$(kv_get "$1" NVIM_VERSION)"
	RS_NVIM_SHA256="$(kv_get "$1" NVIM_SHA256_linux_x86_64)"
	RS_SKIA_KEY="$(kv_get "$1" SKIA_BINARIES_KEY)"
	RS_SKIA_URL_UPSTREAM="$(kv_get "$1" SKIA_BINARIES_URL_UPSTREAM)"
	RS_SKIA_SHA256="$(kv_get "$1" SKIA_BINARIES_SHA256)"
	RS_SKIA_ARCHIVE="${RS_SKIA_URL_UPSTREAM##*/}"
	RS_NFPM_VERSION="$(kv_get "$1" NFPM_VERSION)"
	RS_NFPM_SHA256="$(kv_get "$1" NFPM_SHA256_linux_x86_64)"
	RS_RUSTUP_INIT_VERSION="$(kv_get "$1" RUSTUP_INIT_VERSION)"
	RS_RUSTUP_INIT_SHA256="$(kv_get "$1" RUSTUP_INIT_SHA256_linux_x86_64)"
}

# verdandi_fetch_public GITDIR URL REV: fetch URL's branches into GITDIR's refs/public/ (created
# bare if missing), pruning every branch URL no longer has, then require REV to be reachable from
# one of them. Returns 0 when it is, 1 when it is not, 2 when the fetch fails. GITDIR is a cache kept
# across runs, and a rehearsal's --verdandi-mirror fetches into it too, so a commit object merely
# being present proves nothing: it may be one only a mirror ever had (Task 4 review). After a pruning,
# forced fetch refs/public/ is exactly URL's branches, whatever an earlier run fetched.
verdandi_fetch_public() {
	[ -e "$1/HEAD" ] || git init -q --bare "$1" || return 2
	git -C "$1" fetch -q --no-tags --prune --force "$2" '+refs/heads/*:refs/public/*' || return 2
	[ -n "$(git -C "$1" for-each-ref --contains "$3" refs/public/ 2>/dev/null)" ] || return 1
}

source_asset_url() {
	printf '%s\n' "$RS_PUBLIC_REPO/releases/download/v$1/eitri-$1-source.tar.gz"
}

# public_commits_check CLONE: F7 (whole-branch review). CLONE's HEAD is the commit this run releases,
# and the `git push origin HEAD:main` host_main prints publishes it with every commit behind it the
# public repo lacks (a `git push origin main` typed by habit publishes main's). HEAD must be authored
# and committed by RS_PUBLIC_NAME/EMAIL; HEAD's whole raw object (identities, headers, message) must
# pass RS_SCAN --message, even when the public repo already has it, and so must every commit
# reachable from HEAD or main and from no branch the public repo has -- the same checks
# publish/tag.sh makes before it tags. The public repo's branches are read from it (git ls-remote
# RS_PUBLIC_REPO.git), never from CLONE's own refs/remotes/origin/*: in a clone of a local clone those
# already hold the commits the public repo lacks, and the scan then read nothing (fix round 1). A
# public tip CLONE lacks excludes nothing, so the scan reads more, never less. EITRI_PUBLIC_REPO_URL
# reads the branches from another URL instead, for tests, with a warning.
#
# A replacement object (git replace, under refs/replace/ or wherever GIT_REPLACE_REF_BASE points)
# changes what git log, rev-list and cat-file read, never what a push sends: a commit "fixed" with
# `git replace --edit` was checked as its stand-in and pushed as the original (fix round 2). So every
# read here ignores replacements, and a CLONE with refs/replace/ refs is refused outright -- phase F
# copies them into the container, whose git archive of HEAD would read them too.
public_commits_check() {
	local -x GIT_NO_REPLACE_OBJECTS=1
	local ident dump url refs sha ref head new count replace_refs tips="" base=() published=(HEAD)
	replace_refs="$(git -C "$1" for-each-ref --count=1 refs/replace/)" || die "cannot list $1's refs"
	[ -z "$replace_refs" ] \
		|| die "$1 has replacement refs (refs/replace/, from git replace): they change what git log, this scan and the build read, never what a push sends, so 'git replace --edit' fixes nothing a push publishes -- delete them (git -C $1 replace -d <sha>) and rewrite the commit instead"
	ident="$(git -C "$1" log -1 --format='%an <%ae>|%cn <%ce>' HEAD)" || die "cannot read $1's HEAD commit"
	[ "$ident" = "$RS_PUBLIC_NAME <$RS_PUBLIC_EMAIL>|$RS_PUBLIC_NAME <$RS_PUBLIC_EMAIL>" ] \
		|| die "$1's HEAD is authored|committed as '$ident', not the public identity $RS_PUBLIC_NAME <$RS_PUBLIC_EMAIL> publish/commit.sh commits with: a release publishes that commit as it is"
	url="${EITRI_PUBLIC_REPO_URL:-$RS_PUBLIC_REPO.git}"
	[ "$url" = "$RS_PUBLIC_REPO.git" ] || warn "reading the public repo's branches from $url (EITRI_PUBLIC_REPO_URL, for tests), not $RS_PUBLIC_REPO.git"
	refs="$(GIT_TERMINAL_PROMPT=0 git -C "$1" ls-remote "$url")" \
		|| die "cannot read the branches of $url: the commits new to the public repo, which are the ones scanned, are read from it, never from $1's own refs/remotes/origin/*"
	while IFS=$'\t' read -r sha ref; do
		case "$ref" in refs/heads/*) ;; *) continue ;; esac
		tips="$tips ${ref#refs/heads/} at ${sha:0:12}"
		if git -C "$1" cat-file -e "$sha^{commit}" 2>/dev/null; then
			base+=("$sha")
		else
			warn "$url's ${ref#refs/heads/} is at $sha, which $1 does not have (git -C $1 fetch origin): the scan also reads the commits it would have excluded"
		fi
	done <<< "$refs"
	if git -C "$1" rev-parse -q --verify refs/heads/main >/dev/null; then published+=(refs/heads/main); fi
	head="$(git -C "$1" rev-parse HEAD)" || die "cannot read $1's HEAD commit"
	new="$(git -C "$1" rev-list "${published[@]}" --not ${base[@]+"${base[@]}"})" \
		|| die "cannot list the commits $1 would publish"
	count=0
	[ -z "$new" ] || count="$(printf '%s\n' "$new" | wc -l)"
	say "$url has${tips:- no branches}; $count commit(s) reachable from HEAD or main are new to it -- scanning those and HEAD"
	mkdir -p -- "${XDG_CACHE_HOME:-$HOME/.cache}"
	dump="$(mktemp "${XDG_CACHE_HOME:-$HOME/.cache}/eitri-release-commits.XXXXXX")"
	if ! { printf '%s\n' "$head"; [ -z "$new" ] || printf '%s\n' "$new"; } | awk '!seen[$0]++' \
		| git -C "$1" cat-file --batch > "$dump"; then
		rm -f -- "$dump"
		die "cannot read the commits $1 would publish"
	fi
	"$RS_SCAN" --message "$dump" \
		|| die "a commit 'git push origin HEAD:main' would publish from $1 does not pass publish/scan.sh (the line numbers above are in $dump, kept)"
	rm -f -- "$dump"
}

# =================================================================================================
# The host side: refusals, then the three container runs, then sums and signature.
# =================================================================================================

usage() { sed -n '2,36p' "$0" >&2; exit 2; }

parse_args() {
	RS_VERSION=""; RS_SOURCE=""; RS_SIGN_KEY=""; RS_UNSIGNED=0; RS_SIGNERS_OVERRIDE=""
	RS_OUT=""; RS_REHEARSAL=0; RS_VERDANDI_MIRROR=""; RS_NO_LEAK_SCAN=0
	while [ $# -gt 0 ]; do
		case "$1" in
			--source) RS_SOURCE="${2:?--source needs a directory}"; shift 2 ;;
			--sign) RS_SIGN_KEY="${2:?--sign needs a key file}"; shift 2 ;;
			--unsigned) RS_UNSIGNED=1; shift ;;
			--release-signers) RS_SIGNERS_OVERRIDE="${2:?--release-signers needs a file}"; shift 2 ;;
			--out) RS_OUT="${2:?--out needs a directory}"; shift 2 ;;
			--rehearsal) RS_REHEARSAL=1; shift ;;
			--verdandi-mirror) RS_VERDANDI_MIRROR="${2:?--verdandi-mirror needs a directory}"; shift 2 ;;
			--no-leak-scan) RS_NO_LEAK_SCAN=1; shift ;;
			-h|--help) usage ;;
			-*) die "unknown option: $1 (see --help)" ;;
			*) [ -z "$RS_VERSION" ] || die "one version only (got $RS_VERSION and $1)"; RS_VERSION="$1"; shift ;;
		esac
	done
}

# preflight: every refusal, before anything is built or fetched. The order is only which one a
# broken input meets first; each has its own message.
preflight() {
	local script_path rc_version signers pub blob idpub installed signers_file install_sh_path has_key
	[ "$(id -u)" != 0 ] || die "refusing to run as root: only the image build is root; run this as your own user"

	[ -n "$RS_VERSION" ] || die "no version given (see --help)"
	[[ "$RS_VERSION" =~ $RS_VERSION_ERE ]] || die "$RS_VERSION is not X.Y.Z or X.Y.Z-rc.N"
	rc_version=0; case "$RS_VERSION" in *-rc.*) rc_version=1 ;; esac

	if [ -n "$RS_SIGN_KEY" ] && [ "$RS_UNSIGNED" = 1 ]; then die "give --sign KEY or --unsigned, not both"; fi
	if [ "$rc_version" = 0 ]; then
		[ "$RS_UNSIGNED" = 0 ] || die "--unsigned is for release candidates only: $RS_VERSION is a final version, which must be signed (D13)"
		[ -z "$RS_SIGNERS_OVERRIDE" ] || die "--release-signers is for release candidates only: $RS_VERSION is checked against the clone's packaging/release-signers"
		[ -n "$RS_SIGN_KEY" ] || die "$RS_VERSION is a final version and must be signed: give --sign KEY (D13)"
		[ "$RS_REHEARSAL" = 0 ] || die "--rehearsal is for release candidates only"
		[ "$RS_NO_LEAK_SCAN" = 0 ] || die "--no-leak-scan is for release candidates only: a final version is built from the private checkout, and its publish/scan.sh runs"
	else
		[ -n "$RS_SIGN_KEY" ] || [ "$RS_UNSIGNED" = 1 ] || die "give --sign KEY, or --unsigned for this release candidate"
	fi
	if [ "$RS_REHEARSAL" = 1 ] && [ "$RS_UNSIGNED" = 0 ]; then die "--rehearsal runs --unsigned only"; fi
	if [ -n "$RS_VERDANDI_MIRROR" ] && [ "$RS_REHEARSAL" = 0 ]; then die "--verdandi-mirror is for --rehearsal only"; fi

	[ -n "$RS_SOURCE" ] || die "--source <public clone> is required"
	[ -d "$RS_SOURCE" ] || die "--source $RS_SOURCE is not a directory"
	RS_SOURCE="$(realpath "$RS_SOURCE")"
	[ -e "$RS_SOURCE/.git" ] || die "--source $RS_SOURCE has no .git: a bare export tree is not accepted, only a clone of the public repo (spec sec 4.1)"

	script_path="$(realpath "${BASH_SOURCE[0]}")"
	[ -f "$RS_SOURCE/packaging/release.sh" ] || die "$RS_SOURCE has no packaging/release.sh"
	cmp -s "$script_path" "$RS_SOURCE/packaging/release.sh" \
		|| die "this release.sh differs from $RS_SOURCE/packaging/release.sh: run the copy the clone carries, so the script that builds the release is the one its source asset contains"
	# The host leak scan (spec sec 4.2 step 9) lives in the private checkout's publish/, which the
	# public tree leaves out. The clone's own copy of this file passes the cmp above, so running it is
	# easy to do by mistake; without the scanner the run stops rather than skipping the scan quietly.
	RS_SCAN="$(dirname "$script_path")/../publish/scan.sh"
	if [ -x "$RS_SCAN" ]; then
		[ "$RS_NO_LEAK_SCAN" = 0 ] || die "--no-leak-scan, but $RS_SCAN is here: where the scanner exists it always runs"
	elif [ "$RS_NO_LEAK_SCAN" = 0 ]; then
		die "publish/scan.sh is not beside this release.sh ($RS_SCAN): run the private checkout's packaging/release.sh, which has the host leak scan, or pass --no-leak-scan to reproduce a release candidate without it"
	else
		warn "--no-leak-scan: publish/scan.sh is not here, so nothing will scan these assets for private identifiers; never publish them"
	fi

	case "$(git -C "$RS_SOURCE" submodule status -- neovide 2>/dev/null || true)" in
		' '*) ;;
		'-'*) die "the neovide submodule in $RS_SOURCE is not initialised: run 'git submodule update --init neovide' there (after publish/commit.sh)" ;;
		'+'*) die "the neovide submodule in $RS_SOURCE is not at the commit HEAD records: run 'git submodule update --init neovide' there" ;;
		*) die "$RS_SOURCE has no usable neovide submodule ('git submodule status neovide' says nothing sensible)" ;;
	esac
	[ "$(git -C "$RS_SOURCE/neovide" rev-parse HEAD)" = "$(git -C "$RS_SOURCE" ls-tree HEAD neovide | awk '{ print $3 }')" ] \
		|| die "neovide's HEAD is not the commit $RS_SOURCE's HEAD records for it"
	# --untracked-files=all and --ignore-submodules=none override any status.showUntrackedFiles or
	# submodule ignore setting, in the clone's config or a global one, that would hide a stray file.
	[ -z "$(git -C "$RS_SOURCE" status --porcelain --ignored --untracked-files=all --ignore-submodules=none)" ] \
		|| die "$RS_SOURCE is not clean (git status --porcelain --ignored lists something): a stray file, even an ignored one such as a built dist/, would change what is built"
	[ -z "$(git -C "$RS_SOURCE/neovide" status --porcelain --ignored --untracked-files=all --ignore-submodules=none)" ] \
		|| die "$RS_SOURCE/neovide is not clean (git status --porcelain --ignored lists something)"

	[ ! -e "$RS_SOURCE/.cargo" ] || die "$RS_SOURCE has a .cargo/: cargo configuration could change the build behind this script's back (M15)"
	if grep -nE 'ssh://|git@' "$RS_SOURCE/Cargo.lock" >&2; then die "$RS_SOURCE/Cargo.lock names an ssh source"; fi
	if git -C "$RS_SOURCE" grep -nE 'ssh://|git@' -- 'Cargo.toml' '*/Cargo.toml' >&2; then
		die "a Cargo.toml in $RS_SOURCE names an ssh source"
	fi
	if git -C "$RS_SOURCE/neovide" grep -nE 'ssh://|git@' -- 'Cargo.toml' '*/Cargo.toml' >&2; then
		die "a Cargo.toml in $RS_SOURCE/neovide names an ssh source"
	fi

	[ "$(workspace_version "$RS_SOURCE/Cargo.toml")" = "$RS_VERSION" ] \
		|| die "$RS_VERSION disagrees with $RS_SOURCE's [workspace.package] version ($(workspace_version "$RS_SOURCE/Cargo.toml"))"
	local offenders
	offenders="$(legacy_default_offenders "$RS_SOURCE")" || die "could not read $RS_SOURCE's Cargo.toml files"
	[ -z "$offenders" ] || die "a [features] default enables legacy-backend, which no release may contain (D16): $offenders"

	[ -f "$RS_SOURCE/packaging/pins.env" ] || die "$RS_SOURCE has no packaging/pins.env"
	read_pins "$RS_SOURCE/packaging/pins.env"
	[[ "$RS_SKIA_SHA256" =~ ^[0-9a-f]{64}$ ]] && [[ "$RS_SKIA_ARCHIVE" =~ ^skia-binaries-[A-Za-z0-9._-]+\.tar\.gz$ ]] \
		&& [ "$RS_SKIA_ARCHIVE" = "skia-binaries-$RS_SKIA_KEY.tar.gz" ] \
		|| die "pins.env has no complete Skia pin (SKIA_BINARIES_KEY, SKIA_BINARIES_URL_UPSTREAM, SKIA_BINARIES_SHA256), which every release needs (D13, spec sec 4.2 step 2)"
	[[ "$RS_NODE_VERSION" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] && [[ "$RS_NODE_SHA256_X64" =~ ^[0-9a-f]{64}$ ]] \
		&& [[ "$RS_NODE_SHA256_ARM64" =~ ^[0-9a-f]{64}$ ]] \
		|| die "pins.env has no complete Node pin (NODE_VERSION, NODE_SHA256_linux_x64, NODE_SHA256_linux_arm64)"
	[[ "$RS_NFPM_VERSION" =~ ^[0-9.]+$ ]] && [[ "$RS_NFPM_SHA256" =~ ^[0-9a-f]{64}$ ]] \
		&& [[ "$RS_RUSTUP_INIT_VERSION" =~ ^[0-9.]+$ ]] && [[ "$RS_RUSTUP_INIT_SHA256" =~ ^[0-9a-f]{64}$ ]] \
		|| die "pins.env has no complete nfpm or rustup-init pin, which the build image needs"
	if [ -n "$RS_NVIM_VERSION$RS_NVIM_SHA256" ]; then
		[[ "$RS_NVIM_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] && [[ "$RS_NVIM_SHA256" =~ ^[0-9a-f]{64}$ ]] \
			|| die "pins.env's nvim pin is incomplete or malformed (NVIM_VERSION X.Y.Z, NVIM_SHA256_linux_x86_64)"
	elif [ "$RS_REHEARSAL" = 1 ]; then
		warn "pins.env has no nvim pin yet (plan Task 11): this rehearsal's RELEASE carries no NVIM_* fields"
	else
		die "pins.env has no nvim pin (NVIM_VERSION, NVIM_SHA256_linux_x86_64); only a --rehearsal may run without it"
	fi

	verdandi_dep "$RS_SOURCE/agent/Cargo.toml"
	[[ "$RS_VERDANDI_REV" =~ ^[0-9a-f]{40}$ ]] \
		|| die "agent/Cargo.toml's claude-runtime-protocol rev '$RS_VERDANDI_REV' is not a full 40-hex public Verdandi commit"
	[[ "$RS_VERDANDI_URL" == https://* ]] || die "agent/Cargo.toml's claude-runtime-protocol source is not an https URL: $RS_VERDANDI_URL"
	grep -qE "^pub const EXPECTED_VERDANDI_REVISION: &str = \"${RS_VERDANDI_REV:0:7}\";" \
		"$RS_SOURCE/agent/src/providers/claude_sidecar/spawn.rs" \
		|| die "EXPECTED_VERDANDI_REVISION is not ${RS_VERDANDI_REV:0:7}, the 7-char prefix of the pinned Verdandi rev"
	if [ -n "$RS_VERDANDI_MIRROR" ]; then
		[ -d "$RS_VERDANDI_MIRROR" ] || die "--verdandi-mirror $RS_VERDANDI_MIRROR is not a directory"
		RS_VERDANDI_MIRROR="$(realpath "$RS_VERDANDI_MIRROR")"
		git -C "$RS_VERDANDI_MIRROR" cat-file -e "$RS_VERDANDI_REV^{commit}" 2>/dev/null \
			|| die "--verdandi-mirror $RS_VERDANDI_MIRROR does not have $RS_VERDANDI_REV"
	fi

	# installer-claude-2 (Task 1, spec sec 4.4/6.4): packaging/install.sh's embedded
	# release-signers block must never drift from the tracked packaging/release-signers, for
	# every version -- an rc's embedded block may stay empty (it signs with a throwaway
	# --release-signers file instead), as long as the tracked file agrees with what install.sh
	# actually ships. What this check closes is the drift itself: a hand-kept copy of the block
	# that silently stops matching the tracked file, so an installer built from this tree could
	# ship a stale or foreign key while packaging/release-signers looks fine. It is not what stops
	# a final version from building with no key: signers_list_blob (below, for --sign's own key)
	# stopped that before Task 1 too, since no key is listed in a file with no key line; the
	# final-only check just below, added with this one, stops it first and says so by name.
	signers_file="$RS_SOURCE/packaging/release-signers"
	install_sh_path="$RS_SOURCE/packaging/install.sh"
	[ -f "$signers_file" ] || die "$RS_SOURCE has no packaging/release-signers"
	[ -f "$install_sh_path" ] || die "$RS_SOURCE has no packaging/install.sh"
	python3 "$RS_SOURCE/packaging/release_check.py" check-release-signers "$install_sh_path" "$signers_file"

	# Task 5 (v1-dist plan lane D, docs/superpowers/plans/2026-09-28-v1-dist-task17-18.md): every
	# public guide's Chinese twin (README.zh-CN.md, INSTALL.zh-CN.md, and any other X.zh-CN.md the
	# clone carries) must still match the English original it was translated from -- release_check's
	# `twins` check, over $RS_SOURCE's own root, where publish/export.sh puts the guides.
	python3 "$RS_SOURCE/packaging/release_check.py" twins "$RS_SOURCE"

	has_key=0
	signers_has_key "$signers_file" && has_key=1
	if [ "$rc_version" = 0 ]; then
		[ "$has_key" = 1 ] \
			|| die "$RS_VERSION is a final version, but packaging/release-signers holds no key line: the owner adds the dedicated release key before the first final release (spec sec 4.4)"
	elif [ "$has_key" = 1 ]; then
		# installer-claude-10 / docs-codex-6 (fix round 1): once install.sh embeds a real key, an
		# rc built here must actually be verifiable against it -- its installer refuses a release
		# with no SHA256SUMS.sig, and fill_notes' "install.sh verifies it itself, with the release
		# key built into it" would otherwise describe a signature that does not exist.
		# --rehearsal is exempt (it always runs --unsigned, line above, and is never published);
		# fill_notes gives it its own text -- no signature, and this build's installer refuses it.
		if [ "$RS_UNSIGNED" = 1 ] && [ "$RS_REHEARSAL" = 0 ]; then
			die "packaging/release-signers already holds a release key: $RS_VERSION must be signed with --sign (a throwaway --release-signers build is only for before the owner embeds the key); pass --rehearsal for a trial that is never published"
		fi
	fi

	if [ -n "$RS_SIGN_KEY" ]; then
		[ -f "$RS_SIGN_KEY" ] || die "--sign $RS_SIGN_KEY: no such file"
		RS_SIGN_KEY="$(realpath "$RS_SIGN_KEY")"
		if [ -f "$RS_SIGN_KEY.pub" ]; then
			pub="$(cat -- "$RS_SIGN_KEY.pub")"
		else
			pub="$(ssh-keygen -y -f "$RS_SIGN_KEY")" || die "ssh-keygen cannot read the public half of $RS_SIGN_KEY"
		fi
		blob="$(key_blob "$pub")"
		[ -n "$blob" ] || die "cannot read a public key out of $RS_SIGN_KEY"
		signers="${RS_SIGNERS_OVERRIDE:-$RS_SOURCE/packaging/release-signers}"
		[ -f "$signers" ] || die "signers file $signers: no such file"
		signers_list_blob "$signers" "$blob" \
			|| die "the public half of $RS_SIGN_KEY is not listed in $signers: a signature by it would not verify for anyone"
		# installer-claude-10 / docs-codex-6 (fix round 1): a --release-signers override lets an rc
		# sign with a throwaway key while the embedded block is still empty (has_key=0, above) --
		# but once install.sh embeds a real key, a signature by a key that key does not list would
		# not verify for anyone running this build's own installer, whatever the override file says.
		# This compares the key blob only; verify_release_signature, after signing, also verifies
		# against packaging/release-signers itself, which settles its principal and namespace.
		if [ "$has_key" = 1 ] && [ -n "$RS_SIGNERS_OVERRIDE" ] && ! signers_list_blob "$signers_file" "$blob"; then
			die "packaging/release-signers already holds a release key, but --sign's key is not listed there (only in $RS_SIGNERS_OVERRIDE): an installer built from this tree embeds packaging/release-signers and could not verify a signature made with this key"
		fi
		for idpub in "$HOME"/.ssh/id_*.pub; do
			[ -f "$idpub" ] || continue
			if [ "$(key_blob "$(cat -- "$idpub")")" = "$blob" ]; then
				die "$RS_SIGN_KEY is $idpub's key: an everyday login key must never become the release key (spec sec 4.4)"
			fi
		done
		RS_SIGNERS="$(realpath "$signers")"
	fi
	if [ -n "$RS_SIGNERS_OVERRIDE" ]; then
		[ -f "$RS_SIGNERS_OVERRIDE" ] || die "--release-signers $RS_SIGNERS_OVERRIDE: no such file"
		warn "checking the signature against $RS_SIGNERS_OVERRIDE (--release-signers), not the clone's packaging/release-signers"
	fi
	if [ "$RS_UNSIGNED" = 1 ]; then
		warn "$RS_VERSION will be built with no SHA256SUMS.sig (--unsigned): its checksums will only detect corruption"
	fi

	command -v docker >/dev/null 2>&1 || die "docker is not on PATH"
	installed="$(git -C "$RS_SOURCE" rev-parse HEAD)"
	RS_COMMIT="$installed"
	RS_FORK_COMMIT="$(git -C "$RS_SOURCE/neovide" rev-parse HEAD)"
	# A leak check like host_scans, so it runs wherever the scanner does: --no-leak-scan (refused for a
	# final version, and wherever publish/scan.sh exists) skips it with them.
	if [ "$RS_NO_LEAK_SCAN" = 0 ]; then
		public_commits_check "$RS_SOURCE"
	else
		warn "--no-leak-scan: $RS_SOURCE's HEAD and the commits a push would publish are not checked for a private identity or path"
	fi
}

# prepare_dirs: the release root and everything under it (M11). A finished release is never
# overwritten; a partial one (no SHA256SUMS) is cleared and rebuilt.
prepare_dirs() {
	local root="${RS_OUT:-$HOME/.cache/eitri-release}"
	mkdir -p -- "$root"
	RS_ROOT="$(realpath "$root")"
	case "$RS_ROOT" in /tmp|/tmp/*) die "--out must not be under /tmp (a small shared tmpfs)" ;; esac
	RS_TAG="v$RS_VERSION"
	[ "$RS_REHEARSAL" = 0 ] || RS_TAG="v$RS_VERSION-rehearsal"
	RS_REL="$RS_ROOT/$RS_TAG"
	RS_LOGS="$RS_ROOT/logs/$RS_TAG"
	RS_WORK="$RS_ROOT/work/$RS_TAG"
	RS_PROOF="$RS_ROOT/proof/$RS_TAG"
	if [ -e "$RS_REL/SHA256SUMS" ]; then
		die "$RS_REL already holds a finished release (it has SHA256SUMS): move it away first; a release is never rebuilt in place"
	fi
	if [ -e "$RS_REL" ]; then chmod -R u+w -- "$RS_REL"; rm -rf -- "$RS_REL"; fi
	rm -rf -- "$RS_WORK" "$RS_PROOF" "$RS_LOGS"
	mkdir -p -- "$RS_REL" "$RS_LOGS" "$RS_WORK/src" "$RS_WORK/stage" "$RS_WORK/check" "$RS_PROOF" \
		"$RS_ROOT/cargo" "$RS_ROOT/target" "$RS_ROOT/npm" "$RS_ROOT/skia" "$RS_ROOT/verdandi.git"
}

RS_CONTAINERS=()
cleanup_containers() {
	local name
	for name in "${RS_CONTAINERS[@]}"; do
		docker rm -f "$name" >/dev/null 2>&1 || true
	done
}

# ctr_run NAME LOG DOCKER_ARGS... -- STEP: one container run of this script's STEP, as the host
# user, with the script mounted read-only from the clone, its output kept in LOG.
ctr_run() {
	local name="$1" log="$2"; shift 2
	local args=()
	while [ "$1" != -- ]; do args+=("$1"); shift; done
	shift
	RS_CONTAINERS+=("$name")
	docker run --rm --init --name "$name" \
		--user "$(id -u):$(id -g)" -e HOME=/home/builder -e CARGO_BUILD_JOBS=12 \
		--mount "type=bind,src=$RS_SOURCE/packaging/release.sh,dst=/release.sh,readonly" \
		-e "RS_VERSION=$RS_VERSION" -e "RS_REHEARSAL=$RS_REHEARSAL" -e "RS_COMMIT=$RS_COMMIT" \
		"${args[@]}" "$RS_IMAGE" bash /release.sh __container "$@" 2>&1 | tee "$log"
}

build_image() {
	RS_IMAGE="$RS_IMAGE_REPO:$RS_VERSION"
	step "building $RS_IMAGE from $RS_SOURCE/packaging/container/build.Dockerfile (never --pull)"
	docker build \
		--build-arg "UID=$(id -u)" --build-arg "GID=$(id -g)" \
		--build-arg "RUSTUP_INIT_VERSION=$RS_RUSTUP_INIT_VERSION" \
		--build-arg "RUSTUP_INIT_SHA256_LINUX_X86_64=$RS_RUSTUP_INIT_SHA256" \
		--build-arg "NODE_VERSION=$RS_NODE_VERSION" --build-arg "NODE_SHA256_LINUX_X64=$RS_NODE_SHA256_X64" \
		--build-arg "NFPM_VERSION=$RS_NFPM_VERSION" --build-arg "NFPM_SHA256_LINUX_X86_64=$RS_NFPM_SHA256" \
		-t "$RS_IMAGE" -f "$RS_SOURCE/packaging/container/build.Dockerfile" "$RS_SOURCE/packaging/container" \
		2>&1 | tee "$RS_LOGS/image.log"
	RS_BUILD_IMAGE="$(docker image inspect -f '{{.Id}}' "$RS_IMAGE")"
	[[ "$RS_BUILD_IMAGE" =~ ^sha256:[0-9a-f]{64}$ ]] || die "cannot read $RS_IMAGE's image ID"
	say "image $RS_IMAGE is $RS_BUILD_IMAGE"
}

phase_f() {
	local mirror_args=()
	if [ -n "$RS_VERDANDI_MIRROR" ]; then
		mirror_args=(--mount "type=bind,src=$RS_VERDANDI_MIRROR,dst=/verdandi-mirror,readonly" -e RS_VERDANDI_MIRROR=1)
		warn "rehearsal: the public Verdandi revision comes from $RS_VERDANDI_MIRROR, not $RS_VERDANDI_URL"
	fi
	step "phase F (network on): copy, cargo fetch, npm ci, the Skia and nvim downloads, the public Verdandi"
	ctr_run "eitri-release-f-$$" "$RS_LOGS/phase-f.log" \
		--mount "type=bind,src=$RS_SOURCE,dst=/src,readonly" \
		--mount "type=bind,src=$RS_WORK/src,dst=/build/src" \
		--mount "type=bind,src=$RS_WORK/check,dst=/build/check" \
		--mount "type=bind,src=$RS_ROOT/cargo,dst=/build/cargo" \
		--mount "type=bind,src=$RS_ROOT/target,dst=/build/target" \
		--mount "type=bind,src=$RS_ROOT/npm,dst=/build/npm" \
		--mount "type=bind,src=$RS_ROOT/skia,dst=/build/skia" \
		--mount "type=bind,src=$RS_ROOT/verdandi.git,dst=/build/verdandi" \
		"${mirror_args[@]}" -e "RS_VERDANDI_URL=$RS_VERDANDI_URL" -e "RS_VERDANDI_REV=$RS_VERDANDI_REV" \
		-- phase-f
}

phase_b() {
	step "phase B (--network none): build, check, package, assemble, check the extracted assets"
	ctr_run "eitri-release-b-$$" "$RS_LOGS/phase-b.log" --network none \
		--mount "type=bind,src=$RS_WORK/src,dst=/build/src" \
		--mount "type=bind,src=$RS_WORK/stage,dst=/build/stage" \
		--mount "type=bind,src=$RS_WORK/check,dst=/build/check" \
		--mount "type=bind,src=$RS_ROOT/cargo,dst=/build/cargo" \
		--mount "type=bind,src=$RS_ROOT/target,dst=/build/target" \
		--mount "type=bind,src=$RS_ROOT/npm,dst=/build/npm" \
		--mount "type=bind,src=$RS_ROOT/skia,dst=/build/skia,readonly" \
		--mount "type=bind,src=$RS_ROOT/verdandi.git,dst=/build/verdandi,readonly" \
		--mount "type=bind,src=$RS_REL,dst=/build/out" \
		--mount "type=bind,src=$RS_LOGS,dst=/build/logs" \
		-e "RS_VERDANDI_REV=$RS_VERDANDI_REV" -e "RS_BUILD_IMAGE=$RS_BUILD_IMAGE" \
		-- phase-b
}

proof() {
	local asset="eitri-$RS_VERSION-source.tar.gz"
	step "the proof (--network none, empty CARGO_HOME, fresh target, npm stubbed to fail): offline rebuild, then SOURCE's relink recipe"
	ctr_run "eitri-release-p-$$" "$RS_LOGS/proof.log" --network none \
		--mount "type=bind,src=$RS_REL/$asset,dst=/asset/$asset,readonly" \
		--mount "type=bind,src=$RS_PROOF,dst=/proof" \
		--mount "type=bind,src=$RS_LOGS,dst=/build/logs" \
		-e CARGO_HOME=/proof/cargo-home -e CARGO_TARGET_DIR=/proof/target \
		-- proof
}

# host_scans: publish/scan.sh (a private checkout only) over every extracted asset's scan view (M4:
# without vendor/, skia/ and proto/, which are checked by hash instead; a dist/ such as the source
# asset's built web bundle, and the source asset's top-level neovide/ fork checkout, are renamed so
# the scan reads them rather than skipping them).
host_scans() {
	local view name
	if [ "$RS_NO_LEAK_SCAN" = 1 ]; then
		warn "--no-leak-scan: the host leak scan is skipped"
		return 0
	fi
	step "publish/scan.sh over every extracted asset"
	mkdir -p -- "$RS_WORK/scan"
	for name in tarball deb rpm verdandi; do
		python3 "$RS_SOURCE/packaging/release_check.py" make-scan-view "$RS_WORK/check/x/$name" "$RS_WORK/scan/$name"
	done
	python3 "$RS_SOURCE/packaging/release_check.py" make-scan-view \
		"$RS_WORK/check/x/source/eitri-$RS_VERSION-source" "$RS_WORK/scan/source"
	mkdir -p -- "$RS_WORK/scan/assets"
	cp -- "$RS_REL/install.sh" "$RS_REL/RELEASE" "$RS_WORK/scan/assets/"
	for view in "$RS_WORK/scan"/*; do
		"$RS_SCAN" "$view" 2>&1 | tee -a "$RS_LOGS/scan.log" || die "publish/scan.sh found leaks in $view (above, and $RS_LOGS/scan.log)"
	done
}

fill_notes() {
	local notes="$RS_LOGS/release-notes.md" nvim_rs verify_section verify_section_zh
	nvim_rs="$(awk '/^\[\[package\]\]/ { n = 0 } /^name = "nvim-rs"$/ { n = 1 }
		n && /^version = / { gsub(/"/, "", $3); print $3; exit }' "$RS_SOURCE/Cargo.lock")"
	[ -n "$nvim_rs" ] || die "cannot read nvim-rs's version from $RS_SOURCE/Cargo.lock"
	# installer-claude-10 (Task 1): the Verify section describes what THIS BUILD's shipped
	# install.sh can check, which takes two facts, never one. preflight has refused unless
	# install.sh's embedded block equals packaging/release-signers, so reading that file is reading
	# what install.sh embeds; and RS_SIGN_KEY says whether this run made a SHA256SUMS.sig at all.
	#   - no key embedded (every rc so far, signed with a throwaway key or --unsigned): install.sh
	#     checks SHA256SUMS only, and a SHA256SUMS.sig is no trust anchor;
	#   - a key embedded and this run signed: preflight and verify_release_signature have held the
	#     signing key to the embedded one, so install.sh verifies the signature itself;
	#   - a key embedded and this run unsigned (preflight allows it only for --rehearsal, never
	#     published): no signature, and this build's install.sh refuses a release without one.
	# rc.1's notes went by RS_SIGN_KEY alone and called the key "built into it" with none embedded;
	# Task 1's first cut read only release-signers. test_release_sh.py's ReleaseNotes holds all three.
	# Task 5 (v1-dist, lane D): the same three facts, translated -- one Chinese variant per English
	# one, filled into @VERIFY_SECTION_ZH@ so the two languages can never describe a different build
	# (the owner's language decision: one page, English first, `## 简体中文` after).
	if signers_has_key "$RS_SOURCE/packaging/release-signers" && [ -n "$RS_SIGN_KEY" ]; then
		verify_section="$(cat <<'NV_VERIFY'
    sha256sum -c --ignore-missing SHA256SUMS

`SHA256SUMS` is signed (`SHA256SUMS.sig`): `install.sh` verifies it itself, with the release key
built into it. To check it by hand, against the key in the repository's
`packaging/release-signers`:

    ssh-keygen -Y verify -f release-signers -I release@eitri -n eitri-release -s SHA256SUMS.sig < SHA256SUMS
NV_VERIFY
)"
		verify_section_zh="$(cat <<'NV_VERIFY_ZH'
    sha256sum -c --ignore-missing SHA256SUMS

`SHA256SUMS` 已签名（`SHA256SUMS.sig`）：`install.sh` 会自行验证签名，使用内置的发布密钥。如果想手动核对，可对照仓库中的 `packaging/release-signers`：

    ssh-keygen -Y verify -f release-signers -I release@eitri -n eitri-release -s SHA256SUMS.sig < SHA256SUMS
NV_VERIFY_ZH
)"
	elif signers_has_key "$RS_SOURCE/packaging/release-signers"; then
		verify_section="$(cat <<'NV_VERIFY'
    sha256sum -c --ignore-missing SHA256SUMS

This build has no `SHA256SUMS.sig`. The `install.sh` built from this tree carries the release key
and refuses a release whose signature is missing, so it will not install this build.
NV_VERIFY
)"
		verify_section_zh="$(cat <<'NV_VERIFY_ZH'
    sha256sum -c --ignore-missing SHA256SUMS

本次构建没有 `SHA256SUMS.sig`。基于这份源码构建出的 `install.sh` 内置了发布密钥，会拒绝签名缺失的发布版本，因此它不会安装这个构建。
NV_VERIFY_ZH
)"
	else
		verify_section="$(cat <<'NV_VERIFY'
    sha256sum -c --ignore-missing SHA256SUMS

This release candidate carries no release key. `install.sh` built from this tree checks
SHA256SUMS only, which detects a damaged download, not who made the release; `SHA256SUMS.sig`,
when it exists, is signed with a throwaway key that is published nowhere and is not a trust
anchor, so there is no manual verification command against this build that would mean anything.
The first v0.2.0 final release will embed the owner's dedicated release key, and every installer
built after that verifies `SHA256SUMS.sig` automatically.
NV_VERIFY
)"
		verify_section_zh="$(cat <<'NV_VERIFY_ZH'
    sha256sum -c --ignore-missing SHA256SUMS

这个候选版本没有发布密钥。基于这份源码构建出的 `install.sh` 只检查 SHA256SUMS，这能发现下载损坏，但无法证明发布者身份；`SHA256SUMS.sig`（如果存在）是用一次性密钥签名的，该密钥未在任何地方公开，也不是信任锚点，因此没有任何针对本构建的手动验证命令是有意义的。第一个 v0.2.0 正式版会内置所有者专用的发布密钥，此后构建出的每个安装脚本都会自动验证 `SHA256SUMS.sig`。
NV_VERIFY_ZH
)"
	fi
	sed -e "s|@VERSION@|$RS_VERSION|g" -e "s|@COMMIT@|$RS_COMMIT|g" -e "s|@NVIM_RS_VERSION@|$nvim_rs|g" \
		-e "s|@SOURCE_ASSET@|eitri-$RS_VERSION-source.tar.gz|g" \
		-e "s|@VERDANDI_ASSET@|verdandi-${RS_VERDANDI_REV:0:7}-source.tar.gz|g" \
		-e "s|@VERDANDI_REV@|$RS_VERDANDI_REV|g" \
		"$RS_SOURCE/packaging/release-notes.md.in" > "$notes.tmp"
	awk -v r="$verify_section" -v rz="$verify_section_zh" \
		'{ if ($0 == "@VERIFY_SECTION@") { print r } else if ($0 == "@VERIFY_SECTION_ZH@") { print rz } else { print } }' \
		"$notes.tmp" > "$notes"
	rm -f "$notes.tmp"
	if grep -n '@[A-Z_]*@' "$notes" >&2; then die "release-notes.md.in has a placeholder release.sh does not fill"; fi
	if [ "$RS_NO_LEAK_SCAN" = 0 ]; then "$RS_SCAN" --message "$notes" || die "the release notes do not pass publish/scan.sh"; fi
	RS_NOTES="$notes"
}

host_main() {
	parse_args "$@"
	preflight
	prepare_dirs
	trap cleanup_containers EXIT
	trap 'cleanup_containers; exit 130' INT TERM
	local started=$SECONDS names=() name
	say "releasing Eitri $RS_VERSION from $RS_SOURCE at $RS_COMMIT (neovide $RS_FORK_COMMIT, Verdandi $RS_VERDANDI_REV) into $RS_REL"
	build_image
	phase_f
	phase_b
	proof
	host_scans

	mapfile -t names < <(python3 "$RS_SOURCE/packaging/release_check.py" asset-names "$RS_VERSION" "${RS_VERDANDI_REV:0:7}")
	names=("${names[@]:0:7}")
	for name in "${names[@]}"; do [ -f "$RS_REL/$name" ] || die "asset $name was not produced"; done
	step "SHA256SUMS, signature, read-only"
	write_sums "$RS_REL" "${names[@]}"
	names+=(SHA256SUMS)
	if [ -n "$RS_SIGN_KEY" ]; then
		sign_sums "$RS_SIGN_KEY" "$RS_REL"
		verify_release_signature "$RS_REL"
		names+=(SHA256SUMS.sig)
	else
		warn "no SHA256SUMS.sig (--unsigned)"
	fi
	fill_notes
	chmod a-w -- "$RS_REL"/* "$RS_REL"

	local gh_line paths=()
	for name in "${names[@]}"; do paths+=("$RS_REL/$name"); done
	gh_line="$(python3 "$RS_SOURCE/packaging/release_check.py" gh-release-command "$RS_VERSION" \
		--repo "${RS_PUBLIC_REPO#https://github.com/}" --title "Eitri $RS_VERSION" --notes-file "$RS_NOTES" \
		"${paths[@]}")"
	echo
	if [ "$RS_NO_LEAK_SCAN" = 1 ]; then
		echo "NOT LEAK-SCANNED (--no-leak-scan) -- publish/scan.sh never read these assets; never publish them."
	fi
	if [ "$RS_REHEARSAL" = 1 ]; then
		echo "REHEARSAL -- the commands below are what a real release would print; never run them for this output."
		[ -n "$RS_NVIM_VERSION" ] || echo "REHEARSAL -- RELEASE has no NVIM_VERSION/NVIM_SHA256_linux_x86_64: pins.env has no nvim pin yet."
		[ -z "$RS_VERDANDI_MIRROR" ] || echo "REHEARSAL -- Verdandi $RS_VERDANDI_REV was read from $RS_VERDANDI_MIRROR, not its public URL."
	fi
	echo "Eitri $RS_VERSION is in $RS_REL ($(( (SECONDS - started) / 60 )) min); logs in $RS_LOGS."
	echo "The rest is by hand (spec sec 4.4), in this order -- this script runs none of it:"
	echo "  publish/tag.sh $RS_SOURCE $RS_REL"
	echo "  git -C $RS_SOURCE push origin HEAD:main"
	echo "  git -C $RS_SOURCE push origin v$RS_VERSION"
	echo "  $gh_line"
}

# =================================================================================================
# The container side. Runs as the image's `builder` user, from the clone's own copy of this file.
# =================================================================================================

ctr_die() { echo "release.sh (container): $*" >&2; exit 1; }

ctr_env() {
	export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/build/target}"
	export CARGO_NET_GIT_FETCH_WITH_CLI=true
	export npm_config_cache=/build/npm
	export npm_config_update_notifier=false
	export EITRI_BUILD_COMMIT="$RS_COMMIT"
	# "A release always rebuilds clean" (v1-dist plan Task 6, P4-A1): shell/build.rs's own npm ci +
	# npm run build run unconditionally, never trusting a fingerprint (or a node_modules/) left over
	# from an earlier, unrelated build in this tree. Shared by phase F and phase B via this function;
	# ctr_proof deliberately does not call ctr_env and so never sets this -- its whole point is
	# proving the source asset's offline rebuild reaches no npm at all.
	export EITRI_WEB_CLEAN_BUILD=1
	umask 022
}

# ctr_fetch_pinned URL DEST SHA256: DEST, downloaded unless already there, and checked either way.
ctr_fetch_pinned() {
	if [ ! -f "$2" ]; then
		curl -fsSL --proto '=https' --tlsv1.2 -o "$2.part" -- "$1" || ctr_die "cannot download $1"
		mv -- "$2.part" "$2"
	fi
	echo "$3  $2" | sha256sum -c --quiet - || { rm -f -- "$2"; ctr_die "$2 does not match its pinned sha256 $3 (removed)"; }
}

# ctr_copy_source SRC DEST: phase F's copy of the clone, every copied file's mtime then set to now
# (F6, whole-branch review). `cp -a` keeps the clone's own mtimes -- its checkout time -- while
# $RS_ROOT/target is kept across runs, and cargo takes a path crate as fresh when none of its sources
# is newer than its last build there: a tree checked out before some later build in the same --out
# root would ship that build's code under this commit's RELEASE, and proof (a) could not tell, since
# --version comes from EITRI_BUILD_COMMIT. Touched, every path crate rebuilds; registry and git
# crates are keyed by version and stay cached.
ctr_copy_source() {
	cp -a "$1/." "$2/"
	find "$2" -exec touch -h {} +
}

ctr_phase_f() {
	ctr_env
	[ -z "$(ls -A /build/src)" ] || ctr_die "/build/src is not empty"
	ctr_copy_source /src /build/src
	cd /build/src
	[ "$(git rev-parse HEAD)" = "$RS_COMMIT" ] || ctr_die "the copy's HEAD is not $RS_COMMIT"
	read_pins packaging/pins.env

	local url="$RS_VERDANDI_URL"
	if [ "${RS_VERDANDI_MIRROR:-0}" = 1 ]; then
		export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0="url.file:///verdandi-mirror.insteadOf" GIT_CONFIG_VALUE_0="$RS_VERDANDI_URL"
		url=file:///verdandi-mirror
	fi
	step "cargo fetch --locked"
	cargo fetch --locked
	step "npm ci (agent-ui/web)"
	(cd agent-ui/web && npm ci --no-audit --no-fund)

	step "the Skia archive, checked against its pin"
	ctr_fetch_pinned "$RS_SKIA_URL_UPSTREAM" "/build/skia/$RS_SKIA_ARCHIVE" "$RS_SKIA_SHA256"
	if [ -n "$RS_NVIM_VERSION" ]; then
		step "the nvim pin, re-downloaded and checked (spec sec 4.2 step 3)"
		rm -f /build/check/nvim-linux-x86_64.tar.gz
		ctr_fetch_pinned "https://github.com/neovim/neovim/releases/download/v$RS_NVIM_VERSION/nvim-linux-x86_64.tar.gz" \
			/build/check/nvim-linux-x86_64.tar.gz "$RS_NVIM_SHA256"
		rm -f /build/check/nvim-linux-x86_64.tar.gz
	fi

	step "the public Verdandi at $RS_VERDANDI_REV"
	local vrc=0
	verdandi_fetch_public /build/verdandi "$url" "$RS_VERDANDI_REV" || vrc=$?
	case "$vrc" in
		0) ;;
		1) ctr_die "no branch of the public Verdandi ($url) contains $RS_VERDANDI_REV: publish it first (a commit an earlier --verdandi-mirror rehearsal left in this cache does not count)" ;;
		*) ctr_die "cannot fetch $url" ;;
	esac
	git -C /build/verdandi show "$RS_VERDANDI_REV:apps/claude-sidecar/scripts/buildBinary.mjs" > /build/check/buildBinary.mjs
	python3 packaging/release_check.py check-node-pin /build/check/buildBinary.mjs packaging/pins.env \
		|| ctr_die "the pinned Verdandi's Node pin differs from pins.env"
	rm -f /build/check/buildBinary.mjs
	step "phase F done"
}

# ctr_tar DIR PARENT OUT: a reproducible .tar.gz of PARENT/DIR (pre-think sec 3), keeping each
# file's own mtime -- the caller sets them.
ctr_tar() {
	tar --sort=name --format=posix --pax-option=exthdr.name=%d/PaxHeaders/%f,delete=atime,delete=ctime \
		--owner=0 --group=0 --numeric-owner -C "$2" -cf - "$1" | gzip -n -9 > "$3"
}

# ctr_stage_art ST: the desktop entry and the icon tree, copied from the clone (cwd) into the staging
# dir at the paths nfpm-public.yaml names as their sources. The entry is named by the application id
# (shell/src/main.rs's APP_ID), which the compositor matches a window to; the icons are every file under
# packaging/icons/hicolor, found rather than listed so that release_check.py's role table is the one
# list: a file it does not know fails check-assets, and nfpm-public.yaml's own entries are held to the
# committed files by test_nfpm_profiles.py. packaging/legacy/eitri.desktop (0.2.0's own entry) is staged
# too, for the tarball alone: no nfpm profile names it, so no package carries it.
ctr_stage_art() {
	local st="$1" f
	install -m 0644 packaging/cn.huntergrey.eitri.desktop "$st/packaging/cn.huntergrey.eitri.desktop"
	install -D -m 0644 packaging/legacy/eitri.desktop "$st/packaging/legacy/eitri.desktop"
	while IFS= read -r f; do
		install -D -m 0644 "packaging/icons/$f" "$st/packaging/icons/$f"
	done < <(cd packaging/icons && find hicolor -type f | LC_ALL=C sort)
}

# ctr_tarball_art ST TREE: the same files, from the staging dir, at the tarball's own paths
# (share/applications/..., share/icons/hicolor/...). The tarball also gets 0.2.0's desktop entry as
# share/applications/eitri.desktop (packaging/legacy/README.md): 0.2.0's install.sh, which a user may
# rerun to upgrade, refuses a tarball without it. Only that installer reads it; this release's ignores it.
ctr_tarball_art() {
	local st="$1" tree="$2" f
	install -D -m 0644 "$st/packaging/cn.huntergrey.eitri.desktop" "$tree/share/applications/cn.huntergrey.eitri.desktop"
	install -D -m 0644 "$st/packaging/legacy/eitri.desktop" "$tree/share/applications/eitri.desktop"
	while IFS= read -r f; do
		install -D -m 0644 "$st/packaging/icons/$f" "$tree/share/icons/$f"
	done < <(cd "$st/packaging/icons" && find hicolor -type f | LC_ALL=C sort)
}

ctr_phase_b() {
	ctr_env
	export CARGO_NET_OFFLINE=true npm_config_offline=true
	cd /build/src
	read_pins packaging/pins.env
	[ "$(git rev-parse HEAD)" = "$RS_COMMIT" ] || ctr_die "/build/src's HEAD is not $RS_COMMIT"
	RS_FORK_COMMIT="$(git -C neovide rev-parse HEAD)"
	[ "$RS_FORK_COMMIT" = "$(git ls-tree HEAD neovide | awk '{ print $3 }')" ] || ctr_die "neovide is not at its recorded commit"
	# The same value shell/build.rs would read from neovide/'s own checkout here, set explicitly so
	# collect-licenses.py prints it in SOURCE for a rebuild from the asset, which has no .git.
	export EITRI_BUILD_FORK_COMMIT="$RS_FORK_COMMIT"
	local sde skia_url out=/build/out logs=/build/logs b rc7="${RS_VERDANDI_REV:0:7}"
	sde="$(git log -1 --format=%ct HEAD)"
	skia_url="file:///build/skia/$RS_SKIA_ARCHIVE"
	export SKIA_BINARIES_URL='file:///build/skia/skia-binaries-{key}.tar.gz'
	export SOURCE_DATE_EPOCH="$sde"

	step "legacy stays out: the resolved feature graph (M15)"
	cargo tree --locked --offline -e features -p shell -p agent -p supervisor -i agent > "$logs/feature-tree.txt"
	if grep -n 'legacy-backend' "$logs/feature-tree.txt" >&2; then ctr_die "the resolved graph enables legacy-backend"; fi

	# shell/build.rs writes agent-ui/web/dist/index.html into the source tree, which this run copied
	# afresh, while /build/target is kept. A rerun of the same commit (a retry after a failure, or a
	# rehearsal then the real build) leaves every input shell/build.rs watches unchanged, so cargo
	# would not run it, the fresh copy would have no bundle, and include_str! would fail (Task 4
	# review). Cleaning shell alone makes its build script run against this copy every time.
	step "cargo clean --release -p shell (its build script writes the web bundle into this fresh copy)"
	cargo clean --release --locked --offline -p shell
	step "cargo build --release --locked -p shell -p agent -p supervisor --bins (no --features)"
	cargo build --release --locked --offline -p shell -p agent -p supervisor --bins \
		--message-format=json-render-diagnostics > "$logs/build.json"
	local exes=()
	mapfile -t exes < <(python3 packaging/release_check.py build-executables "$logs/build.json")
	[ "${#exes[@]}" = 4 ] || ctr_die "the build did not report exactly four executables"
	local skia_out
	skia_out="$(python3 packaging/release_check.py build-script-out-dir "$logs/build.json" skia-bindings)"
	python3 packaging/release_check.py check-skia-output "$(dirname "$skia_out")/output" "$skia_url"

	step "check-abi-floor.py over the four (GTK $RS_GTK_FLOOR, glibc $RS_GLIBC_FLOOR)"
	python3 packaging/check-abi-floor.py "${exes[@]}" --gtk "$RS_GTK_FLOOR" --glibc "$RS_GLIBC_FLOOR" \
		| tee "$logs/abi.txt"
	nm -C "$CARGO_TARGET_DIR/release/shell" > "$logs/shell.nm"
	python3 packaging/release_check.py check-legacy-symbols "$logs/shell.nm"

	step "the Verdandi source asset (git archive $RS_VERDANDI_REV, no prefix)"
	local vasset="verdandi-$rc7-source.tar.gz"
	git -C /build/verdandi archive --format=tar "$RS_VERDANDI_REV" | gzip -n -9 > "$out/$vasset"
	RS_VERDANDI_SOURCE_SHA256="$(sha256sum "$out/$vasset" | awk '{ print $1 }')"

	step "THIRD-PARTY-LICENSES and SOURCE (collect-licenses.py --no-sidecar)"
	rm -rf /build/stage/* /build/target/collect-licenses
	mkdir -p /build/stage/dist /build/target/collect-licenses
	python3 packaging/collect-licenses.py --no-sidecar --repo /build/src --binaries-dir "$CARGO_TARGET_DIR/release" \
		--source-url "$(source_asset_url "$RS_VERSION")" --source-notice /build/stage/dist/SOURCE \
		--skia-archive "$RS_SKIA_ARCHIVE" --skia-sha256 "$RS_SKIA_SHA256" \
		--skia-archive-path "/build/skia/$RS_SKIA_ARCHIVE" \
		--skia-archive-extract-dir /build/target/collect-licenses \
		--out /build/stage/dist/THIRD-PARTY-LICENSES

	step "RELEASE"
	write_release /build/stage/dist/RELEASE
	local vflag=()
	[ "$RS_REHEARSAL" = 0 ] || vflag=(--rehearsal)
	python3 packaging/release_check.py validate-release-file /build/stage/dist/RELEASE "${vflag[@]}"

	step "the staging dir (M6), the .deb and .rpm, and the tarball from the same bytes"
	local st=/build/stage
	mkdir -p "$st/target/release" "$st/packaging"
	for b in "${RS_BINARIES[@]}"; do install -m 0755 "$CARGO_TARGET_DIR/release/$b" "$st/target/release/$b"; done
	chmod 0644 "$st/dist/RELEASE" "$st/dist/THIRD-PARTY-LICENSES" "$st/dist/SOURCE"
	install -m 0755 packaging/install.sh "$st/packaging/install.sh"
	install -m 0755 packaging/eitri.launcher.sh "$st/packaging/eitri.launcher.sh"
	ctr_stage_art "$st"
	# The .deb's AppArmor profile (nfpm-public.yaml's `packager: deb` entry).
	install -D -m 0644 packaging/apparmor/eitri "$st/packaging/apparmor/eitri"
	install -m 0644 LICENSE "$st/LICENSE"
	install -m 0644 packaging/nfpm-public.yaml "$st/nfpm-public.yaml"
	(cd "$st" && VERSION="$RS_VERSION" EITRI_SOURCE_URL="$(source_asset_url "$RS_VERSION")" \
		nfpm pkg --config nfpm-public.yaml --packager deb --target "$out/eitri_${RS_VERSION}_amd64.deb")
	(cd "$st" && VERSION="$RS_VERSION" EITRI_SOURCE_URL="$(source_asset_url "$RS_VERSION")" \
		nfpm pkg --config nfpm-public.yaml --packager rpm --target "$out/eitri-${RS_VERSION}-1.x86_64.rpm")
	local tt="eitri-$RS_VERSION-x86_64-linux" tb=/build/check/tarball-tree
	rm -rf "$tb"; mkdir -p "$tb/$tt"
	install -D -m 0755 "$st/packaging/eitri.launcher.sh" "$tb/$tt/bin/eitri"
	for b in "${RS_BINARIES[@]}"; do install -D -m 0755 "$st/target/release/$b" "$tb/$tt/lib/eitri/$b"; done
	install -D -m 0755 "$st/packaging/install.sh" "$tb/$tt/lib/eitri/eitri-setup"
	install -D -m 0644 "$st/dist/RELEASE" "$tb/$tt/lib/eitri/RELEASE"
	ctr_tarball_art "$st" "$tb/$tt"
	install -D -m 0644 "$st/LICENSE" "$tb/$tt/share/licenses/eitri/LICENSE"
	install -D -m 0644 "$st/dist/THIRD-PARTY-LICENSES" "$tb/$tt/share/licenses/eitri/THIRD-PARTY-LICENSES"
	install -D -m 0644 "$st/dist/SOURCE" "$tb/$tt/share/licenses/eitri/SOURCE"
	find "$tb/$tt" -exec touch -h -d "@$sde" {} +
	ctr_tar "$tt" "$tb" "$out/$tt.tar.gz"

	step "the Eitri source asset (spec sec 11.3): git archive, neovide, vendor/, proto/, the web bundle, Skia"
	local sa="eitri-$RS_VERSION-source" sd=/build/check/source-tree
	rm -rf "$sd"; mkdir -p "$sd/$sa/neovide"
	git archive --format=tar HEAD | tar -x -C "$sd/$sa"
	git -C neovide archive --format=tar HEAD | tar -x -C "$sd/$sa/neovide"
	git -C /build/verdandi archive --format=tar "$RS_VERDANDI_REV" proto | tar -x -C "$sd/$sa"
	mkdir -p "$sd/$sa/.cargo"
	(cd "$sd/$sa" && cargo vendor --locked --offline vendor > .cargo/config.toml)
	grep -qx 'directory = "vendor"' "$sd/$sa/.cargo/config.toml" \
		|| ctr_die "cargo vendor's printed config does not name directory = \"vendor\""
	install -D -m 0644 agent-ui/web/dist/index.html "$sd/$sa/agent-ui/web/dist/index.html"
	# build.rs (v1-dist plan Task 6) skips npm only when this fingerprint matches the tree it finds --
	# shipping it alongside the bundle it was written for is what lets the source asset's own offline
	# rebuild skip npm too, with no mtime rule to keep in sync with build.rs's own (spec sec 11.3).
	install -D -m 0644 agent-ui/web/dist/.inputs-sha256 "$sd/$sa/agent-ui/web/dist/.inputs-sha256"
	install -D -m 0644 "/build/skia/$RS_SKIA_ARCHIVE" "$sd/$sa/skia/$RS_SKIA_ARCHIVE"
	install -m 0644 "$st/dist/THIRD-PARTY-LICENSES" "$st/dist/SOURCE" "$sd/$sa/"
	find "$sd/$sa" -exec touch -h -d "@$sde" {} +
	ctr_tar "$sa" "$sd" "$out/$sa.tar.gz"

	install -m 0644 packaging/install.sh "$out/install.sh"
	install -m 0644 "$st/dist/RELEASE" "$out/RELEASE"

	step "every asset, extracted and checked (spec sec 4.2 step 9)"
	rm -rf /build/check/x
	python3 packaging/release_check.py check-assets "$out" /build/check/x "$RS_VERSION" "$RS_VERDANDI_REV" \
		/build/src /build/verdandi "$RS_SKIA_ARCHIVE" "$RS_SKIA_SHA256"
	local shell_bin="/build/check/x/tarball/$tt/lib/eitri/shell" want got rc
	nm -C "$shell_bin" > "$logs/extracted-shell.nm"
	python3 packaging/release_check.py check-legacy-symbols "$logs/extracted-shell.nm"
	want="eitri $RS_VERSION (commit ${RS_COMMIT:0:12}, neovide fork ${RS_FORK_COMMIT:0:7}, verdandi $rc7)"
	got="$(env -i PATH=/usr/bin:/bin HOME=/home/builder "$shell_bin" --version)"
	[ "$got" = "$want" ] || ctr_die "shell --version prints '$got', expected '$want'"
	echo "$got" > "$logs/version.txt"
	for how in flag env; do
		rc=0
		if [ "$how" = flag ]; then
			got="$(env -i PATH=/usr/bin:/bin HOME=/home/builder "$shell_bin" --legacy 2>&1)" || rc=$?
		else
			got="$(env -i PATH=/usr/bin:/bin HOME=/home/builder EITRI_AGENT_BACKEND=legacy "$shell_bin" 2>&1)" || rc=$?
		fi
		[ "$rc" = 1 ] && [[ "$got" == *"$RS_LEGACY_MESSAGE"* ]] \
			|| ctr_die "the extracted shell with legacy asked for by $how exited $rc without saying '$RS_LEGACY_MESSAGE': $got"
	done
	echo "legacy proof: 4 executables built; nm finds no legacy symbol and the sidecar backend; --legacy and EITRI_AGENT_BACKEND=legacy exit 1 with '$RS_LEGACY_MESSAGE'; no agent-hook in any asset" \
		> "$logs/legacy-proof.txt"
	{ rustc -vV; dpkg-query -W; } > "$logs/toolchain.txt"
	step "phase B done"
}

ctr_proof() {
	local asset="eitri-$RS_VERSION-source" logs=/build/logs marker=/proof/npm-was-called
	mkdir -p /proof/stub /proof/cargo-home /proof/target /proof/work
	[ -z "$(ls -A /proof/cargo-home)" ] || ctr_die "/proof/cargo-home is not empty"
	[ -z "$(ls -A /proof/target)" ] || ctr_die "/proof/target is not empty"
	printf '#!/bin/sh\necho "npm $*" >> %s\nexit 1\n' "$marker" > /proof/stub/npm
	chmod 0755 /proof/stub/npm
	export PATH="/proof/stub:$PATH" CARGO_NET_OFFLINE=true
	tar -xzf "/asset/$asset.tar.gz" -C /proof/work
	cd "/proof/work/$asset"
	local archive name value
	archive="$(ls skia)"
	# The build identity comes from SOURCE's own rebuild lines, as it would for anyone rebuilding
	# the asset (no .git in it, nor in neovide/): the rebuilt --version must then equal the shipped
	# one phase B checked and wrote to version.txt (whole-branch review, lane D).
	unset EITRI_BUILD_COMMIT EITRI_BUILD_FORK_COMMIT
	while IFS='=' read -r name value; do
		export "$name=$value"
	done < <(python3 packaging/release_check.py rebuild-env SOURCE)
	[ "${EITRI_BUILD_COMMIT:-}" = "$RS_COMMIT" ] \
		|| ctr_die "SOURCE's EITRI_BUILD_COMMIT is '${EITRI_BUILD_COMMIT:-}', not $RS_COMMIT"
	[ -n "${EITRI_BUILD_FORK_COMMIT:-}" ] || ctr_die "SOURCE gives no EITRI_BUILD_FORK_COMMIT"

	step "proof (a): the full release command, offline and --locked, from the source asset"
	SKIA_BINARIES_URL="file://$PWD/skia/$archive" \
		cargo build --release --offline --locked -p shell -p agent -p supervisor --bins
	[ ! -e "$marker" ] || ctr_die "proof (a) reached npm: $(cat "$marker")"
	env -i PATH=/usr/bin:/bin HOME=/home/builder "$CARGO_TARGET_DIR/release/shell" --version > "$logs/proof-a-version.txt"
	cmp -s "$logs/proof-a-version.txt" "$logs/version.txt" \
		|| ctr_die "proof (a)'s shell --version prints '$(cat "$logs/proof-a-version.txt")', the shipped one '$(cat "$logs/version.txt")'"
	echo "proof (a): passed; $(cat "$logs/proof-a-version.txt"), the same as the shipped shell" | tee "$logs/proof-a.txt"

	step "proof (b): SOURCE's relink recipe, parsed out of SOURCE, with a trivial change to nvim-rs"
	local line key value copy="" unlock="" build="" modified="" patch=()
	while IFS= read -r line; do
		key="${line%%=*}"; value="${line#*=}"
		case "$key" in
			MODIFIED_DIR) modified="$value" ;;
			COPY) copy="$value" ;;
			UNLOCK) unlock="$value" ;;
			PATCH) patch+=("$value") ;;
			BUILD) build="$value" ;;
		esac
	done < <(python3 packaging/release_check.py relink-recipe SOURCE)
	[ -n "$copy" ] && [ -n "$unlock" ] && [ -n "$build" ] && [ "${#patch[@]}" = 2 ] || ctr_die "cannot read SOURCE's relink recipe"
	bash -c "$copy"
	bash -c "$unlock"
	printf '\n// Eitri release proof: a trivial change (LGPL-3.0 sec 4(d)(0) relink recipe)\npub const EITRI_RELINK_PROOF: u32 = 1;\n' \
		>> "$modified/src/lib.rs"
	printf '\n%s\n' "${patch[@]}" >> Cargo.toml
	bash -c "$build" 2>&1 | tee "$logs/proof-b-build.log"
	[ ! -e "$marker" ] || ctr_die "proof (b) reached npm: $(cat "$marker")"
	grep -Eq "Compiling nvim-rs v[0-9.]+ \(/proof/work/$asset/$modified\)" "$logs/proof-b-build.log" \
		|| ctr_die "proof (b) did not compile nvim-rs from $modified"
	[ "$(python3 packaging/release_check.py lock-package-sources Cargo.lock nvim-rs)" = "<path>" ] \
		|| ctr_die "after the relink, Cargo.lock still gives nvim-rs a registry source"
	echo "proof (b): passed; nvim-rs compiled from $modified, Cargo.lock's nvim-rs has no source" | tee "$logs/proof-b.txt"
}

ctr_main() {
	case "${1:-}" in
		phase-f) ctr_phase_f ;;
		phase-b) ctr_phase_b ;;
		proof) ctr_proof ;;
		*) ctr_die "unknown container step '${1:-}'" ;;
	esac
}

main() {
	if [ "${1:-}" = __container ]; then
		shift
		ctr_main "$@"
	else
		host_main "$@"
	fi
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
	main "$@"
fi
