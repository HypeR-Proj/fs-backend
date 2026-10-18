// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Worker ownership transactions. Publication belongs to the manager, and a
//! clean return transfers the original `NativeBlock` only after namespace detach.

use crate::format::Format;
use crate::monitor::{Monitor, Registration};
use crate::{deadline, filesystem_error, show};
use core::mem::MaybeUninit;
use hyper_fs_backend::fstab::Entry;
use hyper_fs_backend::worker::{Program, Source};
use hyper_os::{
    block::{NativeBlock, VolumeInfo},
    capability_channel::{CapabilityChannel, CapabilityReceiveSlot},
    fs::{Directory, File},
    handle::{
        ByteChannelObject, NativeBlockObject, OwnedHandle, ProcessObject, ResourceDomainObject,
        Rights, RightsOffer, TaskGroupObject, VmoObject,
    },
    startup::{self, Startup},
    task::{
        ProcessBuilder, ProcessPhase, ResourceLimits, create_resource_domain, create_task_group,
    },
};
use hyper_service::filesystem;
use std::sync::Arc;

const LIMITS: ResourceLimits = ResourceLimits {
    kernel_memory_bytes: 64 * 1024 * 1024,
    processes: 1,
    threads: 4,
    handles: 64,
    kernel_objects: 256,
    committed_pages: 32 * 1024,
    pinned_pages: 32 * 1024,
    guest_pages: 0,
    ipc_messages: 32,
    ipc_bytes: 1024 * 1024,
    ipc_handles: 16,
    subscriptions: 32,
    timers: 32,
    virtual_machines: 0,
    virtual_cpus: 0,
    device_leases: 0,
    dma_mappings: 0,
    user_address_spaces: 1,
    user_mappings: 256,
};

