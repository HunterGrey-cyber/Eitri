# A minimal Arch image carrying only what
# a real end user's machine has -- the installer hint table's runtime libraries, plus openssh so
# the real `ssh-keygen -Y verify` path actually runs. No sudo, no base-devel: this
# is the prebuilt/curl install.sh path, which needs neither (the AUR packages build
# separately and are not exercised here -- no archlinux target through nfpm, so there is no
# neovibe pacman package to `pacman -U` in this image; arch's own e2e assertions are the curl flow
# only, no apt/dnf-equivalent root-install step).
#
# Built and run by packaging/tests/e2e/run.sh: `docker build
# --build-arg UID="$(id -u)" --build-arg GID="$(id -g)" -t <tag> -f packaging/tests/e2e/arch.Dockerfile
# packaging/tests/e2e`, then `docker run --rm --user "$(id -u):$(id -g)" --network host --mount
# type=bind,... <tag> ...`.
FROM archlinux:latest
ARG UID=1000
ARG GID=1000

# archlinux:latest ships no package database by default -- pacman -Sy populates it, a real network
# round trip like every other image's package-manager bootstrap here. Arch's hint row lists
# gtk4, webkitgtk-6.0. Plus curl/xz/ca-certificates and openssh for ssh-keygen.
# `shadow` provides useradd/groupadd (archlinux:latest already carries it, named explicitly so
# a minimal base image change cannot silently drop it).
RUN pacman -Sy --noconfirm \
	&& pacman -S --noconfirm --needed \
		gtk4 webkitgtk-6.0 curl xz ca-certificates openssh shadow \
	&& pacman -Scc --noconfirm

# A non-root user at the host's own uid/gid. `-m` gives it a real $HOME.
RUN if getent passwd "$UID" >/dev/null && [ "$(getent passwd "$UID" | cut -d: -f1)" != tester ]; then \
		userdel -r "$(getent passwd "$UID" | cut -d: -f1)" 2>/dev/null || userdel "$(getent passwd "$UID" | cut -d: -f1)" 2>/dev/null || true; \
	fi; \
	if getent group "$GID" >/dev/null && [ "$(getent group "$GID" | cut -d: -f1)" != tester ]; then \
		groupdel "$(getent group "$GID" | cut -d: -f1)" 2>/dev/null || true; \
	fi; \
	getent group tester >/dev/null || groupadd -g "$GID" tester; \
	getent passwd tester >/dev/null || useradd -u "$UID" -g "$GID" -m -d /home/tester -s /bin/bash tester
