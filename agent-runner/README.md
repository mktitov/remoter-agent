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

At startup the daemon validates the token (`GET /auth/me`) and resolves
`remoter-mcp` against its own PATH — it exits with a clear error if either
fails. Then it polls the backend and claims tickets assigned to its agent
user (a ticket is executable when its project has `repo_url` set, §4.4).

The daemon talks to the backend over HTTP only. Which projects it works on is
backend-owned metadata (`projects.repo_url` / `base_branch`, edited from the
Flutter UI) — there is deliberately no project list in the TOML.

## The port contract (spec §5.8)

A ticket worktree that uses devenv gets its services via
`devenv --no-eval-cache up -d`. Parallel tickets of one project would collide
on hardcoded service ports (two Postgreses, one port), so:

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

## Tests

```sh
cargo test -p remoter-agent            # stub driver + fake ACP agent, no LLM
# Live/opt-in suite (needs the pinned kimi CLI installed):
cargo test -p remoter-agent --test acp_kimi -- --ignored --nocapture
#   ↑ live conformance against the pinned kimi version (spec §5.5); the test
#     refuses to run against any other version — bump PINNED_KIMI_VERSION
#     deliberately after re-validating.
```
