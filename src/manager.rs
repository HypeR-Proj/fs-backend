// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use crate::admission::Publication;
use crate::format::{self as driver, Format, PROBE_BYTES};
use crate::{filesystem_error, process::Worker, show};
use hyper_fs_backend::{
    config::Config,
    fstab::{self, Entry},
    worker,
};
use hyper_os::{
    block::{BlockQueue, NativeBlock, QueueConfig, VolumeInfo},
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
    pub(super) queue: Option<BlockQueue>,
    pub(super) last_target: String,
    pub(super) fault: Option<String>,
    pub(super) incident: Option<String>,
}
impl Volume {
    fn retire_queue(&mut self) -> Result<(), String> {
        if let Some(queue) = &self.queue {
            self.block
                .as_ref()
                .ok_or("missing retained volume authority")?
                .retire_queue(queue.as_handle_ref(), crate::deadline()?)
                .map_err(show)?;
            self.queue = None;
        }
        Ok(())
    }

    fn record_fault(
        &mut self,
        monitor: &crate::monitor::Monitor,
        stage: &str,
        cause: &str,
        loss: &str,
        recovery: &str,
    ) -> String {
        let process = self
            .worker
            .as_ref()
            .and_then(|worker| worker.child.basic_info().ok())
            .map_or(0, |info| info.koid.get());
        let observation = self.worker.as_ref().map_or_else(String::new, |worker| {
            worker
                .observation
                .detail()
                .unwrap_or_else(|error| format!("observation unavailable: {error}"))
        });
        let origin = self
            .incident
            .get_or_insert_with(|| format!("stage={stage} cause={cause}"));
        let report = format!(
            "stage={stage} loss={loss} recovery={recovery} process=0x{process:x} generation=0x{process:x} volume={} mount={:?} cause={cause}; origin=[{origin}]; observation=[{observation}]",
            self.id, self.last_target
        );
        if self.fault.as_ref() != Some(&report) {
            // Keep a queryable record even after the retired worker disappears.
            self.fault = Some(report.clone());
            monitor.report_fault(&report);
        }
        report
    }

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

