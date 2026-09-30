# packaging/container/build.Dockerfile -- the release build image (plan Task 12, Task 12 pre-think
# section 3 "Base image"/"Cache mounts"/"Root vs not", M13, M14, M18; spec
# docs/superpowers/specs/2026-09-27-v1-dist-design.md section 4.1). packaging/release.sh (Task 4)
# builds this once per release and runs the actual `cargo build`/`npm ci`/`nfpm pkg` steps inside
# it via `docker run`, never inside this Dockerfile.
#
# Build context is this directory ONLY (`docker build -f packaging/container/build.Dockerfile
# packaging/container`) -- nothing under the repository tree is COPYed in, so no host path from a
# private checkout can end up baked into a layer. Every pinned download's version and sha256 comes
# in as a build arg, read by the caller out of packaging/pins.env (which lives one directory up and
# is therefore never part of this build context either) -- pins.env stays the one source of truth
# spec section 4.1 names, and this file never repeats a pinned hash as a literal.
#
#   docker build \
#       --build-arg UID="$(id -u)" --build-arg GID="$(id -g)" \
#       --build-arg RUSTUP_INIT_VERSION=... --build-arg RUSTUP_INIT_SHA256_LINUX_X86_64=... \
#       --build-arg NODE_VERSION=... --build-arg NODE_SHA256_LINUX_X64=... \
#       --build-arg NFPM_VERSION=... --build-arg NFPM_SHA256_LINUX_X86_64=... \
#       -t eitri-release-build:<version> \
#       -f packaging/container/build.Dockerfile packaging/container
#
# Never `docker build --pull`: the pinned digest below is the only base there is (Global
# Constraints -- /etc/docker/daemon.json points registry traffic at a dead proxy). A 2-line probe
# Dockerfile (this FROM line + `RUN true`, built without --pull) was checked first and used only
# the local image, per the pre-think's own risk table.
FROM ubuntu:24.04@sha256:008173c23f95b170204355c12626cb5a965d779a7e1283b09e9cffbb1bf33ca3