pub(super) struct Worker {
    pub(super) child: Arc<OwnedHandle<ProcessObject>>,
    pub(super) observation: Registration,
    _group: OwnedHandle<TaskGroupObject>,
    _domain: OwnedHandle<ResourceDomainObject>,
    pub(super) control: CapabilityChannel,
    pub(super) owner: OwnedHandle<ByteChannelObject>,
    pub(super) channel: Option<OwnedHandle<ByteChannelObject>>,
    pub(super) buffer: Option<OwnedHandle<VmoObject>>,
    pub(super) published: bool,
    pub(super) mount_id: Option<u64>,
    pub target: String,
    pub format: Format,
    pub read_only: bool,
}
impl Worker {
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        startup: &Startup<'_>,
        root: &Directory,
        monitor: &Monitor,
        program: Program<File>,
        format: Format,
        geometry: VolumeInfo,
        entry: &Entry,
        block: &mut Option<NativeBlock>,
    ) -> Result<Self, String> {
        Self::launch(
            startup, root, monitor, program, format, geometry, entry, block, false,
        )
    }
    pub(super) fn prepare(
        startup: &Startup<'_>,
        root: &Directory,
        monitor: &Monitor,
        program: Program<File>,
        format: Format,
        geometry: VolumeInfo,
        entry: &Entry,
    ) -> Result<Self, String> {
        Self::launch(
            startup, root, monitor, program, format, geometry, entry, &mut None, true,
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn launch(
        startup: &Startup<'_>,
        root: &Directory,
        monitor: &Monitor,
        program: Program<File>,
        format: Format,
        geometry: VolumeInfo,
        entry: &Entry,
        block: &mut Option<NativeBlock>,
        adopting: bool,
    ) -> Result<Self, String> {
        monitor.check_capacity()?;
        let Program {
            path,
            source,
            image,
        } = program;
        let factory = startup.borrow(startup::TASK_FACTORY).map_err(show)?;
        let domain = create_resource_domain(
            startup.borrow(startup::RESOURCE_DOMAIN).map_err(show)?,
            LIMITS,
        )
        .map_err(show)?;
        let group = create_task_group(factory, domain.as_handle_ref()).map_err(show)?;
        // Environment paths stay anchored at the delegated formal root;
        // namespace paths may select a provider on any mounted volume.
        let system_environment = match source {
            Source::Environment => None,
            Source::Namespace => {
                Some(hyper_service::process::open_environment(root, "/").map_err(show)?)
            }
        };
        let system_libraries = system_environment
            .as_ref()
            .map(hyper_service::process::open_library_directory)
            .transpose()
            .map_err(show)?;
        let system_loader = system_environment
            .as_ref()
            .map(|environment| {
                hyper_service::process::open_loader(
                    environment,
                    hyper_service::process::LoaderKind::Dynamic,
                )
            })
            .transpose()
            .map_err(show)?;
        let loader = match &system_loader {
            Some(file) => file.as_handle_ref(),
            None => startup
                .borrow(hyper_service::process::CHILD_LOADER)
                .map_err(show)?,
        };
        let builder = ProcessBuilder::create(
            factory,
            group.as_handle_ref(),
            domain.as_handle_ref(),
            image.as_handle_ref(),
            loader,
        )
        .map_err(show)?;
        builder.set_name(format.worker_name()).map_err(show)?;
        // The worker locks its own current mappings before announcing READY.
        // No process-wide future-allocation policy is implied by this permission.
        builder.set_memory_lock_allowed(true).map_err(show)?;
        for argument in [
            path.as_str(),
            &geometry.sectors.to_string(),
            if entry.read_only { "ro" } else { "rw" },
        ] {
            builder.add_argument(argument).map_err(show)?;
        }
        if adopting {
            builder.add_argument("--adopt").map_err(show)?;
        }
        let libraries = system_libraries;
        let libraries = match libraries.as_ref() {
            Some(directory) => directory.as_handle_ref(),
            None => startup
                .borrow(hyper_service::process::CHILD_LIBRARY_DIRECTORY)
                .map_err(show)?,
        };
        builder
            .add_handle_duplicate(
                libraries,
                startup::DYNAMIC_LIBRARY_DIRECTORY.as_raw(),
                RightsOffer::Exact(Rights::READ.union(Rights::EXECUTE).union(Rights::PROTECT)),
            )
            .map_err(show)?;
        builder
            .add_handle_duplicate(
                domain.as_handle_ref(),
                startup::RESOURCE_DOMAIN.as_raw(),
                RightsOffer::Exact(Rights::RESOURCE_DOMAIN_SPONSOR),
            )
            .map_err(show)?;
        for (output, contract) in [
            (
                hyper_rt::process::stdout().map_err(show)?,
                hyper_service::stdio::STANDARD_OUTPUT_CONTRACT,
            ),
            (
                hyper_rt::process::stderr().map_err(show)?,
                hyper_service::stdio::STANDARD_ERROR_CONTRACT,
            ),
        ] {
            builder
                .add_handle_duplicate(
                    output.as_handle_ref(),
                    contract.purpose(),
                    RightsOffer::Exact(contract.required_rights()),
                )
                .map_err(show)?;
        }
        let (control, child_control) = CapabilityChannel::create().map_err(show)?;
        let (owner, child_owner) = hyper_os::channel::create_pair().map_err(show)?;
        builder
            .add_handle_move(
                child_control.into_handle(),
                filesystem::WORKER_CONTROL.as_raw(),
                RightsOffer::Exact(Rights::READ.union(Rights::WRITE).union(Rights::WAIT)),
            )
            .map_err(|error| show(error.error()))?;
        builder
            .add_handle_move(
                child_owner,
                filesystem::OWNER.as_raw(),
                RightsOffer::Exact(Rights::READ.union(Rights::WAIT)),
            )
            .map_err(|error| show(error.error()))?;
        // The exclusive block is the final setup transfer; failures before here
        // retain it in the manager and are retryable without reactivation.
        if !adopting {
            let device = block.take().ok_or("volume already delegated")?;
            if let Err(error) = builder.add_handle_move(
                device.into_handle(),
                filesystem::BLOCK.as_raw(),
                RightsOffer::Exact(hyper_os::block::RIGHTS),
            ) {
                let (error, handle) = error.into_parts();
                *block = Some(NativeBlock::from_handle(handle));
                return Err(show(error));
            }
        }
        let retirement = builder
            .retain_on_exit(8 * 1024 * 1024, 10_000_000_000)
            .map_err(show)?;
        builder.seal().map_err(show)?;
        let child = Arc::new(
            builder
                .start()
                .map_err(|failure| format!("worker start failed: {:?}", failure.error()))?,
        );
        let observation = match monitor.register(child.clone(), retirement, format, &entry.target) {
            Ok(observation) => observation,
            Err(error) => {
                let _ = child.as_process_supervisor().request_stop();
                return Err(error);
            }
        };
        let prepared = (|| -> Result<_, String> {
            let mut bytes = [MaybeUninit::uninit(); 16];
            let mut slots = [
                CapabilityReceiveSlot::new::<ByteChannelObject>(filesystem::TRANSPORT_RIGHTS),
                CapabilityReceiveSlot::new::<VmoObject>(filesystem::BUFFER_RIGHTS),
            ];
            let message = control
                .receive(deadline()?, &mut bytes, &mut slots)
                .map_err(show)?;
            if adopting {
                if message.bytes() != filesystem::WORKER_WAITING || message.capability_count() != 0
                {
                    return Err("invalid filesystem adoption preparation".into());
                }
                return Ok((None, None));
            }
            if message.bytes() != filesystem::WORKER_READY || message.capability_count() != 2 {
                return Err("invalid filesystem worker handshake".into());
            }
            let channel = slots[0]
                .take::<ByteChannelObject>()
                .map_err(show)?
                .ok_or("missing filesystem endpoint")?;
            let buffer = slots[1]
                .take::<VmoObject>()
                .map_err(show)?
                .ok_or("missing filesystem buffer")?;
            Ok((Some(channel), Some(buffer)))
        })();
        let (channel, buffer) = match prepared {
            Ok(value) => value,
            Err(error) => {
                // Capture spontaneous termination before cleanup requests could
                // otherwise replace an absent reason with Requested.
                let detail = observation.detail().unwrap_or_else(|failure| failure);
                drop(owner);
                drop(control);
                let _ = child.as_process_supervisor().request_stop();
                let _ = child.as_process_supervisor().wait_terminated(deadline()?);
                return Err(format!("{error}; {detail}"));
            }
        };
        Ok(Self {
            child,
            observation,
            _group: group,
            _domain: domain,
            control,
            owner,
            channel,
            buffer,
            published: false,
            mount_id: None,
            target: entry.target.clone(),
            format,
            read_only: entry.read_only,
        })
    }
    pub fn publish(&mut self, root: &Directory, read_only: bool) -> Result<(), String> {
        hyper_os::filesystem::mount(
            self.channel
                .as_ref()
                .ok_or("missing filesystem transport")?
                .as_handle_ref(),
            self.buffer
                .as_ref()
                .ok_or("missing filesystem buffer")?
                .as_handle_ref(),
            root.as_handle_ref(),
            &self.target,
            read_only,
        )
        .map_err(filesystem_error)?;
        self.published = true;
        self.mount_id = Some(
            root.open_directory(
                &self.target,
                hyper_os::fs::DirectoryRights::READ.union(hyper_os::fs::DirectoryRights::INSPECT),
            )
            .map_err(show)?
            .info()
            .map_err(show)?
            .location()
            .mount()
            .get(),
        );
        println!(
            "HypeR fs-backend: {} mounted at {}",
            self.format.name(),
            self.target
        );
        Ok(())
    }
    pub(super) fn adopt_transport(
        &mut self,
        record: &[u8],
        block: &mut Option<OwnedHandle<NativeBlockObject>>,
        server: &mut Option<OwnedHandle<ByteChannelObject>>,
        shared: &mut Option<OwnedHandle<VmoObject>>,
        channel: OwnedHandle<ByteChannelObject>,
        buffer: OwnedHandle<VmoObject>,
    ) -> Result<(), String> {
        use hyper_os::capability_channel::CapabilityDisposition;
        let mut offers = [
            CapabilityDisposition::move_handle(block, RightsOffer::Exact(hyper_os::block::RIGHTS))
                .map_err(show)?,
            CapabilityDisposition::move_handle(
                server,
                RightsOffer::Exact(filesystem::TRANSPORT_RIGHTS),
            )
            .map_err(show)?,
            CapabilityDisposition::move_handle(
                shared,
                RightsOffer::Exact(filesystem::SERVER_BUFFER_RIGHTS),
            )
            .map_err(show)?,
        ];
        hyper_service::io::send_capabilities(&self.control, record, &mut offers, deadline()?)
            .map_err(show)?;
        let mut bytes = [MaybeUninit::uninit(); 16];
        let reply = self
            .control
            .receive(deadline()?, &mut bytes, &mut [])
            .map_err(show)?;
        if reply.bytes() != filesystem::WORKER_RESUMED || reply.capability_count() != 0 {
            return Err("filesystem worker failed to adopt transport".into());
        }
        self.channel = Some(channel);
        self.buffer = Some(buffer);
        Ok(())
    }

    pub fn finish(self) -> Result<NativeBlock, String> {
        self.observation.begin_stop()?;
        self.owner
            .as_byte_channel()
            .send(filesystem::WORKER_STOP)
            .map_err(show)?;
        let mut bytes = [MaybeUninit::uninit(); 16];
        let mut slots = [CapabilityReceiveSlot::new::<NativeBlockObject>(
            hyper_os::block::RIGHTS,
        )];
        let message = self
            .control
            .receive(deadline()?, &mut bytes, &mut slots)
            .map_err(show)?;
        if message.bytes() != filesystem::WORKER_RETURN || message.capability_count() != 1 {
            return Err("worker did not return exclusive block ownership".into());
        }
        let block = slots[0]
            .take::<NativeBlockObject>()
            .map_err(show)?
            .ok_or("worker omitted block owner")?;
        let supervisor = self.child.as_process_supervisor();
        let retired_by = deadline()?;
        supervisor.wait_terminated(retired_by).map_err(show)?;
        // Termination stops execution before the reaper drops the worker's
        // handles and address space. Reuse only after that cleanup completes.
        while supervisor.info().map_err(show)?.phase != ProcessPhase::Retired {
            let now = hyper_os::time::monotonic_now()
                .map_err(show)?
                .as_nanoseconds();
            if now >= retired_by {
                return Err("filesystem worker retirement timed out".into());
            }
            hyper_os::thread::sleep_until(now.saturating_add(1_000_000).min(retired_by))
                .map_err(show)?;
        }
        Ok(NativeBlock::from_handle(block))
    }
}
