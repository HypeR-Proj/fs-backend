// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use crate::format::Format;
use hyper_os::{Error, Status};

use crate::config::{Config, WorkerDirectory};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Source {
    Environment,
    Namespace,
}

pub struct Program<T> {
    pub path: String,
    pub source: Source,
    pub image: T,
}

/// Search only trusted deployment directories. A missing file permits trying
/// the next directory; access or storage failures must never select a fallback.
pub fn open<T>(
    config: &Config,
    format: Format,
    target: &str,
    mut open_image: impl FnMut(Source, &str) -> hyper_os::Result<T>,
) -> Result<Program<T>, String> {
    let mut attempted = Vec::new();
    for directory in &config.worker_directories {
        let (source, directory) = match directory {
            WorkerDirectory::Environment(path) => (Source::Environment, path),
            WorkerDirectory::Namespace(path) => (Source::Namespace, path),
        };
        let path = format!(
            "{}/{}",
            directory.trim_end_matches('/'),
            format.worker_name()
        );
        match open_image(source, &path) {
            Ok(image) => {
                return Ok(Program {
                    path,
                    source,
                    image,
                });
            }
            Err(Error::Status(Status::NOT_FOUND)) => attempted.push(format!("{source:?}:{path}")),
            Err(error) => {
                return Err(format!(
                    "{} worker for {}: open {path}: {error:?}",
                    format.name(),
                    target,
                ));
            }
        }
    }
    Err(format!(
        "{} recognized for {}, but filesystem worker is unavailable; tried: {}",
        format.name(),
        target,
        attempted.join(", "),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{PROBE_BYTES, identify};

    fn split_config() -> Config {
        Config {
            worker_directories: vec![
                WorkerDirectory::Environment("svc".into()),
                WorkerDirectory::Namespace("/system/svc".into()),
            ],
        }
    }

    #[test]
    fn recognized_missing_worker_is_not_an_unknown_format() -> Result<(), String> {
        let mut probe = [0; PROBE_BYTES];
        assert_eq!(identify(&probe), None);
        probe[1024..1028].copy_from_slice(&0xe0f5_e1e2u32.to_le_bytes());
        let format = identify(&probe).ok_or("EROFS was not recognized")?;
        let result = open::<()>(&split_config(), format, "/system", |_, _| {
            Err(Error::Status(Status::NOT_FOUND))
        });
        let Err(error) = result else {
            return Err("missing worker was accepted".into());
        };
        assert_eq!(
            error,
            "EROFS recognized for /system, but filesystem worker is unavailable; tried: Environment:svc/fs-erofs, Namespace:/system/svc/fs-erofs"
        );
        Ok(())
    }

    #[test]
    fn missing_boot_worker_can_be_loaded_from_system() -> Result<(), String> {
        let mut attempted = Vec::new();
        let program = open(&split_config(), Format::Fat, "/system", |source, path| {
            attempted.push((source, path.to_string()));
            if path == "/system/svc/fs-fat" {
                Ok(17)
            } else {
                Err(Error::Status(Status::NOT_FOUND))
            }
        })?;
        assert_eq!(
            attempted,
            [
                (Source::Environment, "svc/fs-fat".into()),
                (Source::Namespace, "/system/svc/fs-fat".into()),
            ]
        );
        assert_eq!(program.path, "/system/svc/fs-fat");
        assert_eq!(program.source, Source::Namespace);
        assert_eq!(program.image, 17);
        Ok(())
    }

    #[test]
    fn retained_environment_workers_need_no_namespace_lookup() -> Result<(), String> {
        for target in ["/system", "/data", "/tmp/recovered"] {
            let program = open(&split_config(), Format::Fat, target, |source, path| {
                assert_eq!(source, Source::Environment);
                assert_eq!(path, "svc/fs-fat");
                Ok(19)
            })?;
            assert_eq!(program.source, Source::Environment);
            assert_eq!(program.image, 19);
        }
        Ok(())
    }

    #[test]
    fn loader_selection_uses_declared_scope_not_path_prefixes() -> Result<(), String> {
        let config = Config {
            worker_directories: vec![WorkerDirectory::Namespace("/boot/extra".into())],
        };
        let program = open(&config, Format::Fat, "/data", |source, path| {
            assert_eq!(source, Source::Namespace);
            assert_eq!(path, "/boot/extra/fs-fat");
            Ok(())
        })?;
        assert_eq!(program.source, Source::Namespace);
        Ok(())
    }

    #[test]
    fn denied_or_failed_worker_does_not_select_another_image() {
        for status in [Status::ACCESS_DENIED, Status::IO_ERROR, Status::PEER_CLOSED] {
            let mut attempts = 0;
            let result = open::<()>(&split_config(), Format::Fat, "/system", |_, _| {
                attempts += 1;
                Err(Error::Status(status))
            });
            assert!(result.is_err());
            assert_eq!(attempts, 1);
        }
    }
}
