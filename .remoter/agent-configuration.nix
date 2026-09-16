# The package set baked into the agent image, installed as the
# `agent-profile` from agent-flake.nix (see agent-init.sh) — the same role
# `environment.systemPackages` plays on the NixOS daemon host. Edit this
# list to change what run containers have on PATH beyond the Dockerfile's
# nix+devenv bootstrap; these files are part of the image's content key
# (image.rs hashes the whole .remoter/ build context), so any change
# rebuilds the image automatically.
{ pkgs, remoter-agent, system }:

with pkgs;
[
  # Do NOT add openssh here: the nixos/nix base image already ships the ssh
  # client as an element of the same system profile this buildEnv is
  # installed into, and `nix profile install` refuses the collision on
  # libexec/ssh-keysign (equal priority). The base element covers the
  # git+ssh flake fetch at image build time and ssh at runtime.
  # npm for the kimi ACP CLI install in agent-init.sh.
  nodejs_24
  # remoter-mcp is injected into every ACP session by bare name (spec §5.6)
  # — built from this repo's own remote flake, the same way the daemon host
  # installs it.
  remoter-agent.packages.${system}.remoter-mcp
]
