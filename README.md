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
  executes inside a per-run container with postgres/minio sidecars.

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
