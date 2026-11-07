# remoter-agent

The headless daemon that executes Remoter tickets with a dev-agent (kimi over
ACP). The full spec lives in the Remoter monorepo (`docs/specs/remoter-agent.md`);
this README covers only day-to-day operation.

## Installation

The repo root flake ships `remoter-agent` + `remoter-mcp` as packages.
Installation is **host-only** — the daemon injects `remoter-mcp` into agent
sessions by absolute path, so target projects need no remoter-specific setup.

With plain nix (NixOS, nix-darwin, or standalone nix on macOS/Linux; needs
the `nix-command`/`flakes` features enabled):

```sh
nix profile install \
  github:mktitov/remoter-agent#remoter-agent \
  github:mktitov/remoter-agent#remoter-mcp
# ad-hoc run without installing:
nix run github:mktitov/remoter-agent#remoter-agent
```

On NixOS/nix-darwin, declare it in the system flake instead of
`nix profile` — add the input and put the packages into
`environment.systemPackages`:

```nix
# host flake.nix (nix-darwin example)
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    nix-darwin.url = "github:LnL7/nix-darwin";
    nix-darwin.inputs.nixpkgs.follows = "nixpkgs";
    remoter-agent.url = "github:mktitov/remoter-agent";
  };

  outputs = { nix-darwin, remoter-agent, ... }: {
    darwinConfigurations."my-mac" = nix-darwin.lib.darwinSystem {
      system = "aarch64-darwin";
      modules = [
        ({ pkgs, ... }: {
          environment.systemPackages = [
            remoter-agent.packages.${pkgs.system}.remoter-agent
            remoter-agent.packages.${pkgs.system}.remoter-mcp
          ];
          # optional: run the daemon as a launchd agent — point it at a
          # remoter-agent.toml and set REMOTER_AGENT_TOKEN via its environment
        })
      ];
    };
  };
}
```

Then `darwin-rebuild switch --flake .` (NixOS: `nixos-rebuild switch --flake .`
with the same `environment.systemPackages` block). Or pull the packages into
a host **devenv** shell:

```yaml
# host devenv.yaml
inputs:
  remoter-agent:
    url: github:mktitov/remoter-agent
    inputs:
      nixpkgs:
        follows: nixpkgs
```

```nix
# host devenv.nix
{ pkgs, inputs, ... }: {
  packages = with inputs.remoter-agent.packages.${pkgs.system}; [ remoter-agent remoter-mcp ];
}
```

With cargo instead of nix:

```sh
cargo install --git https://github.com/mktitov/remoter-agent remoter-agent
cargo install --git https://github.com/mktitov/remoter-agent remoter-mcp
```

Runtime prerequisites on the same host: `kimi` CLI (the ACP driver), `git`,
and `devenv`. From source instead: `cargo build --release -p remoter-agent -p remoter-mcp`.

## Running

```sh
export REMOTER_AGENT_TOKEN=…   # UI: Users → New agent (POST /users/agent, admin)
remoter-agent                  # or: cargo run -p remoter-agent
```

The token may instead live in the config as `token = "…"` (the env var wins
when both are set) — keep such a file owner-only (`chmod 600`; the daemon
warns otherwise).

The config file is searched in order: `REMOTER_AGENT_CONFIG` →
`./remoter-agent.toml` → `$XDG_CONFIG_HOME/remoter-agent.toml` →
`~/.config/remoter-agent.toml`. So with the file at
`~/.config/remoter-agent.toml` the daemon just runs as `remoter-agent`.

### Kimi agent configuration

With `driver.kind = "kimi-acp"`, provider, model, and thinking defaults come
from one daemon-owned file in every execution mode:

```toml
# remoter-agent.toml
[driver]
kind = "kimi-acp"
kimi_config = "~/.config/remoter-agent/kimi-config.toml" # default
```

The file is required at startup. Use `${KIMI_API_KEY}` as the provider key
placeholder and export the real key in the daemon's environment (for example,
through the systemd unit). At each run the daemon renders the source into
`<agent_home>/.kimi-code/config.toml` with owner-only permissions. Host mode
sets `KIMI_CODE_HOME` to that runtime directory; container and bwrap modes use
their isolated `agent_home` as Kimi's home. Edit only the daemon-owned source,
not the generated copy.

