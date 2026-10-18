// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Observe workers independently of mount-control RPCs. A failed worker remains
//! fenced by its mount owner; observation never stops siblings or reuses media.

use crate::{format::Format, show};
use hyper_os::{
    handle::{ByteChannelObject, CapabilityChannelObject, OwnedHandle, ProcessObject, Rights},
    startup::Startup,
    task::{ProcessInfo, ProcessTermination},
    wait::{ObjectSignals, WaitItem, wait_many},
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

const MAX_WORKERS: usize = 128;

#[derive(Default)]
struct State {
    stopping: bool,
    terminal: Option<ProcessTermination>,
    logged: Option<String>,
    reported: Option<String>,
    notified: Option<String>,
    retention: Option<String>,
}

pub(super) struct Observation {
    process: Arc<OwnedHandle<ProcessObject>>,
    retirement: hyper_os::task::ProcessRetirement,
    name: &'static str,
    target: String,
    koid: u64,
    state: Mutex<State>,
    owner_released: AtomicBool,
    wake: Arc<OwnedHandle<ByteChannelObject>>,
}

impl Observation {
    /// Process termination reasons are immutable. Querying the Process also
    /// closes the interval before the monitoring thread next runs.
    pub fn info(&self) -> Result<ProcessInfo, String> {
        let info = self.process.as_process_supervisor().info().map_err(show)?;
        let mut state = self.state.lock().map_err(show)?;
        if state.terminal.is_none() {
            state.terminal = info.terminal;
        }
        if info.terminal.is_some() && state.retention.is_none() {
            let retained = self.retirement.info().map_err(show)?;
            if retained.ready() || retained.released() {
                self.retirement.release().map_err(show)?;
                state.retention = Some(format!(
                    "fault={} pc=0x{:x} address=0x{:x} retained={} bytes",
                    retained.fault_class,
                    retained.fault_pc,
                    retained.fault_address,
                    retained.retained_bytes,
                ));
            }
        }
        Ok(info)
    }

    pub fn require_running(&self) -> Result<(), String> {
        let info = self.info()?;
        if let Some(reason) = info.terminal {
            return Err(self.failure(reason));
        }
        Ok(())
    }

    pub fn detail(&self) -> Result<String, String> {
        let info = self.info()?;
        Ok(match info.terminal {
            Some(reason) => {
                let retained = self.state.lock().map_err(show)?.retention.clone();
                let mut detail = format!("failed: {}", self.failure(reason));
                if let Some(retained) = retained {
                    detail.push_str("; ");
                    detail.push_str(&retained);
                }
                detail
            }
            None => format!("worker={} process=0x{:x} running", self.name, self.koid),
        })
    }

    pub fn begin_stop(&self) -> Result<(), String> {
        self.info()?;
        let mut state = self.state.lock().map_err(show)?;
        // A death already observed before unmount cannot become an expected
        // exit merely because the administrator subsequently detaches it.
        if state.terminal.is_none() {
            state.stopping = true;
        }
        drop(state);
        let _ = self.wake.as_byte_channel().try_send(&[1]);
        Ok(())
    }

    fn failure(&self, reason: ProcessTermination) -> String {
        format!(
            "{} process=0x{:x} terminated: {reason:?}; mount={:?}",
            self.name, self.koid, self.target
        )
    }

    fn unexpected(&self) -> Result<Option<String>, String> {
        let state = self.state.lock().map_err(show)?;
        Ok(state.terminal.and_then(|reason| {
            let clean = matches!(
                reason,
                ProcessTermination::ProcessExited { status: 0 }
                    | ProcessTermination::LastThreadExited { status: 0 }
                    | ProcessTermination::ThreadExited { status: 0 }
            );
            (!state.stopping || !clean).then(|| self.failure(reason))
        }))
    }

    fn removable(&self) -> Result<bool, String> {
        if !self.owner_released.load(Ordering::Acquire) {
            return Ok(false);
        }
        let failure = self.unexpected()?;
        let state = self.state.lock().map_err(show)?;
        Ok(state.terminal.is_some()
            && state.retention.is_some()
            && failure.as_ref().is_none_or(|failure| {
                state.logged.as_ref() == Some(failure)
                    && state.reported.as_ref() == Some(failure)
                    && state.notified.as_ref() == Some(failure)
            }))
    }
}

/// The sole mount/launch owner. The monitor retains its own observation until
/// terminal diagnostics are delivered, including failed startup and unmount.
pub(super) struct Registration(Arc<Observation>);

impl core::ops::Deref for Registration {
    type Target = Observation;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.0.owner_released.store(true, Ordering::Release);
        let _ = self.0.wake.as_byte_channel().try_send(&[1]);
    }
}

