{ pkgs, ... }:

{
  # Stable toolchain, mirroring CI (dtolnay/rust-toolchain@stable with
  # clippy + rustfmt) and the flake's rust-bin.stable.latest.default.
  languages.rust = {
    enable = true;
    channel = "stable";
    components = [
      "rustc"
      "cargo"
      "clippy"
      "rustfmt"
      "rust-analyzer"
    ];
  };

  packages = with pkgs; [
    pkg-config
    openssl # required by some dev-dependencies (same as the flake's buildInputs)
    git # remoter-agent tests drive real git clones/worktrees
    python3 # remoter-agent test fixtures (flake nativeCheckInputs)
  ];
}
