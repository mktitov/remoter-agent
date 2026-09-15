# remoter-mcp

MCP server (stdio) that exposes a [Remoter](https://github.com/mktitov/remoter)
task board to AI agents and to human-driven MCP clients (Claude Code, Cursor,
…). It is a pure HTTP client of the Remoter backend: a URL and a bearer token
are all it needs — no database credentials, ever.

Tool surface (role-dependent): discover projects/features/tasks, list and
claim assigned tickets, advance board status, manage ticket actions
(checklists), comment, attach/read files, ask/answer ticket questions, submit
reviews, and file the implementation report.

## Installation

Nix flake (needs the `nix-command`/`flakes` features):

```sh
nix profile install github:mktitov/remoter-agent#remoter-mcp
# ad-hoc:
nix run github:mktitov/remoter-agent#remoter-mcp
```

Cargo:

```sh
cargo install --git https://github.com/mktitov/remoter-agent remoter-mcp
```

From source: `cargo build --release -p remoter-mcp` (binary at
`target/release/remoter-mcp`).

Consumer **devenv** projects can pull the binary in with one input:

```yaml
# devenv.yaml
inputs:
  remoter-agent:
    url: github:mktitov/remoter-agent
    inputs:
      nixpkgs:
        follows: nixpkgs
```

```nix
# devenv.nix
{ pkgs, inputs, ... }: {
  packages = [ inputs.remoter-agent.packages.${pkgs.system}.remoter-mcp ];
  env.REMOTER_API_URL = "http://localhost:8181";   # or https://remoter.example.com
  env.REMOTER_TOKEN   = "the-token";
}
```

## Configuration

Everything is passed via environment variables (the binary is spawned as a
child process by the MCP client) plus one optional flag:

| Variable / flag | Purpose | Example |
|---|---|---|
| `REMOTER_API_URL` | Base URL of the Remoter HTTP API (required) | `http://localhost:8181` |
| `REMOTER_TOKEN` | Bearer token: a personal access token, a login JWT, or an agent's static token (required) | `abC123...` |
| `REMOTER_AGENT_TOKEN` | Legacy alias for `REMOTER_TOKEN` (what the `remoter-agent` daemon sets); ignored when `REMOTER_TOKEN` is present | `abC123...` |
| `REMOTER_WORKSPACE_ID` | Workspace id sent as `X-Workspace-Id`. Required when the caller has multiple workspace memberships; optional with a single one | `1` |
| `REMOTER_AGENT_TASK_ID` | Current ticket id when spawned by `remoter-agent` (daemon mode); scopes task creation and supervise-role discovery | `42` |
| `REMOTER_MCP_NAME` | Server name reported in MCP `initialize` (optional) | `remoter` |
| `--role <role>` | Runtime role: `full`, `dev-agent-plan`, `dev-agent-implement`, `dev-agent-supervise`, `dev-agent-review` | `--role dev-agent-implement` |

On startup the server validates the token with one `GET /auth/me` call,
resolves the effective workspace, and caches the identity for `whoami`. The
protocol surface is MCP 2025-06-18 over stdio: `initialize`, `tools/list`,
`tools/call`, `ping` (no resources/prompts/notifications).

## Human usage (without `remoter-agent`)

Any stdio MCP client can spawn `remoter-mcp` with a human's personal access
token (PAT):

1. **Get a PAT.** In the Remoter Flutter app: profile dialog (account icon at
   the bottom of the navigation rail) → **MCP access token** → **Generate
   token** → **Copy**. The token is shown once; generating a new one revokes
   the previous. CLI equivalent:

   ```sh
   JWT=$(curl -s -X POST $API/api/v1/auth/login \
     -H 'content-type: application/json' \
     -d '{"email": "you@example.com", "password": "…"}' | jq -r .token)
   curl -s -X POST $API/api/v1/auth/token \
     -H "Authorization: Bearer $JWT" | jq -r .token
   ```

2. **Point the MCP client at the binary.** Claude Code:

   ```sh
   claude mcp add remoter \
     --env REMOTER_API_URL=http://localhost:8181 \
     --env REMOTER_TOKEN=<pat-from-step-1> \
     --env REMOTER_WORKSPACE_ID=1 \
     -- remoter-mcp
   ```

   Cursor (`.cursor/mcp.json` or Settings → MCP):

   ```json
   {
     "mcpServers": {
       "remoter": {
         "command": "remoter-mcp",
         "env": {
           "REMOTER_API_URL": "http://localhost:8181",
           "REMOTER_TOKEN": "<pat-from-step-1>",
           "REMOTER_WORKSPACE_ID": "1"
         }
       }
     }
   }
   ```

Humans get the same discovery tools as agents (`list_projects`,
`search_tasks`, `list_board`, …). Reading is workspace-wide for any
authenticated caller; writes keep the caller's role-based permissions.

## Tests

```sh
cargo test -p remoter-mcp   # unit tests + HTTP-level tool tests (wiremock)
```

## License

Dual-licensed under [MIT](../LICENSE-MIT) or [Apache-2.0](../LICENSE-APACHE)
at your option.