Projects need no Kimi configuration. To migrate an old project
`.remoter/kimi-config.toml`, move its contents to `driver.kimi_config`; the
project template is no longer read.

At startup the daemon validates the token (`GET /auth/me`) and resolves
`remoter-mcp` against its own PATH — it exits with a clear error if either
fails. Then it polls the backend and claims tickets assigned to its agent
user (a ticket is executable when its project has `repo_url` set, §4.4).

The daemon talks to the backend over HTTP only. Which projects it works on is
backend-owned metadata (`projects.repo_url` / `base_branch`, edited from the
Flutter UI) — there is deliberately no project list in the TOML.

## The port contract (spec §5.8)

A ticket worktree that uses devenv gets its services via
`devenv --no-eval-cache up -d` when its evaluated configuration defines
processes. Shell-only projects skip service startup and readiness after the
usual best-effort cleanup. Parallel tickets of one project would collide on
hardcoded service ports (two Postgreses, one
port), so:

1. The daemon allocates a unique **port block per run** (blocks of 50) and
   exports `REMOTER_AGENT_PORT_BASE` (plus `REMOTER_AGENT_TASK_ID` and
   `CI=1`) into every `devenv --no-eval-cache shell/up` invocation.
2. Projects parameterize their service ports from that variable, keeping the
   historical fixed value as the human default. The Remoter monorepo's
   `devenv.nix` is the reference:

   ```nix
   let
     agentPortBase = builtins.getEnv "REMOTER_AGENT_PORT_BASE";
     dbPort = if agentPortBase != "" then (pkgs.lib.toInt agentPortBase) + 1 else 5435;
   ```

   The same applies to `.mcp.json` entries pointing at fixed localhost ports:
   they must honor the port base (or use `${WORKSPACE_FOLDER}`-style relative
   paths) and must never carry committed tokens.
3. Projects that can't parameterize get serialized instead:
   `max_concurrent_runs_per_project = 1` in `remoter-agent.toml`.

## Container mode (spec remoter-agent-containers.md, Remoter monorepo)

With `[execution] mode = "container"` every attempt runs inside a per-run
docker container instead of on the host:

- The daemon builds a **per-project agent image** from the project clone's
  `.remoter/agent.Dockerfile` (Nix + devenv + the agent CLI) plus the optional
  `.remoter/agent-init.sh`, warming the project's devenv shell into the image
  in an init container and freezing it with `docker commit`. The tag is
  content-keyed — `<image_tag_prefix>:p<project_id>-<sha256-16>` — so an
  unchanged Dockerfile hits the local cache and a changed one rebuilds.
  A project without `.remoter/agent.Dockerfile` is a hard error (no silent
  fallback to host mode).
- Each run gets a fresh run container (worktree at `/work`, `repo/.git` at its
  host path, `agent_home` mounted as `$HOME`/`/root`) plus **postgres and
  minio sidecars sharing its network namespace** — inside, Postgres is always
  `127.0.0.1:5432` and MinIO `127.0.0.1:9000`, so the §5.8 port contract does
  not apply and `REMOTER_AGENT_PORT_BASE` is never exported.
- Agent commands run as `docker exec … devenv shell --no-tui --no-eval-cache
  -- <cmd>` — the same devenv shell as on the host, just inside Linux.
- Prerequisites: a working docker daemon (checked via `docker info` at
  startup; the daemon refuses to boot without it) and the pinned sidecar
  images (pulled on first use).
- **Disk / pruning:** budget ~8–16 GB per project image (PoC-1 measured a
  2.7 GB base → ~16.8 GB baked; the devshell profile dominates). Images are
  labeled `remoter.project_id` / `remoter.image_hash`, and the daemon prunes
  automatically: after every successful image build it removes the project's
  superseded tags, keeping the **current + previous** tag per project — that
  bounds agent images at ~16–32 GB per project while keeping a rollback. The
  prune is best-effort: an image still used by a running container is skipped
  with a warning in the daemon log. Run containers and sidecars are removed
  automatically after every run (a startup sweep reaps leftovers from a
  crashed daemon).

## Bubblewrap mode (rootless NixOS isolation)