pub(super) struct Monitor {
    workers: Arc<Mutex<Vec<Arc<Observation>>>>,
    wake: Arc<OwnedHandle<ByteChannelObject>>,
}

impl Monitor {
    pub fn start(startup: &Startup<'_>) -> Result<Self, String> {
        let connector = startup
            .borrow(hyper_component::CONNECTOR)
            .map_err(show)?
            .duplicate(
                Rights::WRITE
                    .union(Rights::WAIT)
                    .union(Rights::TRANSFER)
                    .union(Rights::DUPLICATE),
            )
            .map_err(show)?;
        let (wake, reader) = hyper_os::channel::create_pair().map_err(show)?;
        let workers = Arc::new(Mutex::new(Vec::new()));
        let observed = workers.clone();
        std::thread::Builder::new()
            .name("filesystem-workers".into())
            .spawn(move || {
                if let Err(error) = observe(observed, reader, connector) {
                    // Reporting failure must not tear down healthy root/media
                    // ownership. Direct mount queries still inspect Processes.
                    eprintln!("HypeR fs-backend: worker monitoring failed: {error}");
                }
            })
            .map_err(show)?;
        Ok(Self {
            workers,
            wake: Arc::new(wake),
        })
    }

    pub fn register(
        &self,
        process: Arc<OwnedHandle<ProcessObject>>,
        retirement: hyper_os::task::ProcessRetirement,
        format: Format,
        target: &str,
    ) -> Result<Registration, String> {
        let observation = Arc::new(Observation {
            koid: process.basic_info().map_err(show)?.koid.get(),
            process,
            retirement,
            name: format.worker_name(),
            // Component status is bounded to 256 characters; preserve room for
            // the process identity and termination reason even for a long path.
            target: target.chars().take(64).collect(),
            state: Mutex::new(State::default()),
            owner_released: AtomicBool::new(false),
            wake: self.wake.clone(),
        });
        let mut workers = self.workers.lock().map_err(show)?;
        if workers.len() == MAX_WORKERS {
            return Err("filesystem worker observation limit exceeded".into());
        }
        workers.push(observation.clone());
        drop(workers);
        // The registry is durable; a coalesced/full wake pipe cannot lose it.
        let _ = self.wake.as_byte_channel().try_send(&[1]);
        Ok(Registration(observation))
    }

    /// Only mount control registers workers; the observer can only remove
    /// entries. Check before delegating exclusive media to a new Process.
    pub fn check_capacity(&self) -> Result<(), String> {
        if self.workers.lock().map_err(show)?.len() >= MAX_WORKERS {
            return Err("filesystem worker observation limit exceeded".into());
        }
        Ok(())
    }
}

