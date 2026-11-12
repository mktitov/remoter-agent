//! Versioned per-project execution configuration from `.remoter/agent.toml`.

use std::path::Path;

use serde::Deserialize;

use crate::config::ExecutionMode;
use crate::driver::DriverError;

pub const CONFIG_REL: &str = ".remoter/agent.toml";

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields, rename_all = "snake_case")]
pub struct ProjectConfig {
    pub driver: Option<String>,
    pub commands: RawProjectCommands,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields, rename_all = "snake_case")]
pub struct RawProjectCommands {
    pub env_wrap: Option<Vec<String>>,
    pub services_up: Option<Vec<String>>,
    pub services_wait: Option<Vec<String>>,
    pub services_down: Option<Vec<String>>,
    pub image_init: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectCommands {
    pub env_wrap: Vec<String>,
    pub services_up: Vec<String>,
    pub services_wait: Vec<String>,
    pub services_down: Vec<String>,
    pub image_init: Vec<String>,
    /// Lazy legacy detection executed only inside the bwrap guest.
    pub processes_detect: Vec<String>,
    pub services_up_if_processes: bool,
    pub services_wait_if_processes: bool,
    /// Legacy container `just db-up` needs the project devenv environment;
    /// explicitly configured service commands leave this empty.
    pub services_up_wrap: Vec<String>,
}

impl ProjectConfig {
    pub fn parse(text: &str) -> Result<Self, DriverError> {
        let config: Self =
            toml::from_str(text).map_err(|e| DriverError::Permanent(format!("invalid {CONFIG_REL}: {e}")))?;
        if let Some(kind) = config.driver.as_deref()
            && !matches!(kind, "stub" | "kimi-acp" | "opencode-acp")
        {
            return Err(DriverError::Permanent(format!(
                "invalid {CONFIG_REL}: unknown driver kind {kind:?} (expected \"stub\", \"kimi-acp\", or \"opencode-acp\")"
            )));
        }
        for (name, command) in [
            ("env_wrap", &config.commands.env_wrap),
            ("services_up", &config.commands.services_up),
            ("services_wait", &config.commands.services_wait),
            ("services_down", &config.commands.services_down),
            ("image_init", &config.commands.image_init),
        ] {
            validate_command(name, command.as_deref())?;
        }
        Ok(config)
    }

    pub fn from_file(repo: &Path) -> Result<Self, DriverError> {
        let path = repo.join(CONFIG_REL);
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(DriverError::Permanent(format!("cannot read {}: {e}", path.display()))),
        }
    }