`[execution] mode = "bwrap"` isolates each ACP turn with Linux namespaces. It
needs neither Docker, a per-project image, systemd-nspawn, root, nor sudo. The
project keeps its own `devenv.nix` services; no Postgres/MinIO sidecars are
created.

### Prerequisites and configuration

Use NixOS with unprivileged user namespaces enabled (the normal NixOS default)
and put the tools on the daemon's PATH:

```nix
# NixOS host configuration
environment.systemPackages = with pkgs; [
  bubblewrap
  slirp4netns
  git
  devenv
];
```

```toml
# remoter-agent.toml
api_url = "http://127.0.0.1:8181"
workspace_root = "~/.local/share/remoter-agent"

[execution]
mode = "bwrap"
# bwrap_binary = "bwrap"                  # default
# slirp4netns_binary = "slirp4netns"      # default
# agent_home = "~/.local/share/remoter-agent/agent-home"

[driver]
kind = "kimi-acp"
```

Startup fails closed if the binaries, Nix store/daemon socket, CA bundle,
agent/MCP programs, or a real namespace smoke test are unavailable. The daemon
also makes a read-only query through the host Nix daemon before accepting runs.
Install `kimi` and `remoter-mcp` on the daemon host as usual; absolute Nix store
paths and NixOS PATH profile entries remain visible inside the sandbox. Kimi
uses the same daemon-owned `driver.kimi_config` as host and container modes,
rendered into the persistent `agent_home` at each run.

