# NixOS module for the shared systemd-nspawn container closure used by
# `execution.mode = "nspawn"` (ticket #237; spec
# docs/specs/remoter-agent-containers.md in the Remoter monorepo). Built via
# the flake output `agentContainer`:
#
#   nix build .#agentContainer --out-link /tmp/agent-container --print-out-paths
#
# The helper boots the toplevel once per ticket run with --ephemeral and its
# explicit /init, so each machine gets writable temporary root state without
# modifying the shared store path. Machine names use the helper's host-wide
# lease slot (`rr-<slot>`), keeping the derived interface below IFNAMSIZ.
#
# The closure is deliberately MINIMAL and GENERIC — systemd, devenv, git and
# basic shell tools, nothing project-specific. agent-runner does not provide
# project dependencies: whatever a project needs (postgres, minio, …) is
# declared in that project's own devenv.nix and started with `devenv up`
# inside the machine. Default ports like 5432 are safe there because the
# machine's network namespace is private to the run and the host helper blocks
# forwarding between run interfaces.
#
# Design constraints baked into this module:
#
# - /nix/store and /nix/var/nix/db are the HOST store and validity database,
#   bind-mounted read-only. The machine can *use* existing store paths but can
#   never build or substitute: NIX_REMOTE selects a read-only local store and
#   there is no nix-daemon socket. The daemon pre-warms closures on the host.
# - Networking is managed externally: the host side of the veth pair is
#   configured by the `remoter-nspawnctl` sudo helper, the container side by
#   the lease's systemd-networkd drop-in bind-mounted over
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

  # Guest commands run as root while bind-mounted repositories belong to the
  # daemon user. Git's ownership check otherwise rejects every repository.
  environment.etc."gitconfig".text = ''
    [safe]
      directory = *
  '';

  # Nix is a client of the host's read-only store and validity database, never
  # a build or substitution path. NIX_CONFIG enables the CLI devenv relies on.
  environment.variables = {
    NIX_REMOTE = lib.mkForce "local?read-only=true";
    NIX_CONFIG = "experimental-features = nix-command flakes";
  };
  nix.settings.experimental-features = [
    "nix-command"
    "flakes"
  ];

  system.stateVersion = "24.11";
}
