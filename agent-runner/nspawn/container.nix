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
# Design constraints baked into this module:
#
# - /nix/store is the HOST store, bind-mounted read-only. The machine must be
#   able to *use* existing store paths (devenv shells resolve instantly) but
#   can never build or substitute — hence no nix-daemon and NIX_REMOTE=""
#   (local store, direct access). Any attempt to realise a missing path
#   fails; that is desired — the daemon pre-warms closures on the host.
# - Networking is managed externally: the host side of the veth pair is
#   configured by the `remoter-nspawnctl` sudo helper, the container side by
#   a per-run systemd-networkd drop-in the daemon bind-mounts over
#   /etc/systemd/network/ (static /30: host <base>.<n>.1, host0 <base>.<n>.2).
#   The closure ships a permissive DHCP=ipv4 default for host0 (below) so the
#   interface is still configured if the drop-in is absent; the daemon's
#   drop-in overrides it with the static address. There is no DHCP server on
#   the veth link — without the drop-in, host0 simply stays unconfigured.
# - postgres and minio run as plain systemd units *inside* the machine (they
#   replace the docker sidecars of container mode) and listen on the
#   machine's loopback: 127.0.0.1:5432 / 127.0.0.1:9000.

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

  # Postgres replacing the container-mode sidecar. The daemon's per-run
  # *_DATABASE_URL vars point at 127.0.0.1 inside the machine, and tests
  # create per-run databases — hence CREATEDB for the postgres superuser.
  # trust-on-loopback is acceptable here: the machine is a single-run
  # throwaway with no other tenants.
  services.postgresql = {
    enable = true;
    # listen_addresses defaults to "localhost" — exactly what we want.
    ensureDatabases = [
      "remoter"
      "remoter-test"
      "remoter_e2e"
    ];
    ensureUsers = [
      {
        name = "postgres";
        ensureClauses.createdb = true;
      }
    ];
    authentication = lib.mkOverride 10 ''
      local all all trust
      host  all all 127.0.0.1/32 trust
      host  all all ::1/128      trust
    '';
  };

  # Minio replacing the container-mode sidecar. Dev-only root credentials —
  # the machine is ephemeral and unreachable except through the host.
  # minio is marked insecure in nixpkgs (upstream's feature-stripping
  # dispute); accept exactly that one package here — the service is
  # loopback-only inside a throwaway machine.
  nixpkgs.config.allowInsecurePredicate = pkg: (lib.getName pkg) == "minio";
  services.minio = {
    enable = true;
    listenAddress = "127.0.0.1:9000";
    consoleAddress = "127.0.0.1:9001";
    dataDir = [ "/var/lib/minio/data" ];
    rootCredentialsFile = "/etc/minio/root-credentials";
  };
  environment.etc."minio/root-credentials" = {
    # 0444: the minio service runs as an unprivileged user and must be able
    # to read this EnvironmentFile. Not a secret — dev creds inside a
    # throwaway machine.
    mode = "0444";
    text = ''
      MINIO_ROOT_USER=minioadmin
      MINIO_ROOT_PASSWORD=minioadmin
    '';
  };

  # Auto-create the attachments bucket once minio accepts connections.
  systemd.services.minio-create-buckets = {
    description = "Create the remoter-attachments bucket";
    after = [ "minio.service" ];
    requires = [ "minio.service" ];
    wantedBy = [ "multi-user.target" ];
    path = [ pkgs.minio-client ];
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
    };
    script = ''
      mc alias set local http://127.0.0.1:9000 minioadmin minioadmin
      for i in $(seq 1 60); do
        if mc mb --ignore-existing local/remoter-attachments; then
          exit 0
        fi
        sleep 1
      done
      echo "minio did not become ready within 60s" >&2
      exit 1
    '';
  };

  environment.systemPackages = with pkgs; [
    devenv
    git
    coreutils
    gnugrep
    findutils
    cacert
    # Client tools (pg_isready for the daemon's readiness polling, psql for
    # debugging); the server package is pulled in by services.postgresql.
    postgresql
    minio-client # mc
  ];

  # nix is needed for `devenv shell`, but only as a *client of the existing
  # store*: /nix/store is the host's, bind-mounted read-only, so building or
  # substituting is impossible (and unwanted — the daemon pre-warms). No
  # nix-daemon: mkForce overrides the nixos-container default NIX_REMOTE
  # "daemon" (that preset assumes the host's daemon socket is bind-mounted
  # in — ours is not); "" selects direct local-store access. NIX_CONFIG
  # enables the experimental CLI devenv relies on. NixOS's /etc/profile
  # (sourced even for non-interactive bash via set-environment) puts
  # systemPackages on PATH, so `devenv shell` works unattended.
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
