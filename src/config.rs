// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use std::io::Read;

use serde::Deserialize;

pub const PATH: &str = "etc/hyper/filesystem.json";
pub const MAX_BYTES: usize = 16 * 1024;
const MAX_WORKER_DIRECTORIES: usize = 8;

/// Deployment policy for the attached volumes. Worker directories remain
/// runtime search paths, independently of which format implementations are built.
#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub worker_directories: Vec<WorkerDirectory>,
}

/// An environment path stays attached to the manager's fixed system environment. A
/// namespace path resolves against the current root and uses the system loader.
#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum WorkerDirectory {
    Environment(String),
    Namespace(String),
}

impl Default for Config {
    fn default() -> Self {
        Self {
            worker_directories: vec![WorkerDirectory::Environment("svc".into())],
        }
    }
}

impl Config {
    #[cfg(target_os = "hyper")]
    pub fn load(environment: &hyper_os::fs::Directory) -> Result<Self, String> {
        let file = match environment.open(PATH, hyper_os::fs::FileRights::READ) {
            Ok(file) => file,
            Err(hyper_os::Error::Status(hyper_os::Status::NOT_FOUND)) => {
                return Ok(Self::default());
            }
            Err(error) => return Err(format!("open {PATH}: {error}")),
        };
        Self::read(file.into_std()).map_err(|error| format!("read {PATH}: {error}"))
    }

    /// Bound allocation even when the file size is unavailable or changes while
    /// reading. Reading one extra byte distinguishes a full file from truncation.
    pub fn read(input: impl Read) -> Result<Self, String> {
        let mut bytes = Vec::new();
        input
            .take(MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        Self::parse(&bytes)
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_BYTES {
            return Err("filesystem configuration exceeds 16 KiB".into());
        }
        let config: Self = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
        if config.worker_directories.is_empty()
            || config.worker_directories.len() > MAX_WORKER_DIRECTORIES
        {
            return Err("worker_directories must contain between 1 and 8 paths".into());
        }
        for (index, directory) in config.worker_directories.iter().enumerate() {
            let (path, valid) = match directory {
                WorkerDirectory::Environment(path) => (path, relative_path(path)),
                WorkerDirectory::Namespace(path) => (path, absolute_path(path)),
            };
            if !valid || path.len() + 1 + "fs-erofs".len() > hyper_os::fs::MAX_PATH_BYTES {
                return Err(
                    "worker directories require normalized paths in their declared scope".into(),
                );
            }
            if config.worker_directories[..index].contains(directory) {
                return Err(format!("duplicate worker directory: {directory:?}"));
            }
        }
        Ok(config)
    }
}

fn relative_path(path: &str) -> bool {
    !path.as_bytes().contains(&0)
        && path
            .split('/')
            .all(|component| !matches!(component, "" | "." | ".."))
}

pub fn absolute_path(path: &str) -> bool {
    path.len() <= hyper_os::fs::MAX_PATH_BYTES
        && !path.as_bytes().contains(&0)
        && (path == "/"
            || path.strip_prefix('/').is_some_and(|relative| {
                relative
                    .split('/')
                    .all(|component| !matches!(component, "" | "." | ".."))
            }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_preserve_the_configuration_volume_mount() -> Result<(), String> {
        assert_eq!(Config::parse(b"{}")?, Config::default());
        Ok(())
    }

    #[test]
    fn accepts_split_deployment_without_requiring_installed_workers() -> Result<(), String> {
        let config = Config::parse(
            br#"{"worker_directories":[{"environment":"svc"},{"namespace":"/system/svc"}]}"#,
        )?;
        assert_eq!(
            config.worker_directories,
            [
                WorkerDirectory::Environment("svc".into()),
                WorkerDirectory::Namespace("/system/svc".into()),
            ]
        );
        Ok(())
    }

    #[test]
    fn rejects_malformed_ambiguous_and_unbounded_configuration() {
        for bytes in [
            b"{".as_slice(),
            br#"{"mount_path":"/"}"#,
            br#"{"mount_path":"relative"}"#,
            br#"{"mount_path":"/system/../data"}"#,
            br#"{"worker_directories":[]}"#,
            br#"{"worker_directories":[{"environment":"/svc"}]}"#,
            br#"{"worker_directories":[{"environment":"../svc"}]}"#,
            br#"{"worker_directories":[{"namespace":"svc"}]}"#,
            br#"{"worker_directories":[{"namespace":"/svc/"}]}"#,
            br#"{"worker_directories":[{"environment":"svc"},{"environment":"svc"}]}"#,
            br#"{"worker_directories":[{"environment":"svc\u0000"}]}"#,
            br#"{"worker_directories":["/svc"]}"#,
            br#"{"worker_directory":"/svc"}"#,
            br#"{"mount_path":"/data","mount_path":"/system"}"#,
        ] {
            assert!(Config::parse(bytes).is_err(), "accepted {bytes:?}");
        }
        let oversized = vec![b' '; MAX_BYTES + 1];
        assert!(Config::parse(&oversized).is_err());
        let many_paths = (0..9)
            .map(|index| serde_json::json!({"environment": format!("svc{index}")}))
            .collect::<Vec<_>>();
        let bytes = serde_json::json!({"worker_directories": many_paths}).to_string();
        assert!(Config::parse(bytes.as_bytes()).is_err());
    }

    #[test]
    fn read_stops_after_the_size_bound() {
        let bytes = vec![b' '; MAX_BYTES * 2];
        let mut input = std::io::Cursor::new(bytes);
        assert!(Config::read(&mut input).is_err());
        assert_eq!(input.position(), MAX_BYTES as u64 + 1);
    }
}
