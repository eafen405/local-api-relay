use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    env,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

pub const APPLICATION_NAME: &str = "local-api-relay";
pub const DEFAULT_PORT: u16 = 8787;

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub data_dir: PathBuf,
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServiceSettings {
    pub port: u16,
}

impl AppPaths {
    pub fn discover() -> Result<Self> {
        Ok(Self {
            data_dir: xdg_path("XDG_DATA_HOME", ".local/share")?.join(APPLICATION_NAME),
            config_dir: xdg_path("XDG_CONFIG_HOME", ".config")?.join(APPLICATION_NAME),
            state_dir: xdg_path("XDG_STATE_HOME", ".local/state")?.join(APPLICATION_NAME),
        })
    }

    pub fn database_path(&self) -> PathBuf {
        self.data_dir.join("relay.sqlite3")
    }

    pub fn backup_dir(&self) -> PathBuf {
        self.data_dir.join("backups")
    }

    fn settings_path(&self) -> PathBuf {
        self.config_dir.join("service.json")
    }

    pub fn prepare(&self) -> Result<()> {
        ensure_private_directory(&self.data_dir)?;
        ensure_private_directory(&self.config_dir)?;
        ensure_private_directory(&self.state_dir)?;
        Ok(())
    }

    pub fn load_or_create_settings(&self) -> Result<ServiceSettings> {
        let path = self.settings_path();
        if !path.exists() {
            let settings = ServiceSettings { port: DEFAULT_PORT };
            write_private_file(&path, &serde_json::to_vec_pretty(&settings)?)?;
            return Ok(settings);
        }

        restrict_file(&path)?;
        let contents = fs::read(&path).context("could not read process configuration")?;
        let settings: ServiceSettings =
            serde_json::from_slice(&contents).context("process configuration is not valid JSON")?;
        validate_port(settings.port)?;
        Ok(settings)
    }
}

pub fn validate_port(port: u16) -> Result<()> {
    if port == 0 {
        bail!("port must be between 1 and 65535");
    }
    Ok(())
}

fn xdg_path(variable: &str, fallback_relative_to_home: &str) -> Result<PathBuf> {
    match env::var_os(variable) {
        Some(value) => {
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                bail!("{variable} must be an absolute path");
            }
            Ok(path)
        }
        None => {
            let home =
                env::var_os("HOME").context("HOME is required when XDG directories are unset")?;
            Ok(PathBuf::from(home).join(fallback_relative_to_home))
        }
    }
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("could not create {}", path.display()))?;
    #[cfg(unix)]
    {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("could not secure {}", path.display()))?;
    }
    Ok(())
}

pub fn restrict_file(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("could not secure {}", path.display()))?;
    }
    Ok(())
}

fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("could not create {}", path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("could not write {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("could not sync {}", path.display()))?;
    restrict_file(path)
}
