use std::path::PathBuf;

const APP_NAME: &str = "whitenoise-cli";
pub const KEYRING_SERVICE_ID: &str = "com.whitenoise.cli";
const SOCKET_FILENAME: &str = "wnd.sock";
const PID_FILENAME: &str = "wnd.pid";

/// Resolved configuration for the CLI daemon and client.
#[derive(Debug, Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    pub logs_dir: PathBuf,
}

impl Config {
    /// Build config by merging platform defaults with CLI overrides.
    pub fn resolve(data_dir: Option<&PathBuf>, logs_dir: Option<&PathBuf>) -> Self {
        let defaults = Self::platform_defaults();
        Self {
            data_dir: data_dir.cloned().unwrap_or(defaults.data_dir),
            logs_dir: logs_dir.cloned().unwrap_or(defaults.logs_dir),
        }
    }

    /// Effective socket path (inside the suffixed data dir).
    ///
    /// `WhitenoiseConfig::new` appends a `dev` or `release` suffix to the
    /// data dir, so the socket lives at e.g. `data_dir/dev/wnd.sock`.
    pub fn socket_path(&self) -> PathBuf {
        self.suffixed_data_dir().join(SOCKET_FILENAME)
    }

    /// Effective PID file path (inside the suffixed data dir).
    pub fn pid_path(&self) -> PathBuf {
        self.suffixed_data_dir().join(PID_FILENAME)
    }

    /// The data dir with the build-mode suffix that `WhitenoiseConfig::new` applies.
    fn suffixed_data_dir(&self) -> PathBuf {
        let suffix = if cfg!(debug_assertions) {
            "dev"
        } else {
            "release"
        };
        self.data_dir.join(suffix)
    }

    #[cfg(target_os = "macos")]
    fn platform_defaults() -> Self {
        let home = dirs::home_dir().expect("could not determine home directory");
        Self {
            data_dir: home
                .join("Library")
                .join("Application Support")
                .join(APP_NAME),
            logs_dir: home.join("Library").join("Logs").join(APP_NAME),
        }
    }

    #[cfg(target_os = "linux")]
    fn platform_defaults() -> Self {
        let data = dirs::data_dir().expect("could not determine data directory");
        Self {
            data_dir: data.join(APP_NAME),
            logs_dir: data.join(APP_NAME).join("logs"),
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    fn platform_defaults() -> Self {
        let data = dirs::data_dir().expect("could not determine data directory");
        Self {
            data_dir: data.join(APP_NAME),
            logs_dir: data.join(APP_NAME).join("logs"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_produce_valid_paths() {
        let config = Config::platform_defaults();
        assert!(config.data_dir.is_absolute());
        assert!(config.logs_dir.is_absolute());
        assert!(config.data_dir.to_string_lossy().contains("whitenoise-cli"));
    }

    #[test]
    fn cli_overrides_take_precedence() {
        let custom_data = PathBuf::from("/tmp/wn-data");
        let config = Config::resolve(Some(&custom_data), None);
        assert_eq!(config.data_dir, custom_data);
        // logs_dir should fall back to platform default
        assert!(config.logs_dir.is_absolute());
    }

    #[test]
    fn socket_path_includes_suffix() {
        let config = Config::resolve(Some(&PathBuf::from("/tmp/wn")), None);
        let socket = config.socket_path();
        // In debug builds, suffix is "dev"
        assert!(
            socket.to_string_lossy().contains("dev")
                || socket.to_string_lossy().contains("release")
        );
        assert!(socket.to_string_lossy().ends_with("wnd.sock"));
    }
}
