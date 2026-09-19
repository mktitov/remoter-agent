//! Shared version identity of the workspace binaries (`remoter-agent`,
//! `remoter-mcp`). Both crates include this file via `#[path]` so the startup
//! log fields, the `--version` output and the build-script fallback logic
//! stay in lockstep — one code path, one log contract.
//!
//! The git identity is baked in at compile time by each crate's `build.rs`:
//! nix builds inject `REMOTER_AGENT_GIT_REV` / `REMOTER_AGENT_GIT_COMMIT_DATE`
//! (flake.nix passes `self.rev` / `self.lastModifiedDate`), plain cargo builds
//! fall back to `git rev-parse HEAD` / `git log -1 --format=%cI`, and without
//! either the value is `"unknown"`. There is deliberately no build date: a
//! commit may be rebuilt many times, so only the commit identifies a binary.

/// Full git SHA of the commit this binary was built from.
pub const GIT_REV: &str = match option_env!("REMOTER_AGENT_GIT_REV") {
    Some(v) => v,
    None => "unknown",
};

/// Commit date of [`GIT_REV`] (ISO 8601 from git, flake timestamp from nix).
pub const GIT_COMMIT_DATE: &str = match option_env!("REMOTER_AGENT_GIT_COMMIT_DATE") {
    Some(v) => v,
    None => "unknown",
};

/// The one-line version string printed by `--version`.
pub fn line(binary: &str) -> String {
    format_line(binary, env!("CARGO_PKG_VERSION"), GIT_REV, GIT_COMMIT_DATE)
}

/// Pure formatting behind [`line`] (unit-tested).
pub fn format_line(binary: &str, pkg_version: &str, git_rev: &str, git_commit_date: &str) -> String {
    format!("{binary} {pkg_version} (git rev: {git_rev}, commit date: {git_commit_date})")
}

/// Resolve a version variable for the build script: an explicitly provided
/// value (nix build) wins; otherwise probe git; otherwise `"unknown"`.
// Used by each crate's `build.rs`, not by the binaries themselves.
#[allow(dead_code)]
pub fn resolve(var: &str, git_args: &[&str]) -> String {
    resolve_with(std::env::var(var).ok().as_deref(), "git", git_args)
}

fn resolve_with(provided: Option<&str>, git_program: &str, git_args: &[&str]) -> String {
    if let Some(v) = provided.filter(|v| !v.is_empty()) {
        return v.to_string();
    }
    git_output(git_program, git_args).unwrap_or_else(|| "unknown".to_string())
}

fn git_output(program: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(program).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if value.is_empty() { None } else { Some(value) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_line_contains_all_fields() {
        let line = format_line("remoter-agent", "0.1.0", "abc123", "2026-09-19T15:00:00+00:00");
        assert_eq!(
            line,
            "remoter-agent 0.1.0 (git rev: abc123, commit date: 2026-09-19T15:00:00+00:00)"
        );
    }

    #[test]
    fn line_uses_compile_time_identity() {
        let line = line("remoter-mcp");
        assert!(
            line.starts_with(&format!("remoter-mcp {} ", env!("CARGO_PKG_VERSION"))),
            "{line}"
        );
        assert!(line.contains(GIT_REV), "{line}");
        assert!(line.contains(GIT_COMMIT_DATE), "{line}");
    }

    #[test]
    fn resolve_prefers_the_provided_value() {
        assert_eq!(
            resolve_with(Some("deadbeef"), "git", &["rev-parse", "HEAD"]),
            "deadbeef"
        );
    }

    #[test]
    fn resolve_ignores_an_empty_provided_value() {
        assert_eq!(
            resolve_with(Some(""), "remoter-nonexistent-git-binary", &["rev-parse", "HEAD"]),
            "unknown"
        );
    }

    #[test]
    fn resolve_returns_unknown_without_git() {
        assert_eq!(
            resolve_with(None, "remoter-nonexistent-git-binary", &["rev-parse", "HEAD"]),
            "unknown"
        );
    }

    #[test]
    fn resolve_returns_unknown_when_git_fails() {
        assert_eq!(
            resolve_with(None, "git", &["rev-parse", "--verify", "refs/heads/definitely-missing"]),
            "unknown"
        );
    }

    #[test]
    fn resolve_never_returns_an_empty_string() {
        // In a git checkout this is the HEAD SHA; in the nix sandbox (no .git)
        // it is "unknown" — either way the value is usable in a log line.
        assert!(!resolve_with(None, "git", &["rev-parse", "HEAD"]).is_empty());
    }
}
