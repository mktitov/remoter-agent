//! `remoter-agent` — the daemon that polls the Remoter backend for tickets
//! assigned to its agent user, claims them, and drives a dev-agent through the
//! plan → plan-review → implement → review workflow (docs/specs/remoter-agent.md).
//!
//! The daemon talks to the backend only (REST + the WebSocket event stream,
//! spec §4.5): it holds no DB credentials and every state change goes through
//! the backend API, so the human-acceptance gates and claim guards are always
//! enforced (spec §2.2).

pub mod claim;
pub mod client;
pub mod config;
pub mod container;
pub mod driver;
pub mod events;
pub mod forge;
pub mod image;
pub mod logs;
pub mod logstore;
pub mod logstream;
pub mod ports;
pub mod review;
pub mod run;
pub mod session_log;
pub mod staging;
pub mod supervise;
pub mod sync;
// Shared with mcp-server: one version module for the whole workspace.
#[path = "../../version.rs"]
pub mod version;
pub mod workspace;

#[cfg(test)]
pub(crate) mod testutil {
    use std::io::Write;
    use std::path::Path;
    use std::process::{Command, Stdio};

    /// Writes `body` to `path` with mode 755 **through a child `sh` process**.
    ///
    /// Test stubs are exec'd directly afterwards. If the test process itself
    /// opened the file for writing (`std::fs::write`), a parallel test
    /// thread's `fork()` inside `Command::spawn` could inherit that write-fd,
    /// and the kernel would then reject execve of the file with ETXTBSY
    /// (rust-lang/rust#114554, ticket #218). Writing through a child process
    /// keeps any write-fd on the script out of this process entirely.
    pub(crate) fn write_executable_script(path: &Path, body: &str) {
        let mut child = Command::new("sh")
            .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
            .arg(path)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("failed to spawn sh to write {}: {e}", path.display()));
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(body.as_bytes()).unwrap();
        drop(stdin);
        let status = child.wait().unwrap();
        assert!(status.success(), "failed to write stub script {}", path.display());
    }
}
