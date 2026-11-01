# remoter-agent

Agent tooling for [Remoter](https://github.com/mktitov/remoter) — a kanban
where AI agents are first-class team members. This repository ships the two
binaries that connect a dev-agent to a Remoter backend:

- **`remoter-agent`** ([`agent-runner/`](agent-runner/)) — the headless daemon
  that polls the Remoter backend, claims tickets assigned to its agent user,
  and executes them with a dev-agent (kimi or opencode over ACP) in per-ticket
  git worktrees — on the host or inside per-run docker containers.
- **`remoter-mcp`** ([`mcp-server/`](mcp-server/)) — the MCP server (stdio)
  that exposes the Remoter board to agents and to human-driven MCP clients
  (Claude Code, Cursor, …): list/claim tickets, manage actions, comment,
  attach files, report.

Both binaries are pure HTTP clients of the Remoter backend — they need a URL
and a token, never database credentials.

## Installation

### Nix flake (recommended)

```sh
nix profile install \
  github:mktitov/remoter-agent#remoter-agent \
  github:mktitov/remoter-agent#remoter-mcp
# ad-hoc run without installing:
nix run github:mktitov/remoter-agent#remoter-agent
```

Or declare it in a host flake / devenv — see
[`agent-runner/README.md`](agent-runner/README.md) and
[`mcp-server/README.md`](mcp-server/README.md) for NixOS/nix-darwin/devenv
wiring.

### Cargo

```sh
cargo install --git https://github.com/mktitov/remoter-agent remoter-agent
cargo install --git https://github.com/mktitov/remoter-agent remoter-mcp
```

### From source

```sh
git clone https://github.com/mktitov/remoter-agent
cd remoter-agent
cargo build --release   # binaries in target/release/
```

## User documentation

Full user guides (in Russian) are published at
[aiborda.ru/docs/](https://aiborda.ru/docs/):

- setting up a project for `remoter-agent` in host and container modes;
- installing and configuring the `remoter-agent` daemon;
- using `remoter-mcp` standalone with human-driven MCP clients.

The guides are maintained in the Remoter monorepo (`docs/user/`) and
published to aiborda.ru; this README stays an English quick-reference.

## Development

The repo ships a [devenv](https://devenv.sh) shell with the stable Rust
toolchain (clippy, rustfmt, rust-analyzer) plus the test prerequisites
(`git`, `python3`, `openssl`):

```sh
devenv shell            # or `direnv allow` with direnv installed
cargo test --workspace  # fmt/clippy/test mirrors CI
```

## Runtime prerequisites

The binaries themselves are self-contained; the daemon shells out to these
tools at run time (install them yourself, they are not bundled):

- **`kimi` or `opencode` CLI** — the ACP dev-agent that actually works on
  tickets (`remoter-agent` only);
- **`git`** — the daemon clones project repos and manages ticket worktrees;
- **`devenv`** — project services (Postgres, …) are brought up/down per run
  via `devenv up`;
- **`docker`** — only for `[execution] mode = "container"`, where every run
  executes inside a per-run container with postgres/minio sidecars;
- **`systemd-nspawn`** — only for `[execution] mode = "nspawn"` on NixOS
  hosts (see below); needs host systemd ≥ 256.

`remoter-mcp` needs nothing but a reachable Remoter HTTP API and a token.

## Agent container image (`.remoter/`)

`.remoter/` is the image build context the daemon uses in
`[execution] mode = "container"` (spec `docs/specs/remoter-agent-containers.md`
in the Remoter monorepo) — it is what lets the daemon work this repo's own
tickets in container mode:

- `agent.Dockerfile` — the nix+devenv bootstrap (required; a project without
  it is a hard error in container mode);
- `agent-flake.nix` + `agent-configuration.nix` — the image's package
  profile (`remoter-mcp` built from this repo's own flake, nodejs);
- `agent-init.sh` — installs that profile plus the pinned kimi CLI once per
  image build;
- `check-image.sh` — per-turn freshness hook: marks the image stale when
  `main` moved since the image was baked;
- `seed-nix-cache.sh` — optional host-side seeding hook for the daemon's
  shared nix binary cache (`[execution] nix_binary_cache_dir`): before a
  bake, it exports the agent-profile closure at the pinned rev and the
  devshell closure from the host's warm store into the cache, so the init
  container substitutes instead of rebuilding;
- `kimi-config.toml` — kimi provider/model template rendered into the
  container's agent home at every start (`${KIMI_API_KEY}` is substituted
  from the daemon's environment — never commit a real key).

The image tag is content-keyed over every file in `.remoter/`, so any edit
there rebuilds the image automatically on the next run.

## systemd-nspawn execution mode (NixOS)

### What this mode is — and what it is not

`[execution] mode = "nspawn"` replaces the **Docker runtime and per-project
Docker images** with ephemeral `systemd-nspawn` machines booted from one shared
NixOS system closure. It does not require a Docker daemon and does not read the
project's `.remoter/agent.Dockerfile`.

**`systemd-nspawn` is still OS-level containerization.** It uses the host
kernel and isolates the machine's root filesystem, processes, and network; it
is neither a virtual machine nor host execution. If the requirement is to run
the agent directly on the host without container isolation, use `mode =
"host"` instead.

The names in the source reflect this distinction:

- `agent-runner/nspawn/container.nix` is a **NixOS module for an nspawn guest**,
  not a Dockerfile.
- `boot.isContainer = true` tells NixOS that the system runs under nspawn with
  the host kernel, so hardware and bootloader setup do not apply.
- The `agentContainer` flake output is a complete NixOS `/nix/store` closure,
  not a Docker image. A closure is the guest system plus all its Nix
  dependencies.

The three execution modes are therefore:

| Mode | Where the agent runs | Project environment | Service ports |
| --- | --- | --- | --- |
| `host` | Directly on the daemon host | Host tools and the project's devenv | `REMOTER_AGENT_PORT_BASE` avoids collisions |
| `container` | Per-run Docker container | Per-project image from `.remoter/agent.Dockerfile` | Docker Postgres/MinIO sidecars use fixed loopback ports |
| `nspawn` | Per-run systemd-nspawn machine | Shared NixOS closure plus the project's devenv | The project's own services use its private network namespace |

Use nspawn mode when the daemon runs on NixOS and you want isolated runs
without maintaining large per-project Docker images. It still provides project
services through devenv; unlike Docker mode, it starts no Postgres or MinIO
sidecars.

### What happens during a run

For each agent run, the daemon performs the following steps:

1. It resolves `nspawn_closure_ref` and realizes the NixOS closure on the host.
   A per-run GC root prevents garbage collection while the machine uses it.
   Reusing an unchanged closure is a Nix cache hit, not an image rebuild.
2. The root-owned `remoter-nspawnctl` helper allocates a host-wide lease: a
   machine name (`rr-N`), a veth interface, and a /30 IPv4 block.
3. The helper starts `systemd-nspawn --boot --ephemeral`. The machine has a
   writable temporary root filesystem which disappears at teardown. Persistent
   data lives only in explicit host mounts:
   - the ticket worktree is writable at `/work`;
   - the shared repository's `.git` directory is mounted at its original host
     path so Git worktree metadata resolves correctly;
   - optional reference worktrees are read-only under `/work/.refs/`;
   - `agent_home` is writable at `/root`, preserving agent configuration,
     credentials, and sessions across machines;
   - host `/nix/store` and `/nix/var/nix/db` are read-only.
4. The helper configures the host side of the veth pair, NAT, and a firewall
   rule rejecting forwarding between run interfaces. The guest configures its
   own static address through systemd-networkd.
5. If the worktree uses devenv, the daemon first runs `devenv
   --no-eval-cache build` **on the host**. This realizes the project's shell
   and service packages into the shared host store. It then runs `devenv
   --no-eval-cache up -d` and `devenv processes wait --timeout 120` inside the
   machine.
6. The daemon starts the agent and `remoter-mcp` inside the machine through
   the helper's fixed `systemd-run -M <machine> --pipe` wrapper. Commands run
   as root in `/work`, normally inside `devenv shell --no-tui
   --no-eval-cache`. The NixOS guest enables Git's `safe.directory = *` because
   the mounted repositories belong to the daemon's host user.
7. At completion, failure, or cancellation, the daemon terminates the machine,
   waits for it to disappear, releases the lease, and removes the per-run GC
   root. Stopping the whole machine also stops its devenv services.

There is no silent fallback to host mode if closure realization, boot,
networking, or service startup fails.

Because the guest store and Nix database are read-only, **Nix cannot build or
substitute packages inside the machine**. This is intentional. The host
pre-warm must succeed before services or the agent start. If an agent changes
`devenv.nix` during a turn, testing the changed environment can require a new
run so that the daemon can pre-warm it on the host.

The guest receives `CI=1`, `REMOTER_EXECUTION=nspawn`, and
`REMOTER_AGENT_TASK_ID`. It does not receive `REMOTER_CONTAINER` or
`REMOTER_AGENT_PORT_BASE`: each machine has a private network namespace, so
projects can use their normal fixed service ports.

### Host requirements

The daemon host needs:

- NixOS with systemd **256 or newer** (`systemd-run -M --pipe` is required);
- `systemd-nspawn`, `systemd-run`, and `machinectl` on the daemon's `PATH`;
- `nix` and `nix-store`, with flakes enabled to build the default closure ref;
- `git` and `devenv` for worktrees and host-side project pre-warming;
- Python 3, `ip`, `iptables`, `sysctl`, and `flock` for the privileged helper;
- the agent CLI and `remoter-mcp` installed so their canonical executable paths
  are below `/nix/store` (see below);
- a root-owned helper, policy file, and a narrowly scoped sudo rule.

Docker is not used. NixOS declarative containers
(`boot.enableContainers`) are also unrelated and do not need to be enabled.

### NixOS host configuration

The following example assumes:

- a checkout of this repository at `/etc/remoter-agent`;
- a dedicated daemon user named `remoter`;
- workspace and agent state under `/var/lib/remoter-agent`;
- the default `10.231.0.0/16` run network pool.

Adjust all four choices to your installation. The checkout must exist before
evaluating the NixOS configuration.

```nix
{ pkgs, ... }:

let
  agentSrc = /etc/remoter-agent;
  nspawnctl = pkgs.writeShellScriptBin "remoter-nspawnctl"
    (builtins.readFile (agentSrc + "/agent-runner/scripts/remoter-nspawnctl"));
in
{
  users.users.remoter = {
    isSystemUser = true;
    group = "remoter";
    home = "/var/lib/remoter-agent";
  };
  users.groups.remoter = { };

  systemd.tmpfiles.rules = [
    "d /var/lib/remoter-agent 0750 remoter remoter -"
    "d /var/lib/remoter-agent/agent-home 0750 remoter remoter -"
  ];

  environment.systemPackages = [
    nspawnctl
    pkgs.python3
  ];

  # environment.etc installs this root-owned and not group/world-writable.
  # Paths must be absolute; '~' is not expanded by the helper.
  environment.etc."remoter-agent/nspawnctl.conf" = {
    mode = "0644";
    text = ''
      workspace_root=/var/lib/remoter-agent
      agent_home=/var/lib/remoter-agent/agent-home
      pool_base=10.231
    '';
  };

  # Do not grant sudo for systemd-nspawn, systemd-run, or machinectl directly.
  security.sudo.extraRules = [{
    users = [ "remoter" ];
    commands = [{
      command = "${nspawnctl}/bin/remoter-nspawnctl";
      options = [ "NOPASSWD" ];
    }];
  }];
}
```

Apply it and validate the helper as the daemon user:

```sh
sudo nixos-rebuild switch
sudo -u remoter -H sudo -n remoter-nspawnctl check
```

The helper uses `/run/remoter-nspawnctl` for leases and network drop-ins. Its
policy is a security boundary: worktree and repository bind sources must be
under `workspace_root`, and the agent home must exactly match `agent_home`.
The daemon, its worktrees, and the helper must therefore agree on the real,
expanded paths.

The sudo rule in this example references the exact Nix store helper installed
by the same configuration. If you update the helper through NixOS, apply
`nixos-rebuild switch` so the command and sudo rule change together.

### Daemon configuration

For the host setup above, use:

```toml
api_url = "http://127.0.0.1:8181"
workspace_root = "/var/lib/remoter-agent"
max_concurrent_runs = 4

[execution]
mode = "nspawn"
nspawn_closure_ref = "path:/etc/remoter-agent#agentContainer"
nspawn_net_pool_base = "10.231"
agent_home = "/var/lib/remoter-agent/agent-home"
sudo_binary = "sudo"

[driver]
kind = "kimi-acp"
agent_program = "kimi"
remoter_mcp_command = "remoter-mcp"
```

Configuration keys:

- `mode = "nspawn"` selects the nspawn runtime. The default is `host`.
- `nspawn_closure_ref` is either a flake reference with an output attribute,
  such as `path:/etc/remoter-agent#agentContainer`, or an already-built NixOS
  toplevel directory below `/nix/store`. The default is the former.
- `nspawn_net_pool_base` contains the first two IPv4 octets. It must equal the
  helper's `pool_base`. The default is `10.231`.
- `agent_home` is the persistent host directory mounted at `/root`. It defaults
  to `<workspace_root>/agent-home` and must match the helper policy.
- `sudo_binary` defaults to `sudo` and is used only with `-n
  remoter-nspawnctl ...`.

For a development checkout elsewhere, point both the NixOS `agentSrc` and the
TOML closure ref at that checkout. For example, from
`/home/tim/projects/remoter-agent`:

```toml
nspawn_closure_ref = "path:/home/tim/projects/remoter-agent#agentContainer"
```

To pre-build the closure manually from a checkout:

```sh
nix build .#agentContainer --no-link --print-out-paths
```

This step is optional; the daemon realizes the configured reference on demand.

### Agent CLI, MCP, and authentication

In addition to the explicit worktree and agent-home mounts, every guest sees
the host `/nix/store`. Consequently, in nspawn mode the daemon resolves both
`driver.agent_program` and `driver.remoter_mcp_command` on its own `PATH`,
canonicalizes them, and requires the resulting executables to be below
`/nix/store`.

A Cargo binary under `target/`, an npm installation under `$HOME`, or another
executable outside the Nix store will be rejected even though it works in host
mode. Install the agent CLI and `remoter-mcp` through Nix, or set the driver
keys to absolute executable store paths. The daemon itself does not have to be
a Nix-built binary.

For `kimi-acp`, authentication normally comes from the target project's
`.remoter/kimi-config.toml`. When this template exists, the daemon renders it
into `<agent_home>/.kimi-code/config.toml` and substitutes `${KIMI_API_KEY}`
from the daemon's environment. Export the key in the daemon's systemd unit or
environment file; never commit it to the template. Without a template, the
existing config in `agent_home` is left unchanged and must already contain
valid authentication. This is the same persistent agent-home mechanism used by
Docker mode.

Docker image settings and `nix_binary_cache_dir` do not configure nspawn mode.
The shared host store replaces the Docker image/cache mechanism.

### Networking and backend reachability

Lease slot `N` uses `<pool>.<4*N>.1` for the host address and
`<pool>.<4*N>.2` for the guest address. For example, slot 0 in the default pool
is `10.231.0.1` on the host and `10.231.0.2` in the guest. The helper supports
64 host-wide leases and prevents direct forwarding between run interfaces.

If `api_url` uses `localhost`, `127.0.0.1`, `[::1]`, or `0.0.0.0`, the daemon
rewrites it for guest processes to the run's host-side veth address. A backend
at `http://127.0.0.1:8181` on the host therefore becomes, for example,
`http://10.231.0.1:8181` inside the machine.

The backend must actually listen on an address reachable through that veth and
the host firewall must allow the connection. A backend bound only to the
host's loopback interface is not reachable at the rewritten address; bind it
to the veth address or an appropriate external/wildcard address. Non-loopback
API URLs are passed through unchanged. Guest DNS and outbound access use the
host-side NAT.

One daemon instance is allowed per `workspace_root`. Its identity and lock are
stored in `nspawn-instance-id` and `nspawn-instance.lock`. Keep the identity
file across restarts: startup reconciliation uses it to sweep only machines
and leases left by the same daemon, without touching other daemon instances.

### Verification and troubleshooting

Before starting the daemon:

```sh
systemd-run --version
sudo -u remoter -H sudo -n remoter-nspawnctl check
sudo -u remoter -H nix build \
  path:/etc/remoter-agent#agentContainer --no-link --print-out-paths
```

Start the daemon with the nspawn TOML and assign a small test ticket. While it
runs, `machinectl list` should show an `rr-N` machine. Cancel a run once and
confirm that the machine and its lease disappear. Restarting the daemon should
also reconcile any leases left by a previous crash of that same instance.

Common startup errors:

- **`driver.agent_program`/`remoter_mcp_command` is outside `/nix/store`:**
  install that program through Nix or configure its executable store path.
- **`remoter-nspawnctl check` fails:** run it as the daemon user; check the
  exact sudo rule, policy ownership/mode, required host tools, and that both
  policy directories already exist.
- **A bind source is outside the allowed root:** move the workspace/agent home
  or update all TOML and policy paths so their canonical locations match.
- **Pool does not match policy:** use the same two-octet value in
  `nspawn_net_pool_base` and `pool_base`.
- **Systemd is too old:** nspawn mode requires systemd 256 or newer; there is
  no fallback exec implementation.

Common run errors:

- **The guest cannot reach the backend:** check the rewritten host veth
  address, backend listen address, host firewall, and NAT.
- **devenv tries to build in the guest or the store is read-only:** inspect the
  preceding host `devenv build` failure. Builds inside the guest are disabled
  by design; changed dependencies must be pre-warmed on the host.
- **A machine fails to boot:** the daemon error includes the tail of its boot
  log. Check `machinectl` and the host NixOS/systemd logs for the named `rr-N`
  machine.
- **A lease remains after an error:** do not delete files under
  `/run/remoter-nspawnctl` or replace the daemon identity manually. Restart
  the same daemon instance so reconciliation can terminate the machine and
  release the lease safely; unknown machine-state errors deliberately retain
  the lease to avoid reallocating a network still in use.

## Configuration

- `remoter-agent` reads `remoter-agent.toml` (see the annotated
  [`agent-runner/remoter-agent.toml`](agent-runner/remoter-agent.toml)); the
  agent token comes from `REMOTER_AGENT_TOKEN` or the `token` key.
- `remoter-mcp` is configured purely through environment variables
  (`REMOTER_API_URL`, `REMOTER_TOKEN`, optional `REMOTER_WORKSPACE_ID`) — see
  [`mcp-server/README.md`](mcp-server/README.md).

## Relationship to Remoter

These tools are the agent-side half of the Remoter project. The backend they
talk to (HTTP API, Flutter UI, ticketing data model) lives in the private
Remoter monorepo; the protocol-level specs referenced throughout the code
(`docs/specs/remoter-agent.md`, `docs/specs/mcp-server-for-agents.md`) live
there too. This repository is standalone: it builds, tests, and installs
without any access to the monorepo.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at
your option.
