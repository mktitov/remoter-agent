# NixOS module for the shared systemd-nspawn container closure used by
# `execution.mode = "nspawn"` (ticket #237; spec
# docs/specs/remoter-agent-containers.md in the Remoter monorepo). Built via
# the flake output `agentContainer`:
#
#   nix build .#agentContainer --no-link --print-out-paths
#
# The daemon boots the resulting toplevel once per ticket run:
#
#   systemd-nspawn --boot --machine=rr-<id> --directory=<toplevel>
#     --network-veth --bind-ro=/nix/store --bind=<worktree>:/work ...
#
# (machine names are `rr-<run_id>`: a longer prefix would truncate the run id
# out of the 15-char `ve-` host interface name)
#
# The closure is deliberately MINIMAL and GENERIC — systemd, devenv, git and
# basic shell tools, nothing project-specific. agent-runner does not provide
# project dependencies: whatever a project needs (postgres, minio, …) is
# declared in that project's own devenv.nix and started with `devenv up`
# inside the machine. Default ports like 5432 are safe there because the
# machine's network namespace is private to the run — no cross-run or host
# port conflicts are possible.
#
# Design constraints baked into this module:
#
# - /nix/store is the HOST store, bind-mounted read-only. The machine must be
#   able to *use* existing store paths (devenv shells and `devenv up`
#   services resolve instantly) but can never build or substitute — hence no
#   nix-daemon and NIX_REMOTE="" (local store, direct access). Any attempt
#   to realise a missing path fails; that is desired — the daemon pre-warms
#   closures on the host.
# - Networking is managed externally: the host side of the veth pair is
#   configured by the `remoter-nspawnctl` sudo helper, the container side by
#   a per-run systemd-networkd drop-in the daemon bind-mounts over
#   /etc/systemd/network/ (static /30: host <base>.<n>.1, host0 <base>.<n>.2).
#   The closure ships a permissive DHCP=ipv4 default for host0 (below) so the
#   interface is still configured if the drop-in is absent; the daemon's
#   drop-in overrides it with the static address. There is no DHCP server on
#   the veth link — without the drop-in, host0 simply stays unconfigured.

{ config, pkgs, lib, ... }:

{
  # nspawn container conventions: no kernel/grub/bootloader, no udev, no
  # hardware anything — systemd is PID 1 in a private netns.
  boot.isContainer = true;

  networking.hostName = "remoter-nspawn";
  networking.useDHCP = false;
  # boot.isContainer defaults useHostResolvConf to true, which asserts
  # against the default-enabled systemd-resolved; the machine has its own
  # netns and does its own DNS through the host-side NAT.
  networking.useHostResolvConf = lib.mkForce false;
  services.resolved.enable = true;
  # Private netns behind host-side NAT (remoter-nspawnctl net-up); no
  # firewall needed inside.
  networking.firewall.enable = false;

  time.timeZone = "UTC";

  systemd.network.enable = true;
  # Fallback config for host0 (the container side of --network-veth). The
  # daemon's per-run drop-in bind-mounted over /etc/systemd/network/
  # overrides this with the run's static /30 address.
  systemd.network.networks."80-host0" = {
    matchConfig.Name = "host0";
    networkConfig.DHCP = "ipv4";
    linkConfig.RequiredForOnline = false;
  };

  # Generic tooling only — project-specific services/tools belong to each
  # project's devenv.nix, not to this closure.
  environment.systemPackages = with pkgs; [
    devenv
    git
    coreutils
    gnugrep
    findutils
    cacert
  ];

  # nix is needed for `devenv shell`/`devenv up`, but only as a *client of
  # the existing store*: /nix/store is the host's, bind-mounted read-only, so
  # building or substituting is impossible (and unwanted — the daemon
  # pre-warms). No nix-daemon: mkForce overrides the nixos-container default
  # NIX_REMOTE "daemon" (that preset assumes the host's daemon socket is
  # bind-mounted in — ours is not); "" selects direct local-store access.
  # NIX_CONFIG enables the experimental CLI devenv relies on. NixOS's
  # /etc/profile (sourced even for non-interactive bash via set-environment)
  # puts systemPackages on PATH, so `devenv shell` works unattended.
  environment.variables = {
    NIX_REMOTE = lib.mkForce "";
    NIX_CONFIG = "experimental-features = nix-command flakes";
  };
  nix.settings.experimental-features = [
    "nix-command"
    "flakes"
  ];

  system.stateVersion = "24.11";
}