When the daemon runs as a systemd service, `AmbientCapabilities` must be empty
in bwrap mode. Rootless user namespaces need neither `CAP_DAC_OVERRIDE` nor
`CAP_FOWNER`; do not carry those container-mode settings over to the bwrap unit,
and do not make `bwrap` setuid root. See
[Ambient capabilities from container mode](#ambient-capabilities-from-container-mode)
if startup fails its namespace smoke test.

### What happens on a run

1. The daemon prepares the same ticket worktree and performs a real host
   `devenv --no-eval-cache shell --no-tui -- true`. This fetches inputs and
   realizes the shell/process closures. It deliberately does **not** use bare
   `devenv build`, which does not warm the required environment in devenv 2.1.2.
2. One Rust launcher per ACP turn creates GC roots for the launcher, agent, and
   late-started `remoter-mcp` executable, then starts `bwrap` plus `slirp4netns`.
   The guest reaches the host Nix daemon through its socket, so devenv commands
   can realize derivations created or changed after the host pre-warm.
3. Inside the sandbox it evaluates the configured processes. If any exist, it
   runs best-effort `devenv processes down`, then `devenv up -d` and
   `devenv processes wait --timeout 120`; shell-only projects skip this
   lifecycle. It then `exec`s the agent with the original ACP stdio. Services
   stop on exit; cancellation, timeout, SIGTERM and SIGKILL are bounded by
   `--die-with-parent` and the
   network helper's exit fd. A launcher parent-death signal extends the same
   cleanup to daemon crashes, including SIGKILL. Stale state is reconciled using
   PID plus process start time, so active or unrelated runs are not removed.

The sandbox has separate user/PID/IPC/UTS/network namespaces and independent
localhost. Parallel tickets can therefore both use `127.0.0.1:5432` without a
host listener or port allocation. `REMOTER_AGENT_PORT_BASE`,
`REMOTER_CONTAINER`, and Docker's `*_DATABASE_URL` values are not exported;
instead the agent gets `CI=1`, `REMOTER_EXECUTION=bwrap`, and
`REMOTER_AGENT_TASK_ID`.

A loopback `api_url` such as `http://127.0.0.1:8181` is rewritten for the
agent/MCP to `http://10.0.2.2:8181` (slirp4netns's host gateway). Non-loopback
URLs are unchanged. The backend must listen on the corresponding host loopback
address; no firewall or host-wide network configuration is changed.

### Filesystem, Nix, and security boundaries

- `/nix/store` is read-only. The guest uses `NIX_REMOTE=daemon` and can access
  only the host daemon's socket, not its database. Builds and substitutions are
  performed by the host daemon, including for devenv changes made by the agent
  after pre-warm; the guest cannot write store paths directly.
- The worktree and `agent_home` are read-write at their **original absolute
  paths**; `/work` is only a compatibility symlink. Keeping the real path is
  required because it participates in devenv derivations. Main Git metadata is
  read-write so the agent can commit; reference worktrees and their Git metadata
  are read-only. Commits use the deterministic sandbox identity
  `remoter-agent <remoter-agent@localhost>` rather than depending on host Git
  configuration.
- Only explicit runtime paths, the Nix daemon socket, CA certificates, generated
  DNS/hosts/user config, and host PATH profile directories are added. The host
  filesystem is not mounted wholesale. `HOME` and the Nix fetcher cache live in
  `agent_home`, and `XDG_RUNTIME_DIR=/tmp` on both sides keeps derivations
  identical. The CA bundle is also exposed at the standard file path probed by
  Dart, so Flutter/pub uses normal certificate verification.
- This is process/filesystem/network isolation, not a VM or a defense against a
  malicious kernel or Nix daemon. The agent runs with the daemon's UID and can
  change anything in the writable worktree, Git metadata, and persistent agent
  home. It can also request host-side Nix builds/substitutions and consume host
  resources, and receives its intended Remoter MCP token and outbound network
  access.

### Troubleshooting

#### Ambient capabilities from container mode

If startup fails the namespace smoke test with:

```text
bubblewrap namespace smoke test failed with 1: bwrap: Unexpected capabilities but not setuid, old file caps config?
```

check whether the daemon's systemd unit still has container-mode capabilities.
A container-mode unit may use `CAP_DAC_OVERRIDE` and `CAP_FOWNER` so the daemon
can remove root-owned files left in worktrees by containers. Bwrap runs as the
ordinary daemon user, and the non-setuid `bwrap` binary rejects inherited
ambient capabilities instead of creating the namespace. Rootless user
namespaces do not need these capabilities, so clear the list when switching to
bwrap mode. On NixOS, override any capability list inherited from another module
declaratively:

```nix
{ lib, ... }:
{
  systemd.services.remoter-agent.serviceConfig.AmbientCapabilities =
    lib.mkForce [ ];
}
```

Apply the host configuration, restart the daemon, and verify the effective unit:

```sh
sudo nixos-rebuild switch --flake .
sudo systemctl restart remoter-agent.service
systemctl show remoter-agent.service -p AmbientCapabilities
```

The last command must print `AmbientCapabilities=`, with an empty value after
`=`. Do not work around the error by making `bwrap` setuid root; that contradicts
the mode's rootless security design.

After a previous container-mode deployment, root-owned files can remain in the
workspace. If cleanup reports permission errors, verify that the affected paths
are inside the intended workspace and have the host administrator correct their
ownership once (for example, with a carefully scoped `chown` back to the daemon
user). Do not restore ambient capabilities as a cleanup workaround.

If startup reports a Nix daemon query failure, verify that the host service and
socket are available to the daemon user. For other pre-warm errors, check that
`HOME`, `XDG_RUNTIME_DIR`, and the project path were not overridden by a wrapper.
Network failures should be diagnosed with the live suite below and the daemon's
slirp4netns stderr. A pre-warm, daemon, network, or service failure fails the
attempt; it never silently runs the agent on the host.

The live suite prints warm launch timings. Cold startup is dominated by the
project's host-side fetch/build and is therefore project-dependent. Bwrap
creates no per-project image; compare the Docker mode budget above (~8–16 GB
per image, plus build time). These are diagnostic measurements, not performance
guarantees; rerun the suite on the target host.

## Tests

```sh
cargo test -p remoter-agent            # stub driver + fake ACP agent, no LLM
# Live/opt-in suite (needs the pinned kimi CLI installed):
cargo test -p remoter-agent --test acp_kimi -- --ignored --nocapture
#   ↑ live conformance against the pinned kimi version (spec §5.5); the test
#     refuses to run against any other version — bump PINNED_KIMI_VERSION
#     deliberately after re-validating.

# Real NixOS bwrap/Nix daemon/network/devenv/Postgres/Flutter/lifecycle suite:
devenv shell --no-tui --no-eval-cache -- \
  nix shell nixpkgs#bubblewrap nixpkgs#slirp4netns nixpkgs#flutter -c \
  cargo test --test bwrap_live -- --ignored --nocapture --test-threads=1
# No LLM. The tests print warm launch timings; guest Nix builds use the host daemon.
```
