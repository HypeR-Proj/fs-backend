// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use crate::format::{self as driver, Format, PROBE_BYTES};
use crate::{filesystem_error, process::Worker, show};
use hyper_fs_backend::{
    config::Config,
    fstab::{self, Entry},
    worker,
};
use hyper_os::{
    block::{NativeBlock, VolumeInfo},
    fs::Directory,
    startup::Startup,
};
use hyper_service::filesystem::control::Request;

pub(super) struct Volume {
    pub id: hyper_block::Guid,
    pub label: String,
    pub(super) geometry: VolumeInfo,
    pub block: Option<NativeBlock>,
    pub(super) worker: Option<Worker>,
}
impl Volume {
    fn matches(&self, source: &str) -> bool {
        hyper_block::Selector::parse(source)
            .is_some_and(|selector| selector.matches(self.id, &self.label))
    }
}
pub(super) struct Manager<'a> {
    pub startup: Startup<'a>,
    pub root: Directory,
    pub environment: Directory,
    pub volumes: Vec<Volume>,
    pub monitor: crate::monitor::Monitor,
}
impl Manager<'_> {
    fn load_table(&self, table: &str) -> Result<Vec<Entry>, String> {
        if !hyper_fs_backend::config::absolute_path(table) {
            return Err("fstab path must be absolute and normalized".into());
        }
        fstab::load(&self.root, table)
    }

    fn mount_all(&mut self, entries: &[Entry]) -> Result<String, String> {
        let mut failures = Vec::new();
        for entry in entries.iter().filter(|entry| entry.automatic) {
            let existing = self.volumes.iter().find_map(|volume| {
                volume
                    .matches(&entry.source)
                    .then_some(volume)
                    .and_then(|volume| volume.worker.as_ref())
                    .filter(|worker| {
                        worker.target == entry.target
                            && worker.read_only == entry.read_only
                            && entry.format.is_none_or(|format| worker.format == format)
                    })
            });
            let result = if let Some(worker) = existing {
                worker.observation.require_running()
            } else {
                self.mount(entry)
            };
            if let Err(error) = result {
                eprintln!(
                    "HypeR fs-backend: {} on {}: {error}",
                    entry.source, entry.target
                );
                if entry.required {
                    failures.push(format!("{}: {error}", entry.target));
                }
            }
        }
        if failures.is_empty() {
            Ok(String::new())
        } else {
            Err(failures.join("; "))
        }
    }
    fn mount(&mut self, entry: &Entry) -> Result<(), String> {
        let config = Config::load(&self.environment)?;
        if hyper_block::Selector::parse(&entry.source).is_none()
            || !hyper_fs_backend::config::absolute_path(&entry.target)
            || entry.target == "/"
        {
            return Err("invalid partition source or mount target".into());
        }
        if self.volumes.iter().any(|volume| {
            volume
                .worker
                .as_ref()
                .is_some_and(|worker| worker.target == entry.target)
        }) {
            return Err(format!("{} is already mounted", entry.target));
        }
        if self
            .volumes
            .iter()
            .filter(|volume| volume.matches(&entry.source))
            .count()
            > 1
        {
            return Err(format!("ambiguous partition source: {}", entry.source));
        }
        let volume = self
            .volumes
            .iter_mut()
            .find(|volume| volume.matches(&entry.source))
            .ok_or_else(|| format!("unknown Native volume: {}", entry.source))?;
        if volume.worker.is_some() {
            return Err(format!(
                "{} already has an exclusive mounted owner",
                entry.source
            ));
        }
        let block = volume
            .block
            .as_ref()
            .ok_or("volume unavailable after worker failure; a new block connection is required")?;
        if volume.geometry.read_only && !entry.read_only {
            return Err("volume is hardware read-only".into());
        }
        let mut probe = [0; PROBE_BYTES];
        let count = volume.geometry.sectors.min((PROBE_BYTES / 512) as u64) as usize * 512;
        block
            .read_sectors(0, &mut probe[..count])
            .map_err(filesystem_error)?;
        let format = driver::identify(&probe[..count]).ok_or("unrecognized filesystem magic")?;
        if entry.format.is_some_and(|expected| expected != format) {
            return Err("requested filesystem type does not match media magic".into());
        }
        if format == Format::Erofs && !entry.read_only {
            return Err("EROFS requires -o ro".into());
        }
        // Missing workers leave the sole block here, so installing the driver
        // later permits retrying this same volume without a new activation.
        let program = worker::open(&config, format, &entry.target, |source, path| {
            let directory = match source {
                worker::Source::Environment => &self.environment,
                worker::Source::Namespace => &self.root,
            };
            directory.open(path, hyper_service::process::EXECUTABLE_FILE_RIGHTS)
        })?;
        // An existing mountpoint needs no directory mutation. In particular,
        // mounting a writable volume over an EROFS root must not try mkdir.
        match std::fs::metadata(&entry.target) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Err("mount target is not a directory".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(&entry.target).map_err(|error| error.to_string())?;
            }
            Err(error) => return Err(error.to_string()),
        }
        let mut worker = Worker::start(
            &self.startup,
            &self.root,
            &self.monitor,
            program,
            format,
            volume.geometry,
            entry,
            &mut volume.block,
        )?;
        match worker.publish(&self.root, entry.read_only) {
            Ok(()) => {
                volume.worker = Some(worker);
                Ok(())
            }
            Err(error) => {
                // Publication may have committed before mount identity inspection
                // failed. Keep that live provider; never drop its exclusive media.
                if worker.published {
                    volume.worker = Some(worker);
                    return Err(error);
                }
                match worker.finish() {
                    Ok(block) => volume.block = Some(block),
                    Err(cleanup) => {
                        eprintln!("HypeR fs-backend: rejected mount cleanup: {cleanup}")
                    }
                }
                Err(error)
            }
        }
    }
    fn unmount(&mut self, target: &str) -> Result<String, String> {
        if target == "/" {
            return Err("the active root filesystem cannot be unmounted".into());
        }
        let volume = self
            .volumes
            .iter_mut()
            .find(|volume| {
                volume
                    .worker
                    .as_ref()
                    .is_some_and(|worker| worker.target == target)
            })
            .ok_or("target is not managed by this filesystem service")?;
        // Failure (including Busy) leaves both namespace and worker in place.
        hyper_os::filesystem::unmount(self.root.as_handle_ref(), target)
            .map_err(|error| format!("{target}: {}", filesystem_error(error)))?;
        let worker = volume.worker.take().ok_or("missing mounted worker")?;
        volume.block = Some(worker.finish().map_err(|error| {
            format!("{target} detached, but volume ownership could not be recovered: {error}")
        })?);
        Ok(String::new())
    }
    pub fn command(&mut self, request: Request<'_>) -> Result<String, String> {
        match request {
            Request::List => {
                // Root switching preserves mount identity, but a rejected
                // successor can leave this original manager serving control.
                // Resolve the current root label without rewriting its original
                // target: a still-pending root transition may also roll back.
                let root_mount = self.root.info().map_err(show)?.location().mount().get();
                let mut output = String::new();
                for volume in &self.volumes {
                    if let Some(worker) = &volume.worker {
                        use std::fmt::Write;
                        writeln!(
                            output,
                            "{} on {} type {} ({}) {}",
                            format_args!("PARTUUID={}", volume.id),
                            if worker.mount_id == Some(root_mount) {
                                "/"
                            } else {
                                &worker.target
                            },
                            worker.format.name().to_ascii_lowercase(),
                            if worker.read_only { "ro" } else { "rw" },
                            worker.observation.detail()?
                        )
                        .map_err(show)?;
                    }
                }
                Ok(output)
            }
            Request::All { table } => {
                let entries = self.load_table(table)?;
                self.mount_all(&entries)
            }
            Request::Unmount { target } => self.unmount(target),
            Request::Entry { table, name } => {
                let entry = self
                    .load_table(table)?
                    .into_iter()
                    .find(|entry| entry.source == name || entry.target == name)
                    .ok_or("no matching fstab entry")?;
                self.mount(&entry)?;
                Ok(String::new())
            }
            Request::Mount {
                source,
                target,
                format,
                options,
            } => {
                let (read_only, automatic, required) = fstab::options(options)?;
                self.mount(&Entry {
                    source: source.into(),
                    target: target.into(),
                    format: fstab::format(format)?,
                    read_only,
                    automatic,
                    required,
                })?;
                Ok(String::new())
            }
        }
    }
}
