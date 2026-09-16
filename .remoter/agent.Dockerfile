# remoter-agent container mode (docs/specs/remoter-agent-containers.md in the
# Remoter monorepo): the daemon builds this image per content hash
# (image.rs), then warms the project devenv shell and runs agent-init.sh in
# an init container with the repo mounted read-only at /repo, and commits the
# result.
#
# This Dockerfile is only the BOOTSTRAP: nix + devenv, which the init step
# needs to enter the project shell. Everything else the image must provide
# on PATH is nix configuration, not Dockerfile:
#   - .remoter/agent-flake.nix + agent-configuration.nix — the image's
#     package profile (remoter-mcp built from this repo's remote flake,
#     nodejs; the ssh client is the base image's own profile element),
#     installed by agent-init.sh;
#   - agent-init.sh also installs kimi (the ACP agent the daemon drives,
#     spec §5.5) via the profile's npm.
FROM nixos/nix:2.30.2

ENV NIX_CONFIG="experimental-features = nix-command flakes"

# nixos/nix links /etc/{passwd,group,shadow} as absolute symlinks into
# /nix/store, and docker exec (runc/libpathrs, Docker ≥ 28) refuses to
# resolve those: every exec into the run container dies with
# "openat etc/group: path escapes from parent" (exit 126). Materialize them
# as regular files — the daemon drives the agent via `docker exec`.
RUN cp -f --remove-destination "$(readlink -f /etc/passwd)" /etc/passwd \
    && cp -f --remove-destination "$(readlink -f /etc/group)" /etc/group \
    && cp -f --remove-destination "$(readlink -f /etc/shadow)" /etc/shadow

# devenv — the only tool the init step needs before the agent profile
# exists (run containers execute the agent via
# `devenv shell --no-tui --no-eval-cache -- <cmd>` in /work; git is already
# in the base image). Install into the *system* profile, not root's: the
# daemon bind-mounts the agent home over /root in run containers (agent
# config + kimi sessions survive runs there), which shadows
# /root/.nix-profile and everything installed into it
# ("exec: \"devenv\": executable file not found in $PATH"). The same
# profile is where agent-init.sh installs the agent profile
# (agent-flake.nix), so its packages land on PATH too.
RUN nix profile install --profile /nix/var/nix/profiles/default \
        nixpkgs#devenv \
    && nix store gc --keep-derivations --keep-outputs || true

# /work in run containers is owned by the host uid while the agent runs as
# root, so every git call fails with "fatal: detected dubious ownership".
# Configure via --system: /root (and any --global config) is shadowed by the
# daemon's bind-mount of the agent home over /root.
RUN git config --system --add safe.directory '*'

# dart:io's embedded BoringSSL ignores SSL_CERT_FILE/NIX_SSL_CERT_FILE and
# reads its trust store from the hardcoded /etc/ssl/certs/ca-certificates.crt
# (falling back to c_rehash-style <hash>.0 lookups in /etc/ssl/certs). The
# nixos/nix image ships only ca-bundle.crt there, so every dart:io TLS
# handshake died with CERTIFICATE_VERIFY_FAILED (kimi's ripgrep bootstrap)
# while curl (OpenSSL, honors SSL_CERT_FILE) worked. Provide the expected
# path as a symlink to the existing bundle.
RUN ln -s /nix/var/nix/profiles/default/etc/ssl/certs/ca-bundle.crt \
        /etc/ssl/certs/ca-certificates.crt

# /usr/local/bin holds npm globals (kimi, installed by agent-init.sh) — the
# base image's PATH covers only the nix profiles, so add it explicitly.
ENV PATH="/usr/local/bin:${PATH}"
# kimi auth comes from .remoter/kimi-config.toml, rendered into the mounted
# agent home at container start (container.rs::provision_kimi_config) — the
# image stays secret-free.
ENV KIMI_DISABLE_TELEMETRY=1

# Optional project init: runs once per image build inside the warmed devenv
# shell (see agent-init.sh). Remove the COPY when no init script is needed.
COPY agent-init.sh /opt/agent-init.sh
