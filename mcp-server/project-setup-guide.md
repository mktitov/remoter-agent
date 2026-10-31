# Preparing a project repository for remoter

This guide is the authoritative checklist for tickets about onboarding a
project into remoter — making its repository workable by the `remoter-agent`
daemon. Work through it top to bottom: human-only steps first (park the
ticket with `request_human_action` when one is missing), then the repository
files you create yourself, then the final checklist.

The reference implementation of every rule below is the `remoter-agent`
repository itself (`mktitov/remoter-agent`): its `AGENTS.md`, `devenv.nix`,
and `.remoter/` directory are working examples.

## 1. Human-only steps (use `request_human_action`)

These settings live outside the repository — in the Remoter UI/backend or in
the daemon's own config — so only a human can change them. When one is
missing, call the `request_human_action` MCP tool with a precise, actionable
description and end your turn; the ticket waits in the `waiting` status until
the human confirms.

- **Project `repo_url` and `base_branch`** — set in the Remoter UI (project
  settings). A ticket is only executable when its project has `repo_url`
  set; the daemon clones that URL and branches ticket worktrees off
  `base_branch`.
- **Forge configuration** — `forge_kind` (e.g. `github`, `gitlab`),
  `forge_api_url` (optional; the forge's default API base is used when
  unset), and `forge_token`. Without `forge_kind` the daemon skips push/PR
  entirely — runs happen, but no branch is pushed and no PR/MR is opened.
- **Ticket assignment** — a ticket only runs when it is assigned to the
  agent's user in the UI.
- **Daemon-side knobs** — `remoter-agent.toml` belongs to the human running
  the daemon: `[execution] mode` (`host` / `container`) and
  `max_concurrent_runs_per_project` (set it to `1` when the project cannot
  honor the port contract in §2.2 — such projects are serialized instead).

## 2. Repository files the agent creates

### 2.1 `AGENTS.md`

The entry point every agent run reads first. It must state, for this
repository:

- how to build, and the exact check commands that mirror CI (format, lints,
  tests) — the agent runs them before finishing every ticket;
- anything environment-specific: required tools, services that must be up,
  test suites that are opt-in or need credentials.

The checks must pass with no LLM and no human credentials — an agent run is
fully autonomous.

### 2.2 `devenv.nix` / `devenv.yaml` and the port contract

The daemon enters the ticket worktree through
`devenv shell --no-eval-cache` and brings project services up with
`devenv --no-eval-cache up -d`. So the project needs a working
[devenv](https://devenv.sh) shell carrying its toolchain and services.

Parallel tickets of one project collide on hardcoded service ports (two
Postgreses, one port), so the daemon allocates a unique **port block per
run** — blocks of 50 ports — and exports `REMOTER_AGENT_PORT_BASE` (plus
`REMOTER_AGENT_TASK_ID` and `CI=1`) into every devenv invocation. The project
must parameterize its service ports from that variable, keeping the
historical fixed port as the human default:

```nix
let
  agentPortBase = builtins.getEnv "REMOTER_AGENT_PORT_BASE";
  dbPort = if agentPortBase != "" then (pkgs.lib.toInt agentPortBase) + 1 else 5435;
in
  # … services.postgres.settings.port = dbPort;
```

A project that cannot parameterize its ports does not get parallel runs: the
human sets `max_concurrent_runs_per_project = 1` in `remoter-agent.toml`
(§1).

### 2.3 `.mcp.json` rules

A repository-level `.mcp.json` gives agent runs extra MCP servers (the
daemon merges it with the built-in remoter server):

- `${WORKTREE}` in `command` / `args` / `env` / `url` / header values expands
  to the ticket worktree path as the agent sees it — use it instead of
  absolute paths;
- entries pointing at fixed localhost ports must honor the port contract
  (§2.2) or they collide across parallel runs;
- when `.mcp.json` defines its own `remoter-mcp` server (for human MCP
  clients such as Claude Code or Cursor), bind the repository to its remoter
  project via `"env": { "REMOTER_PROJECT_ID": "<project-id>" }`: at startup
  remoter-mcp validates the id against the backend, `current_project`
  reports the binding, and every tool with an optional `projectId` defaults
  to it. A project id is not a secret;
- **never commit tokens or other secrets** — `.mcp.json` is committed to the
  repository.

### 2.4 `.remoter/` — the container-mode contract

With `[execution] mode = "container"` every run executes inside a per-run
docker container built from the project's own image context, `.remoter/`:

- `agent.Dockerfile` — **required**; a project without it is a hard error in
  container mode (no silent fallback to host mode). It bootstraps the agent
  environment (typically nix + devenv + the agent CLI);
- `agent-init.sh` — optional, runs once per image build (e.g. installs a
  package profile and the pinned agent CLI);
- `check-image.sh` — optional per-turn freshness hook with an exit-code
  contract: `0` = image is fresh, `42` = stale (rebuild), anything else =
  failed check (the cached image is kept — fail-open);
- `seed-nix-cache.sh` — optional host-side hook that seeds the daemon's
  shared nix binary cache before a bake; fail-open like `check-image.sh`;
- `kimi-config.toml` — optional agent CLI config template rendered into the
  container's agent home at every start. It must stay **secret-free**:
  `${KIMI_API_KEY}` is substituted from the daemon's environment at container
  start — never commit a real key;
- `staging.toml` — optional staging-environment definition (validated when
  present; an enabled one requires a start command and at least one route).

The image tag is content-keyed over **every file** in `.remoter/`, so any
edit there rebuilds the image automatically on the next run. In container
mode the postgres/minio sidecars share the run container's network namespace
(`127.0.0.1:5432` / `127.0.0.1:9000`), so the §2.2 port contract does not
apply inside containers and `REMOTER_AGENT_PORT_BASE` is never exported
there.

## 3. Final checklist

Before finishing the ticket:

- [ ] §1 settings are in place (or the ticket is parked with
      `request_human_action` listing exactly what is missing);
- [ ] `AGENTS.md`, `devenv.nix`/`devenv.yaml`, and (if needed) `.mcp.json`
      and `.remoter/` are committed;
- [ ] the project's checks are green **in the ticket environment** — run
      them exactly as the daemon would (inside the devenv shell of a ticket
      worktree, honoring `REMOTER_AGENT_PORT_BASE`);
- [ ] end-to-end: the daemon can claim a ticket of this project, work it,
      and open a PR.

## 4. Full guides

The complete user guides (in Russian) live at
[aiborda.ru/docs](https://aiborda.ru/docs/): setting up a project for
`remoter-agent` in host and container modes, installing and configuring the
daemon, and using `remoter-mcp` standalone. This guide is the short
authoritative checklist; the site is the long-form reference.
