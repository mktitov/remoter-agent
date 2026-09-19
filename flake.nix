# Nix flake exposing the `remoter-mcp` and `remoter-agent` binaries as
# consumable packages. Installation is host-only: the daemon host pulls both
# in as a flake input (target projects need no remoter-specific setup — the
# daemon injects remoter-mcp into sessions by absolute path):
#
#   # host devenv.yaml:
#   inputs:
#     remoter-agent:
#       url: github:mktitov/remoter-agent
#       inputs:
#         nixpkgs:
#           follows: nixpkgs
#
#   # host devenv.nix:
#   { pkgs, inputs, ... }: {
#     packages = with inputs.remoter-agent.packages.${pkgs.system}; [
#       remoter-mcp
#       remoter-agent
#     ];
#     env.REMOTER_API_URL = "http://localhost:8181";
#     env.REMOTER_AGENT_TOKEN = "the-agent-token";
#   }
#
# Both binaries are HTTP clients of the Remoter backend — no DB credentials
# needed. They link no sqlx/repo code; the builds are self-contained.
#
# Runtime prerequisites NOT covered by this flake (install them yourself):
#   - `kimi` or `opencode` CLI on PATH — the ACP agent the daemon shells out to;
#   - `git` and `devenv` — the daemon clones repos and brings project devenv
#     services up/down during runs;
#   - `docker` — only for the daemon's container execution mode.
{
  description = "Remoter agent tooling — MCP server and the remoter-agent daemon";

  inputs = {
    nixpkgs.url = "github:cachix/devenv-nixpkgs/rolling";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane = {
      url = "github:ipetkov/crane";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      rust-overlay,
      crane,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        toolchain = pkgs.rust-bin.stable.latest.default;
        craneLib = (crane.mkLib pkgs).overrideToolchain toolchain;

        # Common arguments for remoter-mcp (both deps and package)
        remoter-mcp-common = {
          pname = "remoter-mcp";
          version = "0.1.0";
          src = craneLib.path ./.;
          nativeBuildInputs = [
            pkgs.stdenv.cc
            pkgs.pkg-config
          ] ++ pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.glibc ];
          buildInputs = [ pkgs.openssl ];   # required by some dev-dependencies
          env = {
            CC = "${pkgs.stdenv.cc}/bin/cc";
            # Version identity for the startup log / --version (see version.rs
            # and the crates' build.rs). `self.rev`/`self.lastModifiedDate`
            # also exist for github:-input sources, where .git is absent.
            REMOTER_AGENT_GIT_REV = self.rev or "dirty";
            REMOTER_AGENT_GIT_COMMIT_DATE = self.lastModifiedDate or "unknown";
          } // (pkgs.lib.optionalAttrs (pkgs.stdenv.isLinux && pkgs.stdenv.hostPlatform.isx86_64) {
            CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER = "${pkgs.stdenv.cc}/bin/cc";
          });
        };

        remoter-mcp-deps = craneLib.buildDepsOnly remoter-mcp-common;

        remoter-mcp = craneLib.buildPackage (remoter-mcp-common // {
          cargoArtifacts = remoter-mcp-deps;
          cargoExtraArgs = "-p remoter-mcp";
          cargoBuildCommand = "cargo build --release -p remoter-mcp --bin remoter-mcp";
        });

        # Common arguments for remoter-agent (both deps and package)
        remoter-agent-common = {
          pname = "remoter-agent";
          version = "0.1.0";
          src = craneLib.path ./.;
          cargoExtraArgs = "-p remoter-agent";
          nativeBuildInputs = [
            pkgs.pkg-config
            pkgs.stdenv.cc
          ] ++ pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.glibc ];
          buildInputs = [ pkgs.openssl ];
          env = {
            CC = "${pkgs.stdenv.cc}/bin/cc";
            # Version identity for the startup log / --version (see version.rs
            # and the crates' build.rs). `self.rev`/`self.lastModifiedDate`
            # also exist for github:-input sources, where .git is absent.
            REMOTER_AGENT_GIT_REV = self.rev or "dirty";
            REMOTER_AGENT_GIT_COMMIT_DATE = self.lastModifiedDate or "unknown";
          } // (pkgs.lib.optionalAttrs (pkgs.stdenv.isLinux && pkgs.stdenv.hostPlatform.isx86_64) {
            CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER = "${pkgs.stdenv.cc}/bin/cc";
          });
        };

        remoter-agent-deps = craneLib.buildDepsOnly remoter-agent-common;

        remoter-agent = craneLib.buildPackage (
          remoter-agent-common
          // {
            cargoArtifacts = remoter-agent-deps;
            cargoBuildCommand = "cargo build --release -p remoter-agent --bin remoter-agent";
            nativeCheckInputs = [ pkgs.git pkgs.python3 ];
            cargoTestCommand = "cargo test --release -p remoter-agent --lib --test acp_driver --test acp_kimi --test acp_opencode";
          }
        );
      in
      {
        packages = {
          remoter-mcp = remoter-mcp;
          remoter-agent = remoter-agent;
          default = remoter-mcp;
        };

        apps = {
          remoter-mcp = flake-utils.lib.mkApp {
            drv = remoter-mcp;
          };
          remoter-agent = flake-utils.lib.mkApp {
            drv = remoter-agent;
          };
        };

        # `nix flake check` builds both packages (including the agent's test
        # suite via cargoTestCommand).
        checks = {
          inherit remoter-mcp remoter-agent;
        };
      }
    );
}
