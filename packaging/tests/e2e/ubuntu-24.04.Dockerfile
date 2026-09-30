# plan 2026-09-27-v1-dist, Task 15 (spec sec 6, 5.3, 7, 15): a minimal Ubuntu 24.04 image carrying
# only what a real end user's machine has -- the runtime libraries install.sh's own hint table
# names for this distro, plus openssh-client so the real `ssh-keygen -Y verify` path actually runs
# instead of silently degrading (codex verdict #5,
# the private review notes #5: ubuntu:24.04 carries no openssh-client by default, and install.sh's own D13
# degrade-on-missing-ssh-keygen path would otherwise accept a tampered SHA256SUMS.sig here with no
# test noticing). No nvim by default -- ARG WITH_DISTRO_NVIM=1 adds Ubuntu's own neovim (0.9.5,
# below the fork's 0.10 floor: spec sec 2.4's own `apt-cache policy` measurement), for the one
# variant run.sh uses to prove the private nvim install leaves the distro one untouched (spec sec
# 7). No sudo, no build toolchain, no git: install.sh's prebuilt/curl path needs none of them, and
# their absence is itself part of what this image proves.
#
# Built and run by packaging/tests/e2e/run.sh (spec sec 2.4's container hygiene): `docker build
# --build-arg UID="$(id -u)" --build-arg GID="$(id -g)" [--build-arg WITH_DISTRO_NVIM=1] -t <tag>
# -f packaging/tests/e2e/ubuntu-24.04.Dockerfile packaging/tests/e2e`, then `docker run --rm
# --user "$(id -u):$(id -g)" --network host --mount type=bind,... <tag> ...`.
FROM ubuntu:24.04
ARG UID=1000
ARG GID=1000
ARG WITH_DISTRO_NVIM=0
ENV DEBIAN_FRONTEND=noninteractive

# Only the runtime libraries spec sec 6.3's debian/ubuntu hint row names, plus curl/xz-utils/
# ca-certificates for the installer's own downloads and openssh-client for the signature (codex
# verdict #5). apt's own package lists are kept (not `rm -rf /var/lib/apt/lists/*`): the pkg-flow
# scenario (`apt install ./eitri_*.deb` as root) runs later, inside the same image, and re-running
# `apt-get update` there is one more real network round trip this image already proves is needed.
RUN apt-get update \
	&& apt-get install -y --no-install-recommends \
		libgtk-4-1 libwebkitgtk-6.0-4 curl xz-utils ca-certificates openssh-client \
	&& if [ "$WITH_DISTRO_NVIM" = 1 ]; then apt-get install -y --no-install-recommends neovim; fi

# A non-root user at the host's own uid/gid (spec sec 2.4 hygiene: `docker run --user`, so nothing
# root- or foreign-owned lands in a bind-mounted host directory). `-m` gives it a real $HOME under
# /home, matching how install.sh actually behaves for a real user (its own XDG-path rule reads
# $HOME) rather than the install-test harness's synthetic scratch homes.
RUN if getent passwd "$UID" >/dev/null && [ "$(getent passwd "$UID" | cut -d: -f1)" != tester ]; then \
		userdel -r "$(getent passwd "$UID" | cut -d: -f1)" 2>/dev/null || userdel "$(getent passwd "$UID" | cut -d: -f1)" 2>/dev/null || true; \
	fi; \
	if getent group "$GID" >/dev/null && [ "$(getent group "$GID" | cut -d: -f1)" != tester ]; then \
		groupdel "$(getent group "$GID" | cut -d: -f1)" 2>/dev/null || true; \
	fi; \
	getent group tester >/dev/null || groupadd -g "$GID" tester; \
	getent passwd tester >/dev/null || useradd -u "$UID" -g "$GID" -m -d /home/tester -s /bin/bash tester
