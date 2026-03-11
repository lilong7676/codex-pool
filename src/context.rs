use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use anyhow::Context;
use anyhow::Result;

use crate::models::AppConfig;
use crate::models::UsageSnapshot;
use crate::utils::set_private_permissions;

#[derive(Debug, Clone)]
pub enum MockUsageResponse {
    Snapshot(UsageSnapshot),
    Error(String),
}

#[derive(Debug, Clone, Default)]
pub struct TestHooks {
    pub mock_usage: HashMap<String, MockUsageResponse>,
}

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub home_dir: PathBuf,
    pub data_dir: PathBuf,
    pub store_path: PathBuf,
    pub config_path: PathBuf,
    pub live_auth_path: PathBuf,
    pub codex_config_path: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Result<Self> {
        let home_dir = dirs::home_dir().context("failed to resolve HOME")?;
        Ok(Self::for_home(home_dir))
    }

    pub fn for_home(home_dir: PathBuf) -> Self {
        let data_dir = home_dir.join(".codex-pool");
        let store_path = data_dir.join("accounts.json");
        let config_path = data_dir.join("config.toml");
        let live_auth_path = home_dir.join(".codex").join("auth.json");
        let codex_config_path = home_dir.join(".codex").join("config.toml");

        Self {
            home_dir,
            data_dir,
            store_path,
            config_path,
            live_auth_path,
            codex_config_path,
        }
    }

    pub fn legacy_store_candidates(&self) -> Vec<PathBuf> {
        let mut candidates = Vec::new();

        #[cfg(target_os = "macos")]
        {
            candidates.push(
                self.home_dir
                    .join("Library")
                    .join("Application Support")
                    .join("com.carry.codex-tools")
                    .join("accounts.json"),
            );
        }

        #[cfg(all(unix, not(target_os = "macos")))]
        {
            if let Some(xdg_data_home) = env::var_os("XDG_DATA_HOME") {
                candidates.push(
                    PathBuf::from(xdg_data_home)
                        .join("com.carry.codex-tools")
                        .join("accounts.json"),
                );
            }
            candidates.push(
                self.home_dir
                    .join(".local")
                    .join("share")
                    .join("com.carry.codex-tools")
                    .join("accounts.json"),
            );
        }

        candidates
    }
}

#[derive(Debug, Clone)]
pub struct AppContext {
    pub paths: AppPaths,
    pub codex_cli_path: Option<PathBuf>,
    pub test_hooks: TestHooks,
}

impl AppContext {
    pub fn discover() -> Result<Self> {
        let paths = AppPaths::discover()?;
        Ok(Self {
            codex_cli_path: find_codex_cli_path(),
            paths,
            test_hooks: TestHooks::default(),
        })
    }

    pub fn with_paths(paths: AppPaths) -> Self {
        Self {
            codex_cli_path: find_codex_cli_path(),
            paths,
            test_hooks: TestHooks::default(),
        }
    }

    pub fn ensure_layout(&self) -> Result<()> {
        fs::create_dir_all(&self.paths.data_dir)
            .with_context(|| format!("failed to create {}", self.paths.data_dir.display()))?;
        if let Some(parent) = self.paths.live_auth_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if !self.paths.config_path.exists() {
            self.save_config(&AppConfig::default())?;
        }
        Ok(())
    }

    pub fn load_config(&self) -> Result<AppConfig> {
        if !self.paths.config_path.exists() {
            return Ok(AppConfig::default());
        }

        let raw = fs::read_to_string(&self.paths.config_path)
            .with_context(|| format!("failed to read {}", self.paths.config_path.display()))?;
        toml::from_str(&raw)
            .with_context(|| format!("invalid config file {}", self.paths.config_path.display()))
    }

    pub fn save_config(&self, config: &AppConfig) -> Result<()> {
        let serialized = toml::to_string_pretty(config).context("failed to serialize config")?;
        fs::write(&self.paths.config_path, serialized)
            .with_context(|| format!("failed to write {}", self.paths.config_path.display()))?;
        set_private_permissions(&self.paths.config_path);
        Ok(())
    }

    pub fn new_codex_command(&self) -> Result<Command> {
        let codex_path = self
            .codex_cli_path
            .clone()
            .ok_or_else(|| anyhow::anyhow!("codex executable not found in PATH"))?;

        let mut command = Command::new(&codex_path);
        if let Some(parent) = codex_path.parent() {
            let merged_path = if let Some(current_path) = env::var_os("PATH") {
                let path_entries = std::iter::once(parent.to_path_buf())
                    .chain(env::split_paths(&current_path))
                    .collect::<Vec<_>>();
                env::join_paths(path_entries).context("failed to build PATH")?
            } else {
                env::join_paths([parent]).context("failed to build PATH")?
            };
            command.env("PATH", merged_path);
        }

        Ok(command)
    }

    pub fn resolve_legacy_store_path(&self, explicit: Option<&Path>) -> Option<PathBuf> {
        if let Some(path) = explicit {
            return path.exists().then(|| path.to_path_buf());
        }

        self.paths
            .legacy_store_candidates()
            .into_iter()
            .find(|candidate| candidate.exists())
    }
}

fn find_codex_cli_path() -> Option<PathBuf> {
    let mut candidates = Vec::new();

    if let Some(path_os) = env::var_os("PATH") {
        for dir in env::split_paths(&path_os) {
            candidates.push(dir.join("codex"));
        }
    }

    if let Some(home) = dirs::home_dir() {
        for dir in [
            home.join(".local").join("bin"),
            home.join(".npm-global").join("bin"),
            home.join(".volta").join("bin"),
            home.join(".asdf").join("shims"),
            home.join(".pnpm"),
            home.join("Library").join("pnpm"),
            home.join("bin"),
        ] {
            candidates.push(dir.join("codex"));
        }
    }

    #[cfg(target_os = "macos")]
    {
        for dir in [
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/usr/local/bin"),
            PathBuf::from("/usr/bin"),
        ] {
            candidates.push(dir.join("codex"));
        }
    }

    for candidate in candidates {
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }

    None
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };

    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }

    #[cfg(not(unix))]
    {
        true
    }
}
