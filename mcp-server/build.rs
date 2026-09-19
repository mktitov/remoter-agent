//! Bake the git identity into the binary (see `version.rs` at the workspace
//! root): a nix build injects `REMOTER_AGENT_GIT_REV` /
//! `REMOTER_AGENT_GIT_COMMIT_DATE` from flake.nix; a plain cargo build falls
//! back to `git`; without either the value is `"unknown"`.
#[allow(dead_code)]
#[path = "../version.rs"]
mod version;

fn main() {
    println!("cargo:rerun-if-env-changed=REMOTER_AGENT_GIT_REV");
    println!("cargo:rerun-if-env-changed=REMOTER_AGENT_GIT_COMMIT_DATE");
    println!(
        "cargo:rustc-env=REMOTER_AGENT_GIT_REV={}",
        version::resolve("REMOTER_AGENT_GIT_REV", &["rev-parse", "HEAD"])
    );
    println!(
        "cargo:rustc-env=REMOTER_AGENT_GIT_COMMIT_DATE={}",
        version::resolve("REMOTER_AGENT_GIT_COMMIT_DATE", &["log", "-1", "--format=%cI"])
    );
}
