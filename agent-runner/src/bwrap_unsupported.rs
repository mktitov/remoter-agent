use std::path::Path;

use anyhow::bail;

use crate::driver::DriverError;

pub const INTERNAL_LAUNCH_ARG: &str = "__bwrap-launch";

const UNSUPPORTED: &str = "bubblewrap execution is supported only on Linux";

pub fn api_url(input: &str) -> String {
    input.to_string()
}

pub fn run_env(task_id: i32) -> Vec<(String, String)> {
    vec![
        ("CI".to_string(), "1".to_string()),
        ("REMOTER_EXECUTION".to_string(), "bwrap".to_string()),
        ("REMOTER_AGENT_TASK_ID".to_string(), task_id.to_string()),
    ]
}

pub async fn prewarm(
    _worktree: &Path,
    _agent_home: &Path,
    _env: &[(String, String)],
    _env_wrap: &[String],
) -> Result<(), DriverError> {
    Err(DriverError::Permanent(UNSUPPORTED.to_string()))
}

pub fn sweep(_state_root: &Path) {}

pub fn validate_prerequisites(_bwrap: &str, _slirp4netns: &str) -> anyhow::Result<()> {
    bail!(UNSUPPORTED)
}

pub fn launch_from_args(_args: &[String]) -> anyhow::Result<i32> {
    bail!(UNSUPPORTED)
}