# spec section 4.1's apt list, plus ca-certificates (M14: the list as written lacks it, and
# --no-install-recommends https downloads below need it) and libarchive-tools (bsdtar, for the
# content checks spec section 4.2 step 9 runs on extracted assets).
RUN apt-get update \
	&& DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
		build-essential \
		clang \
		pkg-config \
		libgtk-4-dev \
		libwebkitgtk-6.0-dev \
		protobuf-compiler \
		git \
		curl \
		xz-utils \
		binutils \
		python3 \
		libarchive-tools \
		ca-certificates \
	&& rm -rf /var/lib/apt/lists/*

# --- Rust, via rustup-init checked by sha256 --------------------------------------------------
#
# The pitfall (pre-think section 3, "Cache mounts"): install with CARGO_HOME=/opt/cargo
# RUSTUP_HOME=/opt/rustup, put /opt/cargo/bin on PATH, and ONLY THEN set CARGO_HOME=/build/cargo
# for the actual project builds. /build/cargo is where release.sh bind-mounts a host cache
# directory (rw in phase F, and the proof run gets an empty CARGO_HOME entirely) -- installing
# with CARGO_HOME already pointed there would put cargo/rustc's rustup-proxy binaries inside a
# directory that gets hidden by that mount, so the toolchain vanishes at `docker run` time even
# though it built successfully in the image.
ARG RUSTUP_INIT_VERSION
ARG RUSTUP_INIT_SHA256_LINUX_X86_64
ENV RUSTUP_HOME=/opt/rustup \
	CARGO_HOME=/opt/cargo
RUN set -eu; \
	curl -fsSL -o /tmp/rustup-init \
		"https://static.rust-lang.org/rustup/archive/${RUSTUP_INIT_VERSION}/x86_64-unknown-linux-gnu/rustup-init"; \
	echo "${RUSTUP_INIT_SHA256_LINUX_X86_64}  /tmp/rustup-init" | sha256sum -c -; \
	chmod +x /tmp/rustup-init; \
	/tmp/rustup-init -y --no-modify-path --profile minimal --default-toolchain 1.96.0; \
	rm -f /tmp/rustup-init
ENV PATH=/opt/cargo/bin:${PATH}
# Only now: every project build after this line uses the bind-mounted cache instead of /opt/cargo,
# while /opt/cargo/bin (rustc, cargo, and rustup's own proxies) stays reachable via PATH regardless
# of what is (or is not) mounted at /build/cargo.
ENV CARGO_HOME=/build/cargo

# --- Node, checked by sha256, for `shell/build.rs`'s `npm ci` of agent-ui/web only -------------
#
# spec section 4.1: "the official Node tarball at the version and sha256 in packaging/pins.env
# ... used for shell/build.rs's npm ci of agent-ui/web and nothing else: the release never builds
# the sidecar." NODE_VERSION/NODE_SHA256_LINUX_X64 are pins.env's NODE_VERSION/
# NODE_SHA256_linux_x64, passed in as build args since pins.env itself is outside this build
# context.
ARG NODE_VERSION
ARG NODE_SHA256_LINUX_X64
RUN set -eu; \
	curl -fsSL -o /tmp/node.tar.xz \
		"https://nodejs.org/dist/${NODE_VERSION}/node-${NODE_VERSION}-linux-x64.tar.xz"; \
	echo "${NODE_SHA256_LINUX_X64}  /tmp/node.tar.xz" | sha256sum -c -; \
	mkdir -p /opt/node; \
	tar -xJf /tmp/node.tar.xz -C /opt/node --strip-components=1; \
	rm -f /tmp/node.tar.xz
ENV PATH=/opt/node/bin:${PATH}

# --- nfpm, checked by sha256 --------------------------------------------------------------------
#
# A static Go binary that writes .deb/.rpm without dpkg-deb/rpmbuild (spec section 4.1).
# NFPM_VERSION/NFPM_SHA256_LINUX_X86_64 are pins.env's NFPM_VERSION/NFPM_SHA256_linux_x86_64.
ARG NFPM_VERSION
ARG NFPM_SHA256_LINUX_X86_64
RUN set -eu; \
	curl -fsSL -o /tmp/nfpm.tar.gz \
		"https://github.com/goreleaser/nfpm/releases/download/v${NFPM_VERSION}/nfpm_${NFPM_VERSION}_Linux_x86_64.tar.gz"; \
	echo "${NFPM_SHA256_LINUX_X86_64}  /tmp/nfpm.tar.gz" | sha256sum -c -; \
	mkdir -p /tmp/nfpm-extract; \
	tar -xzf /tmp/nfpm.tar.gz -C /tmp/nfpm-extract nfpm; \
	install -m 0755 /tmp/nfpm-extract/nfpm /usr/local/bin/nfpm; \
	rm -rf /tmp/nfpm.tar.gz /tmp/nfpm-extract

# --- The build user (pre-think section 3, "Root vs not"; M14) ----------------------------------
#
# ubuntu:24.04 already ships uid 1000 as user "ubuntu" (noted in packaging/tests/install/
# Dockerfile.dash), and this host's own uid is 1000 too. Rename it in place when the caller's UID
# matches (usermod -l, so the /opt/{cargo,rustup,node} directories that build step already chowned
# to uid 1000 keep working); create a fresh "builder" user otherwise. Either way `docker run` is
# expected to pass --user "$UID:$GID" -e HOME=/home/builder (spec section 4.1, pre-think section
# 3): nothing in this image is ever run as root except this build.
ARG UID=1000
ARG GID=1000
RUN set -eu; \
	if [ "$(id -u ubuntu 2>/dev/null || echo -1)" = "${UID}" ]; then \
		usermod -l builder -d /home/builder -m ubuntu; \
		groupmod -n builder ubuntu 2>/dev/null || true; \
	else \
		getent group "${GID}" >/dev/null 2>&1 || groupadd -g "${GID}" builder; \
		useradd -u "${UID}" -g "${GID}" -m -d /home/builder -s /bin/bash builder; \
	fi

# --- Fixed in-container paths (spec section 4.1; pre-think section 3, "Cache mounts") ----------
#
# No host path is ever compiled into a public binary: /build/src is where the read-only source
# clone is copied, /build/{cargo,target,npm,skia} are the bind-mounted host caches (skia ro in
# phase B), /build/out is where the assembled release lands. Created here, owned by builder, so a
# `docker run` that leaves one of them unmounted (the proof run's fresh target/ and empty
# cargo-home/, for instance) still has a writable, correctly-owned directory rather than a
# root-owned one from image build time. Owned by the numeric ${UID}:${GID} that `docker run --user`
# passes, not by name: a GID the base image already has (1000, "ubuntu", with any UID but 1000)
# means no group called builder exists, and `chown builder:builder` failed the image build
# (whole-branch review, codex; reproduced in the base image with UID 1001, GID 1000).
RUN mkdir -p /build/src /build/cargo /build/target /build/npm /build/skia /build/out \
	&& chown -R "${UID}:${GID}" /build

USER builder
WORKDIR /build/src
