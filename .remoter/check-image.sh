#!/bin/sh
# Per-turn agent-image freshness hook (docs/specs/remoter-agent-containers.md
# §3.1). The daemon runs this on the host, as the daemon user, with cwd = the
# project's central clone (already synced to origin/main), whenever the
# content-keyed image tag is already built.
#
# Environment (set by the daemon; SSH_AUTH_SOCK/HOME are inherited):
#   REMOTER_IMAGE_TAG       — the content-keyed tag under scrutiny
#   REMOTER_IMAGE_BUILT_REV — label remoter.image_flake_rev of that image:
#                             the main rev the init step baked in (empty
#                             for images built before the label existed)
#   REMOTER_DOCKER          — the daemon's docker binary
#
# Exit-code contract:
#   0   — the image is fresh, keep the cache
#   42  — the image is stale, rebuild it under the same tag (reserved code)
#   any other non-zero — the check itself failed; the daemon logs a warning
#                        and keeps the cache (fail open)
#
# The image bakes remoter-mcp from this repo's remote flake (see
# agent-flake.nix), so freshness is the standard nix question "did the
# remoter-agent input move on main?": lock a scratch flake's remoter-agent
# input at the current main tip and compare the resolved rev with the one
# baked into the image. Requires nix (with flakes) and GitHub SSH access on
# the daemon host — both already required to build the image in the first
# place.

set -u

if [ -z "${REMOTER_IMAGE_BUILT_REV:-}" ]; then
    # The image predates the label (or the label is unreadable) — freshness
    # cannot be proven, so rebuild once; the new image carries the label.
    exit 42
fi

if ! command -v nix >/dev/null 2>&1; then
    echo "check-image.sh: nix not found on the daemon host; cannot check freshness" >&2
    exit 1
fi

flake_dir=$(mktemp -d)
trap 'rm -rf "$flake_dir"' EXIT
# The scratch flake exposes the input's rev as an output: `nix flake update`
# locks the whole transitive input graph (remoter-agent plus its
# crane/nixpkgs/... inputs), so scraping "rev" fields out of flake.lock
# cannot tell remoter-agent's rev from a transitive input's — the first
# match alphabetically is crane's, which made every image read as stale and
# rebuild on every turn. Evaluating the locked flake's own `rev` output
# reads remoter-agent's rev from sourceInfo.
cat > "$flake_dir/flake.nix" <<'EOF'
{
  inputs.remoter-agent.url = "git+ssh://git@github.com/mktitov/remoter-agent?ref=main";
  outputs = { self, remoter-agent }: { inherit (remoter-agent) rev; };
}
EOF

# `nix flake update` resolves the input against main right now and writes
# flake.lock; `nix eval .#rev` then reads the locked remoter-agent rev.
if ! (cd "$flake_dir" && nix --extra-experimental-features 'nix-command flakes' flake update remoter-agent) >&2; then
    echo "check-image.sh: nix flake update failed" >&2
    exit 1
fi
if ! main_rev=$(cd "$flake_dir" && nix --extra-experimental-features 'nix-command flakes' eval --raw .#rev 2>/dev/null) \
    || [ -z "$main_rev" ]; then
    echo "check-image.sh: could not read the locked remoter-agent rev" >&2
    exit 1
fi

# main moved since the image was baked -> the image is stale.
[ "$main_rev" = "$REMOTER_IMAGE_BUILT_REV" ] || exit 42
exit 0
