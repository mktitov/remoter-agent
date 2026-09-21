#!/bin/sh
# Host-side seeding hook for the shared nix binary cache
# (docs/specs/remoter-agent-containers.md in the Remoter monorepo). The daemon
# runs this on the host, as the daemon user, with cwd = the project's central
# clone, before baking the agent image — but only when the host has nix and
# the image's OS/arch matches the host's (otherwise host-built paths could
# never substitute inside the container).
#
# Environment (set by the daemon; SSH_AUTH_SOCK/HOME are inherited):
#   REMOTER_NIX_CACHE_DIR   — the shared cache directory (a file:// binary
#                             cache); this hook exports closures into it
#   REMOTER_IMAGE_FLAKE_REV — the main rev the image is being baked from
#                             (the label remoter.image_flake_rev)
#
# The contract is fail-open: any failure only means an unseeded cache (a
# slower bake), so the script never has to be defensive about partial state —
# nix copy is idempotent and concurrent exports to the same cache are safe.
#
# What gets seeded mirrors what the init container builds (see
# agent-init.sh): the agent profile at exactly REMOTER_IMAGE_FLAKE_REV —
# including the crane build of remoter-mcp, the expensive part — and the
# project devshell closure the init step's `devenv shell` warm produces.
# On a daemon host whose own store is already warm (the daemon itself runs
# from this repo's flake) both exports are pure copies.

set -u

: "${REMOTER_NIX_CACHE_DIR:?set by the daemon (image.rs)}"
: "${REMOTER_IMAGE_FLAKE_REV:?set by the daemon (image.rs)}"

cache="file://$REMOTER_NIX_CACHE_DIR"
# The signing keypair lives next to the cache dir (generated once by the
# daemon); sign when it exists so signed-mode containers accept the paths.
key_file="$(dirname "$REMOTER_NIX_CACHE_DIR")/remoter-nix-cache-key.secret"

nix_cmd="nix --extra-experimental-features nix-command --extra-experimental-features flakes"

sign_and_copy() {
    if [ -f "$key_file" ]; then
        $nix_cmd store sign --key-file "$key_file" "$@" || return 1
    fi
    $nix_cmd copy --to "$cache" "$@"
}

# The agent profile at exactly the rev the image bakes in — the same staging
# agent-init.sh uses (a flake must be a directory containing flake.nix).
flake_dir=$(mktemp -d)
trap 'rm -rf "$flake_dir"' EXIT
cp .remoter/agent-flake.nix "$flake_dir/flake.nix"
cp .remoter/agent-configuration.nix "$flake_dir/"
profile=$($nix_cmd build --no-link --no-write-lock-file --print-out-paths \
    --override-input remoter-agent \
    "git+ssh://git@github.com/mktitov/remoter-agent?ref=main&rev=${REMOTER_IMAGE_FLAKE_REV}" \
    "$flake_dir#agent-profile")
sign_and_copy "$profile"

# The devshell closure: build the clone's shell on the host (a warm host
# store or cache.nixos.org make this cheap) and export the gc roots devenv
# registers under .devenv/gc/. Best-effort — the profile above matters most.
if command -v devenv >/dev/null 2>&1; then
    devenv shell --no-tui --no-eval-cache -- true || true
    for root in .devenv/gc/*; do
        [ -e "$root" ] || continue
        sign_and_copy "$(readlink -f "$root")" || true
    done
fi