    fn mount_all(
        &mut self,
        entries: &[Entry],
        publication: &Publication<'_>,
    ) -> Result<String, String> {
        let mut failures = Vec::new();
        for entry in entries.iter().filter(|entry| entry.automatic) {
            publication.check().map_err(show)?;
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
                self.mount(entry, publication)
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
    fn mount(&mut self, entry: &Entry, publication: &Publication<'_>) -> Result<(), String> {
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
        if volume.worker.is_some() || volume.queue.is_some() {
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
        // Reject unsupported access before delegating any queue to a worker.
        if format == Format::F2fs && entry.read_only {
            return Err("F2FS read-only mounts are not supported".into());
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
        // Finish read-only preparation before accepting the operation: even
        // metadata may wait for a filesystem worker or backing I/O.
        let create_target = match std::fs::metadata(&entry.target) {
            Ok(metadata) if metadata.is_dir() => false,
            Ok(_) => return Err("mount target is not a directory".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(error) => return Err(error.to_string()),
        };
        publication.check().map_err(show)?;
        // An existing mountpoint needs no mutation, including over EROFS.
        if create_target {
            std::fs::create_dir_all(&entry.target).map_err(|error| error.to_string())?;
        }
        #[cfg(feature = "block-queue-test")]
        crate::queue_probe::run(
            block,
            volume.geometry.sectors,
            entry.read_only,
            self.startup
                .borrow(hyper_os::startup::ROOT_VMAR)
                .map_err(show)?,
        )?;
        volume.last_target = entry.target.clone();
        volume.queue = Some(block.create_queue(QueueConfig::default()).map_err(show)?);
        // A new generation starts a new incident, while the last fault remains
        // queryable until another failure replaces it.
        volume.incident = None;
        let worker = Worker::start(
            &self.startup,
            &self.root,
            &self.monitor,
            program,
            format,
            volume.geometry,
            entry,
            volume
                .queue
                .as_ref()
                .ok_or("missing delegated queue")?
                .as_handle_ref(),
        );
        let mut worker = match worker {
            Ok(worker) => worker,
            Err(error) => {
                if let Err(retirement) = volume.retire_queue() {
                    volume.record_fault(
                        &self.monitor,
                        "launch-retirement",
                        &retirement,
                        "unknown",
                        "unavailable",
                    );
                }
                return Err(error);
            }
        };
        let result = worker.publish(&self.root, entry.read_only);
        volume.worker = Some(worker);
        if let Err(error) = result {
            // Keep the failed generation until both process and queue retirement
            // are proven. A failed cleanup must not silently admit a replacement.
            let cleanup = volume
                .worker
                .as_mut()
                .ok_or("missing worker")?
                .finish()
                .and_then(|()| volume.retire_queue());
            match cleanup {
                Ok(()) => volume.worker = None,
                Err(cause) => {
                    volume.record_fault(
                        &self.monitor,
                        "publication-cleanup",
                        &cause,
                        "unknown",
                        "unavailable",
                    );
                }
            }
            return Err(error);
        }
        Ok(())
    }
    fn unmount(&mut self, target: &str, publication: &Publication<'_>) -> Result<String, String> {
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
        publication.check().map_err(show)?;
        // Failure (including Busy) leaves both namespace and worker in place.
        hyper_os::filesystem::unmount(self.root.as_handle_ref(), target)
            .map_err(|error| format!("{target}: {}", filesystem_error(error)))?;
        let worker = volume.worker.as_mut().ok_or("missing mounted worker")?;
        worker.published = false;
        let result = worker.finish().and_then(|()| volume.retire_queue());
        if let Err(cause) = result {
            return Err(volume.record_fault(
                &self.monitor,
                "graceful-retirement",
                &cause,
                "unknown",
                "unavailable",
            ));
        }
        volume.worker = None;
        Ok(String::new())
    }

    fn detach_failed(
        &mut self,
        target: &str,
        publication: &Publication<'_>,
    ) -> Result<String, String> {
        if target == "/" {
            return Err("the active root filesystem cannot be detached".into());
        }
        let volume = self
            .volumes
            .iter_mut()
            .find(|volume| {
                volume.last_target == target && (volume.worker.is_some() || volume.queue.is_some())
            })
            .ok_or("target has no managed failed generation")?;
        publication.check().map_err(show)?;
        let mut discarded = None;
        if let Some(worker) = &mut volume.worker
            && worker.published
        {
            let id = worker.mount_id.ok_or("missing failed mount identity")?;
            // The kernel independently refuses a healthy provider, the root
            // mount and nested mounts. No worker is killed before this gate.
            discarded = Some(
                hyper_os::filesystem::detach_failed(self.root.as_handle_ref(), id)
                    .map_err(filesystem_error)?,
            );
            worker.published = false;
        }
        let loss = discarded.map_or_else(
            || "unknown".into(),
            |bytes| format!("vfs-bytes:{bytes};metadata:unknown"),
        );
        volume.record_fault(
            &self.monitor,
            "detached",
            "explicit failed-provider recovery",
            &loss,
            "quarantined",
        );
        let retired = match &mut volume.worker {
            Some(worker) => worker.stop_failed(),
            None => Ok(()),
        }
        .and_then(|()| volume.retire_queue());
        if let Err(cause) = retired {
            return Err(volume.record_fault(
                &self.monitor,
                "queue-retirement",
                &cause,
                &loss,
                "unavailable",
            ));
        }
        let report = volume.record_fault(
            &self.monitor,
            "retired",
            "old generation cannot submit I/O",
            &loss,
            "remount-allowed",
        );
        volume.worker = None;
        Ok(format!("{report}\n"))
    }
    pub fn command(
        &mut self,
        request: Request<'_>,
        publication: &Publication<'_>,
    ) -> Result<String, String> {
        publication.check().map_err(show)?;
        match request {
            Request::List => {
                // Root switching preserves mount identity, but a rejected
                // successor can leave this original manager serving control.
                // Resolve the current root label without rewriting its original
                // target: a still-pending root transition may also roll back.
                let root_mount = self.root.info().map_err(show)?.location().mount().get();
                let mut output = String::new();
                for volume in &self.volumes {
                    if let Some(fault) = &volume.fault {
                        use std::fmt::Write;
                        writeln!(output, "CRITICAL: {fault}").map_err(show)?;
                    }
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
                self.mount_all(&entries, publication)
            }
            Request::Unmount { target } => self.unmount(target, publication),
            Request::DetachFailed { target } => self.detach_failed(target, publication),
            Request::Entry { table, name } => {
                let entry = self
                    .load_table(table)?
                    .into_iter()
                    .find(|entry| entry.source == name || entry.target == name)
                    .ok_or("no matching fstab entry")?;
                self.mount(&entry, publication)?;
                Ok(String::new())
            }
            Request::Mount {
                source,
                target,
                format,
                options,
            } => {
                let (read_only, automatic, required) = fstab::options(options)?;
                self.mount(
                    &Entry {
                        source: source.into(),
                        target: target.into(),
                        format: fstab::format(format)?,
                        read_only,
                        automatic,
                        required,
                    },
                    publication,
                )?;
                Ok(String::new())
            }
        }
    }
}
