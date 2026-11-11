//! Cross-platform OS integration, application configuration directories,
//! and FFmpeg/ffprobe binary discovery for CutDub.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, info, warn};

#[derive(Debug, Error)]
pub enum PlatformError {
    #[error("FFmpeg binary not found. Looked in PATH and standard system directories.")]
    FfmpegNotFound,
    #[error("ffprobe binary not found. Looked in PATH and standard system directories.")]
    FfprobeNotFound,
    #[error("Failed to execute binary '{path}': {source}")]
    ExecutionFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Binary '{path}' exited with non-zero status")]
    NonZeroExit { path: PathBuf },
    #[error("Configuration error: {0}")]
    Config(String),
}

/// Discovered and verified paths + versions for `ffmpeg`, `ffprobe`, and optional external encoders (such as `vvencapp`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FfmpegBinaries {
    pub ffmpeg_path: PathBuf,
    pub ffprobe_path: PathBuf,
    pub ffmpeg_version: String,
    pub ffprobe_version: String,
    #[serde(default)]
    pub vvencapp_path: Option<PathBuf>,
    #[serde(default)]
    pub vvencapp_version: Option<String>,
}

impl FfmpegBinaries {
    /// Discover `ffmpeg`, `ffprobe`, and optional `vvencapp` using user-configured overrides,
    /// environment variables (`CUTDUB_FFMPEG` / `CUTDUB_FFPROBE` / `CUTDUB_VVENCAPP`), `PATH`,
    /// and standard platform locations.
    pub fn discover(
        explicit_ffmpeg: Option<&Path>,
        explicit_ffprobe: Option<&Path>,
    ) -> Result<Self, PlatformError> {
        let ffmpeg_path = resolve_binary("ffmpeg", "CUTDUB_FFMPEG", explicit_ffmpeg)
            .ok_or(PlatformError::FfmpegNotFound)?;
        let ffprobe_path = resolve_binary("ffprobe", "CUTDUB_FFPROBE", explicit_ffprobe)
            .ok_or(PlatformError::FfprobeNotFound)?;

        let ffmpeg_version = query_version(&ffmpeg_path)?;
        let ffprobe_version = query_version(&ffprobe_path)?;

        let mut vvencapp_path = resolve_binary("vvencapp", "CUTDUB_VVENCAPP", None);
        if vvencapp_path.is_none() {
            if let Some(parent) = ffmpeg_path.parent() {
                let sibling = parent.join(binary_executable_name("vvencapp"));
                if is_executable_file(&sibling) {
                    vvencapp_path = Some(sibling);
                }
            }
        }
        let vvencapp_version = vvencapp_path
            .as_deref()
            .and_then(|p| query_version_flag(p, "--version").ok());
        // A file on PATH can have missing shared libraries or be non-functional.
        if vvencapp_version.is_none() {
            vvencapp_path = None;
        }

        info!(
            ffmpeg = %ffmpeg_path.display(),
            ffprobe = %ffprobe_path.display(),
            vvencapp = ?vvencapp_path,
            version = %ffmpeg_version,
            "Discovered FFmpeg toolchain"
        );

        Ok(Self {
            ffmpeg_path,
            ffprobe_path,
            ffmpeg_version,
            ffprobe_version,
            vvencapp_path,
            vvencapp_version,
        })
    }
}

fn binary_executable_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

fn resolve_binary(
    base_name: &str,
    env_var: &str,
    explicit: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(p) = explicit {
        if is_executable_file(p) {
            return Some(p.to_path_buf());
        }
        warn!(path = %p.display(), "Explicit binary path does not exist or is not a file");
    }

    if let Ok(val) = env::var(env_var) {
        let p = PathBuf::from(val);
        if is_executable_file(&p) {
            return Some(p);
        }
    }

    let exe_name = binary_executable_name(base_name);

    // Search PATH
    if let Some(path_os) = env::var_os("PATH") {
        for dir in env::split_paths(&path_os) {
            let candidate = dir.join(&exe_name);
            if is_executable_file(&candidate) {
                debug!(candidate = %candidate.display(), "Found binary in PATH");
                return Some(candidate);
            }
        }
    }

    // Search common platform-specific fallback locations (important when launched as a GUI bundle on macOS/Linux/Windows)
    let fallback_dirs: &[&str] = if cfg!(target_os = "macos") {
        &[
            "/opt/homebrew/bin",
            "/usr/local/bin",
            "/opt/local/bin",
            "/usr/bin",
        ]
    } else if cfg!(target_os = "windows") {
        &[
            r"C:\ffmpeg\bin",
            r"C:\Program Files\ffmpeg\bin",
            r"C:\ProgramData\chocolatey\bin",
        ]
    } else {
        &["/usr/local/bin", "/usr/bin", "/snap/bin"]
    };

    for dir in fallback_dirs {
        let candidate = Path::new(dir).join(&exe_name);
        if is_executable_file(&candidate) {
            debug!(candidate = %candidate.display(), "Found binary in fallback directory");
            return Some(candidate);
        }
    }

    None
}

fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

