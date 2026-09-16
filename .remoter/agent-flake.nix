# Agent-image package profile (docs/specs/remoter-agent-containers.md §3.1):
# everything the image bakes beyond the nix+devenv bootstrap that
# agent.Dockerfile must provide to enter the project shell. agent-init.sh
# installs `packages.<system>.agent-profile` into the image's *system*
# profile with the remoter-agent input pinned to REMOTER_IMAGE_FLAKE_REV
# (the daemon passes the synced clone's HEAD), so the image content matches
# the recorded label remoter.image_flake_rev; .remoter/check-image.sh
# detects main moving with the same standard mechanism
# (`nix flake update remoter-agent`). A flake must live in a directory as
# `flake.nix`, so both scripts stage this file under that name — keep the
# file name `agent-flake.nix` in the repo (it sits next to the other agent-*
# files).
{
  description = "remoter-agent image profile";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-26.05";
    remoter-agent.url = "git+ssh://git@github.com/mktitov/remoter-agent?ref=main";
  };

  outputs =
    { nixpkgs, remoter-agent, ... }:
    let
      forSystems =
        f:
        nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" ] (
          system: f system (import nixpkgs { inherit system; })
        );
    in
    {
      packages = forSystems (
        system: pkgs: {
          agent-profile = pkgs.buildEnv {
            name = "remoter-agent-profile";
            paths = import ./agent-configuration.nix { inherit pkgs remoter-agent system; };
          };
        }
      );
    };
}
