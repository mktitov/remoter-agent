#!/bin/sh
# Image-init script for remoter-agent container mode (see agent.Dockerfile).
# Runs ONCE per image build, inside `devenv shell --no-tui --no-eval-cache`
# at /repo (the project clone, mounted read-only). Anything installed onto
# the image here is available to every run container.
set -e

# Install the image's package profile — the nix configuration of the image
# (agent-flake.nix + agent-configuration.nix, the same role the daemon
# host's flake.nix/configuration.nix play on NixOS). It brings remoter-mcp
# (built from this repo's own remote flake, pinned by the daemon to
# REMOTER_IMAGE_FLAKE_REV = HEAD of the synced central clone, so the image
# content matches the recorded label remoter.image_flake_rev) plus the
# out-of-shell tools (nodejs; the ssh client comes from the nixos/nix base
# image's own profile — listing openssh here collides with it). SSH access
# for the git+ssh fetch is provided by the daemon (agent forwarding or a
# read-only ~/.ssh mount, host known_hosts). A flake must be a directory
# containing flake.nix, so stage the repo's agent-flake.nix under that name
# (/repo is read-only anyway). Install into the *system* profile: the daemon
# bind-mounts the agent home over /root in run containers, shadowing
# anything installed into root's own profile.
: "${REMOTER_IMAGE_FLAKE_REV:?REMOTER_IMAGE_FLAKE_REV must be set by the daemon (image.rs)}"
flake_dir=$(mktemp -d)
cp /repo/.remoter/agent-flake.nix "$flake_dir/flake.nix"
cp /repo/.remoter/agent-configuration.nix "$flake_dir/"
nix profile install --profile /nix/var/nix/profiles/default \
    --no-write-lock-file \
    --override-input remoter-agent \
    "git+ssh://git@github.com/mktitov/remoter-agent?ref=main&rev=${REMOTER_IMAGE_FLAKE_REV}" \
    "$flake_dir#agent-profile"

# The ACP agent CLI (same package/version line as the daemon hosts) — npm
# comes from the agent profile installed above. Installs into /usr/local
# (on the image PATH via ENV in the Dockerfile).
npm install -g --prefix /usr/local @moonshot-ai/kimi-code@0.41.0