/// Runs `<binary> -version` and returns the first line of output.
pub fn query_version(binary_path: &Path) -> Result<String, PlatformError> {
    query_version_flag(binary_path, "-version")
}

/// Runs `<binary> <flag>` and returns the first non-empty line of output.
pub fn query_version_flag(binary_path: &Path, flag: &str) -> Result<String, PlatformError> {
    let mut cmd = Command::new(binary_path);
    cmd.arg(flag);
    configure_background_command(&mut cmd);

    let output = cmd
        .output()
        .map_err(|source| PlatformError::ExecutionFailed {
            path: binary_path.to_path_buf(),
            source,
        })?;

    if !output.status.success() {
        return Err(PlatformError::NonZeroExit {
            path: binary_path.to_path_buf(),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let first_line = stdout
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("Unknown version")
        .trim();
    Ok(first_line.to_string())
}

/// Applies platform-neutral command flags (such as hiding console windows on Windows).
pub fn configure_background_command(_cmd: &mut Command) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        _cmd.creation_flags(CREATE_NO_WINDOW);
    }
}

/// Platform-independent paths for CutDub configuration, presets, and caches.
#[derive(Debug, Clone)]
pub struct AppPaths {
    pub config_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub settings_file: PathBuf,
    pub presets_file: PathBuf,
}

impl AppPaths {
    pub fn resolve() -> Self {
        if let Some(proj_dirs) = ProjectDirs::from("org", "CutDub", "CutDub") {
            let config_dir = proj_dirs.config_dir().to_path_buf();
            let cache_dir = proj_dirs.cache_dir().to_path_buf();
            Self::from_dirs(config_dir, cache_dir)
        } else {
            let fallback = env::temp_dir().join("cutdub");
            Self::from_dirs(fallback.join("config"), fallback.join("cache"))
        }
    }

    pub fn from_dirs(config_dir: PathBuf, cache_dir: PathBuf) -> Self {
        let settings_file = config_dir.join("settings.json");
        let presets_file = config_dir.join("encoding_presets.json");
        Self {
            config_dir,
            cache_dir,
            settings_file,
            presets_file,
        }
    }

    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        fs::create_dir_all(&self.config_dir)?;
        fs::create_dir_all(&self.cache_dir)?;
        Ok(())
    }
}

/// Persistent user settings for CutDub.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    pub custom_ffmpeg_path: Option<PathBuf>,
    pub custom_ffprobe_path: Option<PathBuf>,
    pub frame_cache_capacity: usize,
    pub prefer_hardware_decode: bool,
    pub recent_files: Vec<PathBuf>,
    pub max_recent_files: usize,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            custom_ffmpeg_path: None,
            custom_ffprobe_path: None,
            frame_cache_capacity: 48,
            prefer_hardware_decode: true,
            recent_files: Vec::new(),
            max_recent_files: 10,
        }
    }
}

impl AppSettings {
    pub fn load(paths: &AppPaths) -> Self {
        if let Ok(bytes) = fs::read(&paths.settings_file) {
            if let Ok(settings) = serde_json::from_slice::<Self>(&bytes) {
                return settings;
            }
        }
        Self::default()
    }

    pub fn save(&self, paths: &AppPaths) -> Result<(), PlatformError> {
        paths
            .ensure_dirs()
            .map_err(|e| PlatformError::Config(e.to_string()))?;
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| PlatformError::Config(e.to_string()))?;
        fs::write(&paths.settings_file, json)
            .map_err(|e| PlatformError::Config(e.to_string()))?;
        Ok(())
    }

    pub fn add_recent_file(&mut self, path: PathBuf) {
        self.recent_files.retain(|p| p != &path);
        self.recent_files.insert(0, path);
        if self.recent_files.len() > self.max_recent_files {
            self.recent_files.truncate(self.max_recent_files);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_settings_roundtrip_and_recent_files() {
        let tmp = TempDir::new().unwrap();
        let paths = AppPaths::from_dirs(tmp.path().join("cfg"), tmp.path().join("cache"));
        let mut settings = AppSettings::default();
        settings.add_recent_file(PathBuf::from("/tmp/a.mp4"));
        settings.add_recent_file(PathBuf::from("/tmp/b.mp4"));
        settings.add_recent_file(PathBuf::from("/tmp/a.mp4"));
        assert_eq!(settings.recent_files.len(), 2);
        assert_eq!(settings.recent_files[0], PathBuf::from("/tmp/a.mp4"));

        settings.save(&paths).unwrap();
        let loaded = AppSettings::load(&paths);
        assert_eq!(loaded.recent_files, settings.recent_files);
        assert_eq!(loaded.frame_cache_capacity, 48);
    }

    #[test]
    fn test_discover_ffmpeg_binaries() {
        // On the dev machine ffmpeg and ffprobe are installed in /opt/homebrew/bin
        let discovered = FfmpegBinaries::discover(None, None);
        assert!(discovered.is_ok(), "Expected ffmpeg and ffprobe to be discovered");
        let bins = discovered.unwrap();
        assert!(bins.ffmpeg_version.to_lowercase().contains("ffmpeg"));
        assert!(bins.ffprobe_version.to_lowercase().contains("ffprobe"));
    }
}
