use std::path::Path;

const API_KEY_PLACEHOLDER: &str = "${KIMI_API_KEY}";

pub fn provision(source: &Path, agent_home: &Path, run_id: i32) -> Result<(), String> {
    let key = std::env::var("KIMI_API_KEY").ok().filter(|k| !k.is_empty());
    provision_with(source, agent_home, run_id, key.as_deref())
}

fn provision_with(source: &Path, agent_home: &Path, run_id: i32, api_key: Option<&str>) -> Result<(), String> {
    let text = std::fs::read_to_string(source)
        .map_err(|e| format!("cannot read configured Kimi config {}: {e}", source.display()))?;
    let rendered = if text.contains(API_KEY_PLACEHOLDER) {
        let key = api_key.ok_or_else(|| {
            format!(
                "{} contains {API_KEY_PLACEHOLDER} but KIMI_API_KEY is not set in the daemon's environment",
                source.display()
            )
        })?;
        text.replace(API_KEY_PLACEHOLDER, key)
    } else {
        text
    };
    let dir = agent_home.join(".kimi-code");
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let target = dir.join("config.toml");
    let tmp = dir.join(format!(".config.toml.tmp-{run_id}"));
    std::fs::write(&tmp, rendered).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("cannot chmod {}: {e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, &target).map_err(|e| format!("cannot install {}: {e}", target.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provision_substitutes_the_api_key() {
        let dir = std::env::temp_dir().join(format!("remoter-kimi-prov-{}", std::process::id()));
        let source = dir.join("kimi-config.toml");
        let home = dir.join("agent-home");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&source, "api_key = \"${KIMI_API_KEY}\"\n").unwrap();
        provision_with(&source, &home, 1, Some("sk-test")).unwrap();
        let rendered = std::fs::read_to_string(home.join(".kimi-code/config.toml")).unwrap();
        assert_eq!(rendered, "api_key = \"sk-test\"\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(home.join(".kimi-code/config.toml"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn provision_missing_key_is_an_error() {
        let dir = std::env::temp_dir().join(format!("remoter-kimi-prov-err-{}", std::process::id()));
        let source = dir.join("kimi-config.toml");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&source, "api_key = \"${KIMI_API_KEY}\"\n").unwrap();
        let err = provision_with(&source, &dir.join("agent-home"), 1, None).unwrap_err();
        assert!(err.contains("KIMI_API_KEY"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn provision_missing_source_is_an_error() {
        let dir = std::env::temp_dir().join(format!("remoter-kimi-prov-none-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("missing.toml");
        let err = provision_with(&source, &dir.join("agent-home"), 1, None).unwrap_err();
        assert!(err.contains(&source.display().to_string()), "{err}");
        assert!(!dir.join("agent-home/.kimi-code/config.toml").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parallel_provisions_use_independent_temporary_files() {
        let dir = std::env::temp_dir().join(format!("remoter-kimi-prov-parallel-{}", std::process::id()));
        let source = dir.join("kimi-config.toml");
        let home = dir.join("agent-home");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&source, "default_model = \"kimi-code/k3\"\n").unwrap();
        std::thread::scope(|scope| {
            for run_id in [1, 2] {
                let source = &source;
                let home = &home;
                scope.spawn(move || provision_with(source, home, run_id, None).unwrap());
            }
        });
        let rendered = std::fs::read_to_string(home.join(".kimi-code/config.toml")).unwrap();
        assert_eq!(rendered, "default_model = \"kimi-code/k3\"\n");
        std::fs::remove_dir_all(&dir).ok();
    }
}
