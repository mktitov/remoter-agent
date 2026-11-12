# remoter-agent

Agent tooling for [Remoter](https://github.com/mktitov/remoter) — a kanban
where AI agents are first-class team members. This repository ships the two
binaries that connect a dev-agent to a Remoter backend:

- **`remoter-agent`** ([`agent-runner/`](agent-runner/)) — the headless daemon
  that polls the Remoter backend, claims tickets assigned to its agent user,
  and executes them with a dev-agent (kimi or opencode over ACP) in per-ticket
  git worktrees — on the host, in per-run Docker containers, or in rootless
  Bubblewrap sandboxes on NixOS.
- **`remoter-mcp`** ([`mcp-server/`](mcp-server/)) — the MCP server (stdio)
  that exposes the Remoter board to agents and to human-driven MCP clients
  (Claude Code, Cursor, …): list/claim tickets, manage actions, comment,
  attach files, report — plus the built-in project-setup guide
  (`get_project_setup_guide`).

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
- installing and configuring the `remoter-agent` daemon (the rootless NixOS
  Bubblewrap mode is documented in [`agent-runner/README.md`](agent-runner/README.md#bubblewrap-mode-rootless-nixos-isolation));
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
- **`devenv`** — the legacy-default project environment and service lifecycle;
  projects can replace or disable these commands in `.remoter/agent.toml`;
- **`docker`** — only for `[execution] mode = "container"`, where every run
  executes inside a per-run container with postgres/minio sidecars;
- **`bubblewrap` + `slirp4netns`** — only for `[execution] mode = "bwrap"` on
  NixOS: rootless process/filesystem/network isolation without Docker images or
  sidecars; see the [configuration and architecture guide](agent-runner/README.md#bubblewrap-mode-rootless-nixos-isolation).

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
- `agent.toml` — optional project execution configuration for every mode and
  the image build (see [Configuration](#configuration));
- `check-image.sh` — per-turn freshness hook: marks the image stale when
  `main` moved since the image was baked;
- `seed-nix-cache.sh` — optional host-side seeding hook for the daemon's
  shared nix binary cache (`[execution] nix_binary_cache_dir`): before a
  bake, it exports the agent-profile closure at the pinned rev and the
  devshell closure from the host's warm store into the cache, so the init
  container substitutes instead of rebuilding;

The image tag is content-keyed over every file in `.remoter/`, so any edit
there rebuilds the image automatically on the next run.

## Configuration

- `remoter-agent` reads `remoter-agent.toml` (see the annotated
  [`agent-runner/remoter-agent.toml`](agent-runner/remoter-agent.toml)); the
  agent token comes from `REMOTER_AGENT_TOKEN` or the `token` key.
- Each project may version `.remoter/agent.toml` to override the daemon's
  driver kind (`stub`, `kimi-acp`, or `opencode-acp`) and define argv-only
  environment, service lifecycle, and image initialization commands. Missing
  fields inherit legacy behavior independently; `[]` explicitly disables a
  command. The daemon reads the file from the synced central clone and applies
  it to plan/implement/supervise/review and image builds. Invalid TOML, unknown
  fields or drivers, and invalid argv are permanent errors; never put secrets
  in this file. See the [full contract, inheritance table, examples, and
  execution order](agent-runner/README.md#project-execution-configuration-remoteragenttoml).
- `remoter-mcp` is configured purely through environment variables
  (`REMOTER_API_URL`, `REMOTER_TOKEN`, optional `REMOTER_WORKSPACE_ID`) — see
  [`mcp-server/README.md`](mcp-server/README.md).

### Project execution overrides

A partial `.remoter/agent.toml` changes only the fields it declares:

```toml
[commands]
services_up = ["just", "db-up"]
services_wait = []
```

Here `just db-up` also performs the readiness check. Every other field keeps its
legacy default independently; an empty array disables only that step.

| Missing field | Legacy inheritance |
|---|---|
| `driver` | Daemon `[driver].kind`. |
| `env_wrap` | Auto-detected devenv shell for the selected host/container/bwrap mode, or direct execution without devenv. |
| `services_up` | Host/bwrap: `devenv up -d` when processes are configured. Container: `just db-up` for a devenv project with a justfile. |
| `services_wait` | Host/bwrap: `devenv processes wait --timeout 120` when processes are configured. Container: no separate wait. |
| `services_down` | Best-effort `devenv processes down` on host/bwrap. |
| `image_init` | Legacy devenv shell plus `agent-init.sh` when present, otherwise a devenv warm with `true`. |

The [full contract and mode-specific execution
order](agent-runner/README.md#project-execution-configuration-remoteragenttoml)
includes the complete argv example and bwrap pre-warm rules.

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