fn observe(
    registry: Arc<Mutex<Vec<Arc<Observation>>>>,
    wake: OwnedHandle<ByteChannelObject>,
    connector: OwnedHandle<CapabilityChannelObject>,
) -> Result<(), String> {
    let stderr = hyper_rt::process::stderr().map_err(show)?;
    let mut published = String::new();
    let mut last_failure = None;
    let mut scratch = [0; 128];
    loop {
        while wake.as_byte_channel().try_receive(&mut scratch).is_ok() {}
        let workers = registry.lock().map_err(show)?.clone();
        let mut live = Vec::new();
        let mut failures = Vec::new();
        let mut retry = false;
        for worker in &workers {
            let failure = match worker.info() {
                Ok(info) => {
                    if info.terminal.is_none() {
                        live.push(worker.process.clone());
                    } else if worker.state.lock().map_err(show)?.retention.is_none() {
                        // TERMINATED precedes the reaper's diagnostic capture.
                        // Keep polling until it is ready or the bounded lease
                        // expires, without waiting for a new mount-control call.
                        retry = true;
                    }
                    worker.unexpected()?
                }
                Err(error) => {
                    // One failed inspection must not disable observation of
                    // all other filesystem providers.
                    retry = true;
                    Some(format!(
                        "{} process=0x{:x} inspection failed: {error}; mount={:?}",
                        worker.name, worker.koid, worker.target
                    ))
                }
            };
            if let Some(failure) = failure {
                failures.push((worker, failure.clone()));
                let notified =
                    worker.state.lock().map_err(show)?.notified.as_ref() == Some(&failure);
                if !notified {
                    // The parent manager retains a direct Console path even
                    // when the root filesystem or journal has failed.
                    if hyper_component::connection::report_failure(
                        connector.as_handle_ref(),
                        &failure,
                    )
                    .is_ok()
                    {
                        worker.state.lock().map_err(show)?.notified = Some(failure.clone());
                    } else {
                        retry = true;
                    }
                }
                let sent = worker.state.lock().map_err(show)?.logged.as_ref() == Some(&failure);
                if !sent {
                    let record = format!("HypeR fs-backend: {failure}\n");
                    // Journal backpressure cannot block mount control or the
                    // observation of another worker's terminal state.
                    match stderr.as_byte_channel().try_send(record.as_bytes()) {
                        Ok(()) => worker.state.lock().map_err(show)?.logged = Some(failure),
                        Err(_) => retry = true,
                    }
                }
            }
        }
        // Report each new failure at least once, rather than letting an older
        // failed mount permanently hide a later failed worker in the summary.
        let mut selected = None;
        for (worker, failure) in &failures {
            if worker.state.lock().map_err(show)?.reported.as_ref() != Some(failure) {
                selected = Some((*worker, failure));
                break;
            }
        }
        if let Some((_, failure)) = selected {
            last_failure = Some(failure.clone());
        }
        let summary = if !failures.is_empty() {
            let (_, failure) = selected
                .or_else(|| {
                    failures.iter().find_map(|(worker, failure)| {
                        (Some(failure) == last_failure.as_ref()).then_some((*worker, failure))
                    })
                })
                .or_else(|| failures.first().map(|(worker, failure)| (*worker, failure)))
                .ok_or("missing worker failure detail")?;
            format!("{} worker(s) failed; {failure}", failures.len())
        } else if let Some(failure) = &last_failure {
            format!("{} worker(s) running; last failure: {failure}", live.len())
        } else {
            format!("{} filesystem worker(s) running", live.len())
        };
        let summary: String = summary.chars().take(256).collect();
        if summary != published
            && hyper_component::connection::status(connector.as_handle_ref(), &summary).is_ok()
        {
            published = summary.clone();
        }
        retry |= summary != published;
        if summary == published
            && let Some((worker, failure)) = selected
        {
            worker.state.lock().map_err(show)?.reported = Some(failure.clone());
        }
        for (worker, failure) in &failures {
            retry |= worker.state.lock().map_err(show)?.reported.as_ref() != Some(failure);
        }
        {
            let mut registry = registry.lock().map_err(show)?;
            let mut index = 0;
            while index < registry.len() {
                if registry[index].removable()? {
                    registry.remove(index);
                    // Publish the changed worker count without requiring a
                    // further external event after the last owner disappeared.
                    retry = true;
                } else {
                    index += 1;
                }
            }
        }
        drop(failures);
        drop(workers);
        let mut items = vec![WaitItem::new(
            wake.as_handle_ref(),
            ObjectSignals::<ByteChannelObject>::READABLE
                .union(ObjectSignals::<ByteChannelObject>::PEER_CLOSED),
        )];
        // More mounted volumes than wait slots receive bounded polling. Normal
        // operation sleeps until a Process or registry change; terminal objects
        // are excluded to avoid a level-triggered busy loop.
        for process in live.iter().take(hyper_os::wait::MAX_ITEMS - 1) {
            items.push(WaitItem::new(
                process.as_handle_ref(),
                ObjectSignals::<ProcessObject>::TERMINATED,
            ));
        }
        let until = if retry || live.len() >= hyper_os::wait::MAX_ITEMS {
            hyper_os::time::deadline_after(std::time::Duration::from_millis(100))
                .map_err(show)?
                .as_raw()
        } else {
            hyper_os::DEADLINE_INFINITE
        };
        match wait_many(&items, until) {
            Ok(observation)
                if observation.index == 0
                    && ObjectSignals::<ByteChannelObject>::PEER_CLOSED
                        .is_present_in(observation.observed) =>
            {
                return Ok(());
            }
            Ok(_) | Err(hyper_os::Error::Status(hyper_os::Status::TIMED_OUT)) => {}
            Err(error) => return Err(show(error)),
        }
    }
}
