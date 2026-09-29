# plan 2026-09-27-v1-dist, Task 15 (spec sec 6, 5.3, 7, 15): a minimal Fedora image carrying only
# what a real end user's machine has -- spec sec 6.3's fedora/rhel hint row's runtime libraries, plus
# openssh-clients so the real `ssh-keygen -Y verify` path actually runs (codex verdict #5). Fedora 44,
# not the plan's original Fedora 41 (owner, 2026-09-28): fedora-44.Dockerfile is this file's own
# name, and the rpm install check (run.sh) runs on Fedora 44 too. No sudo, no build toolchain: this
# is the prebuilt/curl install.sh path, which needs neither.
#
# Built and run by packaging/tests/e2e/run.sh (spec sec 2.4's container hygiene): `docker build
# --build-arg UID="$(id -u)" --build-arg GID="$(id -g)" -t <tag> -f
# packaging/tests/e2e/fedora-44.Dockerfile packaging/tests/e2e`, then `docker run --rm --user
# "$(id -u):$(id -g)" --network host --mount type=bind,... <tag> ...`.
FROM fedora:44
ARG UID=1000
ARG GID=1000

# Fedora's own hint row (spec sec 6.3): gtk4, webkitgtk6.0. Plus curl/xz/ca-certificates for the
# installer's own downloads (Fedora ships curl already; named explicitly so a minimal base image
# change cannot silently drop it) and openssh-clients for ssh-keygen (codex verdict #5). Package
# lists are not cleaned afterward: the pkg-flow scenario (`dnf install ./neovibe-*.rpm` as root)
# runs later in the same image and re-touches dnf's metadata cache regardless.
RUN dnf install -y --setopt=install_weak_deps=False \
		gtk4 webkitgtk6.0 curl xz ca-certificates openssh-clients shadow-utils \
	&& dnf clean packages
# fedora:44's own base image ships sudo (unlike ubuntu:24.04/archlinux:latest, which do not) --
# removed so all three images agree: no sudo anywhere, so any attempt to use one fails loudly
# (run.sh's own assertion) rather than quietly working. dnf's default dnf.conf protects `sudo`
# against ordinary removal (measured: plain `dnf remove -y sudo` refuses, "would result in removing
# ... protected packages"), so protected_packages is cleared for this one removal.
RUN rpm -q sudo >/dev/null 2>&1 && dnf remove -y --setopt=protected_packages= sudo || true

# A non-root user at the host's own uid/gid (spec sec 2.4 hygiene). `-m` gives it a real $HOME.
RUN if getent passwd "$UID" >/dev/null && [ "$(getent passwd "$UID" | cut -d: -f1)" != tester ]; then \
		userdel -r "$(getent passwd "$UID" | cut -d: -f1)" 2>/dev/null || userdel "$(getent passwd "$UID" | cut -d: -f1)" 2>/dev/null || true; \
	fi; \
	if getent group "$GID" >/dev/null && [ "$(getent group "$GID" | cut -d: -f1)" != tester ]; then \
		groupdel "$(getent group "$GID" | cut -d: -f1)" 2>/dev/null || true; \
	fi; \
	getent group tester >/dev/null || groupadd -g "$GID" tester; \
	getent passwd tester >/dev/null || useradd -u "$UID" -g "$GID" -m -d /home/tester -s /bin/bash tester
