# remoter-agent — agent notes

Rust workspace: `agent-runner/` (the `remoter-agent` daemon) + `mcp-server/`
(`remoter-mcp`). The root flake ships both as nix packages. Design decisions
live in the Remoter monorepo's `docs/specs/` (`remoter-agent.md`,
`remoter-agent-containers.md`, `mcp-server-for-agents.md`) — specs win over
intuition, but this repo builds and tests standalone: never require monorepo
access to finish a ticket here.

## Checks (run before finishing a ticket)

You are already inside `devenv shell --no-eval-cache` of the ticket worktree
when a remoter-agent daemon runs you (stable Rust toolchain + clippy/rustfmt,
git, python3, openssl — nothing else to install). Mirror CI
(`.github/workflows/ci.yml`) exactly:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

`cargo test --workspace` needs no LLM or credentials (stub driver + fake ACP
agent). The live conformance suites are `#[ignore]`-gated and opt-in —
`--test acp_kimi` needs an authenticated kimi CLI at exactly
`PINNED_KIMI_VERSION` (`agent-runner/tests/acp_kimi.rs`), `--test
acp_opencode` an authenticated opencode. Skip them unless the ticket touches
the ACP driver, and never bump a pinned version without re-running its suite.

### Running inside a remoter-agent container

When the daemon runs you in container mode (`execution.mode = "container"`,
docs/specs/remoter-agent-containers.md), the environment differs from host
mode:

- `REMOTER_CONTAINER=1` is set; there is **no** `REMOTER_AGENT_PORT_BASE`.
  Postgres/MinIO sidecars share your network namespace
  (`127.0.0.1:5432`/`127.0.0.1:9000`) — this repo's tests need neither;
  the exported `*_DATABASE_URL` vars are irrelevant here.
- Your cwd is the ticket worktree mounted at `/work`; the shared clone's
  `.git` is mounted at its original host path, so plain git works.
- The daemon's host is reachable as `host.docker.internal` (e.g.
  `REMOTER_API_URL` for remoter-mcp already points there).
- Git operations that touch origin (fetch/push) are done by the daemon on the
  host after your turn — never push yourself unless told to.

## Agent container image (`.remoter/`)

`.remoter/` is the build context of this project's own agent image — what
the daemon builds to run this repo's tickets in container mode (see README §
"Agent container image (`.remoter/`)"). Its tag is content-keyed over
**every file** in `.remoter/`, so any edit rebuilds the image on the next
run:

- To change what run containers have on PATH, edit
  `.remoter/agent-configuration.nix` — the Dockerfile is only the nix+devenv
  bootstrap. Do not add openssh: it collides with the base image's own ssh
  element on `libexec/ssh-keysign`.
- `.remoter/kimi-config.toml` must stay secret-free — `${KIMI_API_KEY}` is
  substituted from the daemon's environment at container start.
- `.remoter/check-image.sh` keeps the documented exit-code contract
  (0 fresh / 42 stale / other = failed check, cache kept).
- `.remoter/seed-nix-cache.sh` is the optional host-side seeding hook for the
  daemon's shared nix binary cache (`[execution] nix_binary_cache_dir`): it
  exports the agent-profile closure at `REMOTER_IMAGE_FLAKE_REV` plus the
  devshell closure into `REMOTER_NIX_CACHE_DIR`. Fail-open like
  check-image.sh — a failing hook just means a slower bake.
