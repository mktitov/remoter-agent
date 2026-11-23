use std::path::{Path, PathBuf};

use crate::client::DiagnosticAction;
use crate::config::{Config, ExecutionMode};
use crate::container::{self, DoctorContainer};
use crate::workspace::{BwrapExec, ContainerExec, DoctorBwrap, ExecEnv};

pub struct DoctorRuntime {
    pub exec: ExecEnv,
    container: Option<DoctorContainer>,
}

impl DoctorRuntime {
    pub async fn teardown(self) {
        if let Some(container) = self.container {
            container.teardown().await;
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct WriteScope {
    pub writable_paths: Vec<PathBuf>,
}

impl WriteScope {
    pub fn validate(config: &Config, action: &DiagnosticAction) -> anyhow::Result<Self> {
        if !config.doctor.write_enabled {
            anyhow::bail!("Doctor write is disabled by configuration");
        }
        if config.execution.mode == ExecutionMode::Host {
            anyhow::bail!("Doctor host write is forbidden; use execution.mode = \"bwrap\" or \"container\"");
        }
        let source = config.doctor.source_dir.canonicalize().map_err(|error| {
            anyhow::anyhow!(
                "cannot canonicalize doctor.source_dir {}: {error}",
                config.doctor.source_dir.display()
            )
        })?;
        let roots = config
            .doctor
            .allowed_write_roots
            .iter()
            .map(|root| {
                root.canonicalize().map_err(|error| {
                    anyhow::anyhow!("cannot canonicalize Doctor write root {}: {error}", root.display())
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let mut writable_paths = Vec::new();
        for raw in &action.writable_paths {
            let path = PathBuf::from(raw);
            if !path.is_absolute() {
                anyhow::bail!("approved Doctor path {raw:?} is not absolute");
            }
            let canonical = canonical_target(&path)?;
            if !roots.iter().any(|root| canonical.starts_with(root)) {
                anyhow::bail!(
                    "approved Doctor path {} is outside allowed_write_roots",
                    canonical.display()
                );
            }
            if canonical.starts_with(&source) || source.starts_with(&canonical) {
                anyhow::bail!(
                    "approved Doctor path {} overlaps read-only source {}",
                    canonical.display(),
                    source.display()
                );
            }
            if !canonical.exists() {
                std::fs::create_dir_all(&canonical)?;
            }
            writable_paths.push(canonical);
        }
        writable_paths.sort();
        writable_paths.dedup();
        Ok(Self { writable_paths })
    }
}

fn canonical_target(path: &Path) -> anyhow::Result<PathBuf> {
    if path.exists() {
        return Ok(path.canonicalize()?);
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Doctor path {} has no parent", path.display()))?;
    let parent = parent.canonicalize()?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("Doctor path {} has no final component", path.display()))?;
    Ok(parent.join(name))
}

pub async fn start(
    config: &Config,
    attempt_id: i32,
    read_only: bool,
    scope: Option<&WriteScope>,
) -> anyhow::Result<DoctorRuntime> {
    let agent_home = config.execution.agent_home(&config.workspace_root);
    if config.driver.kind == "kimi-acp" {
        crate::driver::kimi_config::provision(&config.driver.kimi_config, &agent_home, attempt_id)
            .map_err(anyhow::Error::msg)?;
    }
    let writable_paths = scope.map(|scope| scope.writable_paths.clone()).unwrap_or_default();
    match config.execution.mode {
        ExecutionMode::Host => {
            if !read_only {
                anyhow::bail!("Doctor host write is forbidden");
            }
            Ok(DoctorRuntime {
                exec: ExecEnv::host(false),
                container: None,
            })
        }
        ExecutionMode::Bwrap => Ok(DoctorRuntime {
            exec: ExecEnv::Bwrap(BwrapExec {
                launcher: std::env::current_exe().unwrap_or_else(|_| "remoter-agent".into()),
                bwrap: config.execution.bwrap_binary.clone(),
                slirp4netns: config.execution.slirp4netns_binary.clone(),
                agent_home,
                state_root: config.workspace_root.join("bwrap"),
                devenv: false,
                retained_commands: Vec::new(),
                sessions_dir: config
                    .driver
                    .sessions_dir
                    .clone()
                    .unwrap_or_else(|| config.workspace_root.join("agent-home/.kimi-code/sessions")),
                api_url: crate::bwrap::api_url(&config.api_url),
                doctor: Some(DoctorBwrap {
                    writable_paths,
                    read_only_paths: Vec::new(),
                }),
            }),
            container: None,
        }),
        ExecutionMode::Container => {
            let image = config.doctor.container_image.as_deref().ok_or_else(|| {
                anyhow::anyhow!("doctor.container_image is required when execution.mode = \"container\"")
            })?;
            let container = container::start_doctor(container::DoctorContainerSpec {
                docker: &config.execution.docker_binary,
                attempt_id,
                image,
                source_dir: &config.doctor.source_dir,
                agent_home: &agent_home,
                writable_paths: &writable_paths,
                read_only_paths: &[],
            })
            .await
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            Ok(DoctorRuntime {
                exec: ExecEnv::Container(ContainerExec {
                    docker: config.execution.docker_binary.clone(),
                    container: container.name().to_string(),
                    work_dir: PathBuf::from(container::WORK_DIR),
                    devenv: false,
                    doctor: true,
                    sessions_dir: config
                        .driver
                        .sessions_dir
                        .clone()
                        .unwrap_or_else(|| config.workspace_root.join("agent-home/.kimi-code/sessions")),
                    api_url: container::container_api_url(&config.api_url),
                }),
                container: Some(container),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(dir: &Path, mode: &str, source: &Path) -> Config {
        let toml = format!(
            "api_url = \"http://localhost\"\nworkspace_root = \"{}\"\n\n[execution]\nmode = \"{}\"\n\n[doctor]\nwrite_enabled = true\nsource_dir = \"{}\"\nallowed_write_roots = [\"{}\"]\n\n[driver]\nkind = \"stub\"",
            dir.display(),
            mode,
            source.display(),
            dir.display(),
        );
        Config::from_toml_str(&toml, Some("token".to_string())).unwrap()
    }

    #[test]
    fn host_write_is_forbidden_before_sandbox_start() {
        let dir = std::env::temp_dir().join(format!("remoter-doctor-host-write-{}", std::process::id()));
        let source = dir.join("source");
        std::fs::create_dir_all(&source).unwrap();
        let config = config(&dir, "host", &source);
        let action = DiagnosticAction {
            writable_paths: vec![dir.join("scratch").to_string_lossy().into_owned()],
            ..Default::default()
        };
        let error = WriteScope::validate(&config, &action).unwrap_err().to_string();
        assert!(error.contains("host write is forbidden"), "{error}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn write_scope_accepts_only_canonical_paths_below_approved_roots() {
        let dir = std::env::temp_dir().join(format!("remoter-doctor-write-scope-{}", std::process::id()));
        let source = dir.join("source");
        let scratch = dir.join("scratch");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&scratch).unwrap();
        let config = config(&dir, "bwrap", &source);
        let action = DiagnosticAction {
            writable_paths: vec![scratch.to_string_lossy().into_owned()],
            ..Default::default()
        };
        let scope = WriteScope::validate(&config, &action).unwrap();
        assert_eq!(scope.writable_paths, vec![scratch.canonicalize().unwrap()]);
        let outside = DiagnosticAction {
            writable_paths: vec!["/tmp".to_string()],
            ..Default::default()
        };
        assert!(WriteScope::validate(&config, &outside).is_err());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn source_can_never_become_writable_through_scope_or_parent_mount() {
        let dir = std::env::temp_dir().join(format!("remoter-doctor-source-ro-{}", std::process::id()));
        let source = dir.join("source");
        std::fs::create_dir_all(&source).unwrap();
        let config = config(&dir, "bwrap", &source);
        for path in [source.clone(), dir.clone()] {
            let action = DiagnosticAction {
                writable_paths: vec![path.to_string_lossy().into_owned()],
                ..Default::default()
            };
            let error = WriteScope::validate(&config, &action).unwrap_err().to_string();
            assert!(error.contains("read-only source"), "{error}");
        }
        std::fs::remove_dir_all(dir).ok();
    }
}