    /// Reads the configuration from a freshly fetched remote ref, never from
    /// the central clone's shared checkout (which other runs may advance).
    pub async fn from_git(repo: &Path, base_branch: Option<&str>) -> Result<Self, DriverError> {
        let base = match base_branch {
            Some(base) => base.to_string(),
            None => crate::workspace::remote_default_branch(repo)
                .await
                .map_err(|e| DriverError::Transient(format!("resolve project config base: {e}")))?,
        };
        crate::workspace::fetch_branch(repo, &base)
            .await
            .map_err(|e| DriverError::Transient(format!("fetch project config base {base}: {e}")))?;
        let spec = format!("origin/{base}:{CONFIG_REL}");
        let output = tokio::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["show", &spec])
            .output()
            .await
            .map_err(|e| DriverError::Transient(format!("read project config from {spec}: {e}")))?;
        if !output.status.success() {
            if output.status.code() == Some(128) {
                return Ok(Self::default());
            }
            return Err(DriverError::Permanent(format!(
                "cannot read {spec}: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        let text = String::from_utf8(output.stdout)
            .map_err(|e| DriverError::Permanent(format!("invalid UTF-8 in {spec}: {e}")))?;
        Self::parse(&text)
    }

    pub async fn commands_for(
        &self,
        mode: ExecutionMode,
        worktree: &Path,
        env: &[(String, String)],
    ) -> Result<ProjectCommands, DriverError> {
        self.commands_for_with_detector(mode, worktree, || legacy_has_processes(worktree, env))
            .await
    }

    async fn commands_for_with_detector<F, Fut>(
        &self,
        mode: ExecutionMode,
        worktree: &Path,
        detect_processes: F,
    ) -> Result<ProjectCommands, DriverError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<bool, DriverError>>,
    {
        let raw = &self.commands;
        let devenv = uses_devenv(worktree);
        let needs_process_detection = devenv && (raw.services_up.is_none() || raw.services_wait.is_none());
        let legacy_processes = match mode {
            ExecutionMode::Host if needs_process_detection => detect_processes().await?,
            _ => false,
        };
        let env_wrap = raw.env_wrap.clone().unwrap_or_else(|| {
            if devenv {
                match mode {
                    ExecutionMode::Bwrap => argv(&["devenv", "--no-eval-cache", "shell", "--no-tui", "--"]),
                    ExecutionMode::Host | ExecutionMode::Container => {
                        argv(&["devenv", "shell", "--no-tui", "--no-eval-cache", "--"])
                    }
                }
            } else {
                Vec::new()
            }
        });
        let services_up = raw.services_up.clone().unwrap_or_else(|| match mode {
            ExecutionMode::Host if legacy_processes => argv(&["devenv", "--no-eval-cache", "up", "-d"]),
            ExecutionMode::Bwrap if devenv => argv(&["devenv", "--no-eval-cache", "up", "-d"]),
            ExecutionMode::Container if devenv && has_justfile(worktree) => argv(&["just", "db-up"]),
            _ => Vec::new(),
        });
        let services_wait = raw.services_wait.clone().unwrap_or_else(|| match mode {
            ExecutionMode::Host if legacy_processes => {
                argv(&["devenv", "--no-eval-cache", "processes", "wait", "--timeout", "120"])
            }
            ExecutionMode::Bwrap if devenv => {
                argv(&["devenv", "--no-eval-cache", "processes", "wait", "--timeout", "120"])
            }
            _ => Vec::new(),
        });
        let services_down = raw.services_down.clone().unwrap_or_else(|| match mode {
            ExecutionMode::Host | ExecutionMode::Bwrap if devenv => {
                argv(&["devenv", "--no-eval-cache", "processes", "down"])
            }
            _ => Vec::new(),
        });
        let processes_detect = if mode == ExecutionMode::Bwrap && needs_process_detection {
            argv(&["devenv", "--no-eval-cache", "eval", "processes"])
        } else {
            Vec::new()
        };
        let services_up_if_processes = mode == ExecutionMode::Bwrap && devenv && raw.services_up.is_none();
        let services_wait_if_processes = mode == ExecutionMode::Bwrap && devenv && raw.services_wait.is_none();
        let services_up_wrap =
            if mode == ExecutionMode::Container && raw.services_up.is_none() && devenv && has_justfile(worktree) {
                argv(&["devenv", "shell", "--no-tui", "--no-eval-cache", "--"])
            } else {
                Vec::new()
            };
        Ok(ProjectCommands {
            env_wrap,
            services_up,
            services_wait,
            services_down,
            image_init: self.image_init(worktree),
            processes_detect,
            services_up_if_processes,
            services_wait_if_processes,
            services_up_wrap,
        })
    }

    pub fn image_init(&self, repo: &Path) -> Vec<String> {
        self.commands.image_init.clone().unwrap_or_else(|| {
            let mut command = argv(&["devenv", "shell", "--no-tui", "--no-eval-cache", "--"]);
            if repo.join(".remoter/agent-init.sh").is_file() {
                command.extend(argv(&["sh", "/opt/agent-init.sh"]));
            } else {
                command.push("true".to_string());
            }
            command
        })
    }

    pub fn legacy_services_down(&self, mode: ExecutionMode, worktree: &Path) -> Vec<String> {
        self.commands.services_down.clone().unwrap_or_else(|| {
            if mode != ExecutionMode::Container && uses_devenv(worktree) {
                argv(&["devenv", "--no-eval-cache", "processes", "down"])
            } else {
                Vec::new()
            }
        })
    }
}

fn validate_command(name: &str, command: Option<&[String]>) -> Result<(), DriverError> {
    let Some(command) = command else {
        return Ok(());
    };
    if command.first().is_some_and(|program| program.trim().is_empty()) {
        return Err(DriverError::Permanent(format!(
            "invalid {CONFIG_REL}: commands.{name} has an empty executable"
        )));
    }
    if command.iter().any(|arg| arg.contains('\0')) {
        return Err(DriverError::Permanent(format!(
            "invalid {CONFIG_REL}: commands.{name} contains a NUL byte"
        )));
    }
    Ok(())
}

fn argv(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

pub fn uses_devenv(dir: &Path) -> bool {
    dir.join("devenv.nix").exists() || dir.join("devenv.yaml").exists()
}

pub fn has_justfile(worktree: &Path) -> bool {
    ["justfile", "Justfile", ".justfile"]
        .iter()
        .any(|file| worktree.join(file).exists())
}

async fn legacy_has_processes(dir: &Path, env: &[(String, String)]) -> Result<bool, DriverError> {
    let what = "devenv --no-eval-cache eval processes";
    let output = tokio::process::Command::new("devenv")
        .args(["--no-eval-cache", "eval", "processes"])
        .current_dir(dir)
        .env("CI", "1")
        .env("DEVENV_NO_AI_AGENT", "1")
        .envs(env.iter().map(|(key, value)| (key.as_str(), value.as_str())))
        .output()
        .await
        .map_err(|e| DriverError::Transient(format!("{what}: {e}")))?;
    if !output.status.success() {
        return Err(DriverError::Transient(format!(
            "{what} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let evaluated: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| DriverError::Transient(format!("{what}: invalid JSON output: {e}")))?;
    let processes = evaluated
        .get("processes")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| DriverError::Transient(format!("{what}: missing processes object")))?;
    Ok(!processes.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "remoter-project-config-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[tokio::test]
    async fn from_git_reads_fetched_ref_without_touching_shared_checkout() {
        let dir = TestDir::new();
        let seed = dir.path().join("seed");
        let clone = dir.path().join("clone");
        std::fs::create_dir_all(seed.join(".remoter")).unwrap();
        git(&seed, &["init", "-b", "main"]);
        git(&seed, &["config", "user.email", "test@example.com"]);
        git(&seed, &["config", "user.name", "Test"]);
        std::fs::write(seed.join(CONFIG_REL), "driver = \"kimi-acp\"").unwrap();
        git(&seed, &["add", ".remoter/agent.toml"]);
        git(&seed, &["commit", "-m", "old config"]);
        git(dir.path(), &["clone", seed.to_str().unwrap(), clone.to_str().unwrap()]);
        std::fs::write(seed.join(CONFIG_REL), "driver = \"opencode-acp\"").unwrap();
        git(&seed, &["add", ".remoter/agent.toml"]);
        git(&seed, &["commit", "-m", "new config"]);

        let config = ProjectConfig::from_git(&clone, Some("main")).await.unwrap();
        assert_eq!(config.driver.as_deref(), Some("opencode-acp"));
        assert_eq!(
            std::fs::read_to_string(clone.join(CONFIG_REL)).unwrap(),
            "driver = \"kimi-acp\"",
            "the shared checkout must not be advanced by run configuration loading"
        );
    }

    #[test]
    fn missing_empty_and_commandless_files_preserve_defaults() {
        let dir = TestDir::new();
        assert_eq!(ProjectConfig::from_file(dir.path()).unwrap(), ProjectConfig::default());
        std::fs::create_dir(dir.path().join(".remoter")).unwrap();
        let path = dir.path().join(CONFIG_REL);
        std::fs::write(&path, "").unwrap();
        assert_eq!(ProjectConfig::from_file(dir.path()).unwrap(), ProjectConfig::default());
        std::fs::write(&path, "driver = \"opencode-acp\"").unwrap();
        let config = ProjectConfig::from_file(dir.path()).unwrap();
        assert_eq!(config.driver.as_deref(), Some("opencode-acp"));
        assert_eq!(config.commands, RawProjectCommands::default());
    }

    #[test]
    fn partial_and_disabled_commands_are_distinct() {
        let partial =
            ProjectConfig::parse("[commands]\nservices_up = [\"just\", \"db-up\"]\nservices_wait = []").unwrap();
        assert_eq!(partial.commands.services_up, Some(argv(&["just", "db-up"])));
        assert_eq!(partial.commands.services_wait, Some(Vec::new()));
        assert_eq!(partial.commands.env_wrap, None);
        assert_eq!(partial.commands.services_down, None);
        assert_eq!(partial.commands.image_init, None);
    }

    #[test]
    fn unknown_fields_drivers_and_invalid_commands_are_permanent() {
        for text in [
            "unknown = true",
            "[commands]\nunknown = []",
            "driver = \"other\"",
            "[commands]\nenv_wrap = [\"\"]",
            "[commands]\nenv_wrap = [\"   \"]",
            "[commands]\nservices_up = \"devenv up\"",
            "[commands]\nservices_down = [\"sh\", \"\\u0000\"]",
        ] {
            assert!(
                matches!(ProjectConfig::parse(text), Err(DriverError::Permanent(_))),
                "expected a permanent error for {text:?}"
            );
        }
        assert!(ProjectConfig::parse("[commands]\nservices_up = [\"sh\", \"\"]").is_ok());
    }

    #[tokio::test]
    async fn no_devenv_disables_mode_specific_environment_and_services() {
        for mode in [ExecutionMode::Host, ExecutionMode::Container, ExecutionMode::Bwrap] {
            let dir = TestDir::new();
            let commands = ProjectConfig::default()
                .commands_for(mode, dir.path(), &[])
                .await
                .unwrap();
            assert!(commands.env_wrap.is_empty());
            assert!(commands.services_up.is_empty());
            assert!(commands.services_wait.is_empty());
            assert!(commands.services_down.is_empty());
        }
    }

    #[tokio::test]
    async fn every_missing_field_inherits_container_defaults() {
        let dir = TestDir::new();
        std::fs::write(dir.path().join("devenv.nix"), "").unwrap();
        std::fs::write(dir.path().join("justfile"), "").unwrap();
        std::fs::create_dir(dir.path().join(".remoter")).unwrap();
        std::fs::write(dir.path().join(".remoter/agent-init.sh"), "").unwrap();
        let commands = ProjectConfig::default()
            .commands_for(ExecutionMode::Container, dir.path(), &[])
            .await
            .unwrap();
        assert_eq!(
            commands,
            ProjectCommands {
                env_wrap: argv(&["devenv", "shell", "--no-tui", "--no-eval-cache", "--"]),
                services_up: argv(&["just", "db-up"]),
                services_wait: vec![],
                services_down: vec![],
                image_init: argv(&[
                    "devenv",
                    "shell",
                    "--no-tui",
                    "--no-eval-cache",
                    "--",
                    "sh",
                    "/opt/agent-init.sh",
                ]),
                processes_detect: vec![],
                services_up_if_processes: false,
                services_wait_if_processes: false,
                services_up_wrap: argv(&["devenv", "shell", "--no-tui", "--no-eval-cache", "--"]),
            }
        );
    }

    #[tokio::test]
    async fn container_legacy_up_requires_devenv_and_a_justfile() {
        for devenv in [false, true] {
            let dir = TestDir::new();
            if devenv {
                std::fs::write(dir.path().join("devenv.yaml"), "").unwrap();
            }
            let commands = ProjectConfig::default()
                .commands_for(ExecutionMode::Container, dir.path(), &[])
                .await
                .unwrap();
            assert!(commands.services_up.is_empty());
            assert!(commands.services_wait.is_empty());
            assert!(commands.services_down.is_empty());
        }
    }

    #[tokio::test]
    async fn explicit_container_commands_and_empty_arrays_are_used_verbatim() {
        let dir = TestDir::new();
        std::fs::write(dir.path().join("devenv.nix"), "").unwrap();
        let config = ProjectConfig::parse(
            "[commands]\nenv_wrap = []\nservices_up = [\"./up\"]\nservices_wait = []\nservices_down = []\nimage_init = []",
        )
        .unwrap();
        let commands = config
            .commands_for(ExecutionMode::Container, dir.path(), &[])
            .await
            .unwrap();
        assert_eq!(
            commands,
            ProjectCommands {
                env_wrap: vec![],
                services_up: argv(&["./up"]),
                services_wait: vec![],
                services_down: vec![],
                image_init: vec![],
                processes_detect: vec![],
                services_up_if_processes: false,
                services_wait_if_processes: false,
                services_up_wrap: vec![],
            }
        );
    }

    #[tokio::test]
    async fn host_defaults_follow_detected_processes() {
        let dir = TestDir::new();
        std::fs::write(dir.path().join("devenv.yaml"), "").unwrap();
        let commands = ProjectConfig::default()
            .commands_for_with_detector(ExecutionMode::Host, dir.path(), || async { Ok(true) })
            .await
            .unwrap();
        assert_eq!(
            commands.env_wrap,
            argv(&["devenv", "shell", "--no-tui", "--no-eval-cache", "--"])
        );
        assert_eq!(commands.services_up, argv(&["devenv", "--no-eval-cache", "up", "-d"]));
        assert_eq!(
            commands.services_wait,
            argv(&["devenv", "--no-eval-cache", "processes", "wait", "--timeout", "120"])
        );
        assert_eq!(
            commands.services_down,
            argv(&["devenv", "--no-eval-cache", "processes", "down"])
        );

        let commands = ProjectConfig::default()
            .commands_for_with_detector(ExecutionMode::Host, dir.path(), || async { Ok(false) })
            .await
            .unwrap();
        assert!(commands.services_up.is_empty());
        assert!(commands.services_wait.is_empty());
        assert_eq!(
            commands.services_down,
            argv(&["devenv", "--no-eval-cache", "processes", "down"])
        );
    }

    #[tokio::test]
    async fn bwrap_defaults_are_lazy_in_guest() {
        let dir = TestDir::new();
        std::fs::write(dir.path().join("devenv.yaml"), "").unwrap();
        let commands = ProjectConfig::default()
            .commands_for_with_detector(ExecutionMode::Bwrap, dir.path(), || async {
                Err(DriverError::Permanent(
                    "host process detection is forbidden".to_string(),
                ))
            })
            .await
            .unwrap();
        assert_eq!(
            commands.env_wrap,
            argv(&["devenv", "--no-eval-cache", "shell", "--no-tui", "--"])
        );
        assert_eq!(
            commands.processes_detect,
            argv(&["devenv", "--no-eval-cache", "eval", "processes"])
        );
        assert_eq!(commands.services_up, argv(&["devenv", "--no-eval-cache", "up", "-d"]));
        assert_eq!(
            commands.services_wait,
            argv(&["devenv", "--no-eval-cache", "processes", "wait", "--timeout", "120"])
        );
        assert!(commands.services_up_if_processes && commands.services_wait_if_processes);
        assert_eq!(
            commands.services_down,
            argv(&["devenv", "--no-eval-cache", "processes", "down"])
        );
    }

    #[tokio::test]
    async fn missing_host_fields_inherit_independently() {
        let dir = TestDir::new();
        std::fs::write(dir.path().join("devenv.nix"), "").unwrap();
        let custom_up = ProjectConfig::parse("[commands]\nservices_up = [\"custom-up\"]").unwrap();
        let commands = custom_up
            .commands_for_with_detector(ExecutionMode::Host, dir.path(), || async { Ok(true) })
            .await
            .unwrap();
        assert_eq!(commands.services_up, argv(&["custom-up"]));
        assert_eq!(
            commands.services_wait,
            argv(&["devenv", "--no-eval-cache", "processes", "wait", "--timeout", "120"])
        );
        assert_eq!(
            commands.services_down,
            argv(&["devenv", "--no-eval-cache", "processes", "down"])
        );
        assert_eq!(
            commands.env_wrap,
            argv(&["devenv", "shell", "--no-tui", "--no-eval-cache", "--"])
        );

        let disabled_wait = ProjectConfig::parse("[commands]\nservices_wait = []").unwrap();
        let commands = disabled_wait
            .commands_for_with_detector(ExecutionMode::Bwrap, dir.path(), || async { Ok(true) })
            .await
            .unwrap();
        assert_eq!(commands.services_up, argv(&["devenv", "--no-eval-cache", "up", "-d"]));
        assert!(commands.services_wait.is_empty());
        assert_eq!(
            commands.services_down,
            argv(&["devenv", "--no-eval-cache", "processes", "down"])
        );
    }

    #[tokio::test]
    async fn explicit_host_service_commands_do_not_eval_processes() {
        let dir = TestDir::new();
        std::fs::write(dir.path().join("devenv.nix"), "").unwrap();
        let config = ProjectConfig::parse(
            "[commands]\nservices_up = [\"definitely-not-devenv\"]\nservices_wait = []\nservices_down = []",
        )
        .unwrap();
        let commands = config
            .commands_for_with_detector(ExecutionMode::Host, dir.path(), || async {
                Err(DriverError::Permanent("unexpected process detection".to_string()))
            })
            .await
            .unwrap();
        assert_eq!(commands.services_up, argv(&["definitely-not-devenv"]));
        assert!(commands.services_wait.is_empty());
        assert!(commands.services_down.is_empty());
    }

    #[test]
    fn image_init_missing_script_warms_with_true_and_empty_disables_it() {
        let dir = TestDir::new();
        assert_eq!(
            ProjectConfig::default().image_init(dir.path()),
            argv(&["devenv", "shell", "--no-tui", "--no-eval-cache", "--", "true"])
        );
        let config = ProjectConfig::parse("[commands]\nimage_init = []").unwrap();
        assert!(config.image_init(dir.path()).is_empty());
    }
}
