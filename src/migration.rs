// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Adopt unchanged mount transports after all replacement code is resident.
use crate::{
    deadline,
    manager::{Manager, Volume},
    process::Worker,
    show,
};
use core::mem::MaybeUninit;
use hyper_fs_backend::{config::Config, format::Format, fstab::Entry, transfer, worker};
use hyper_os::{
    block::{NativeBlock, VolumeInfo},
    capability_channel::{CapabilityChannel, CapabilityDisposition, CapabilityReceiveSlot},
    handle::{
        BlockQueueObject, ByteChannelObject, CapabilityChannelObject, NativeBlockObject,
        OwnedHandle, ProcessObject, Rights, RightsOffer, VmoObject,
    },
    memory::SnapshotVmo,
    startup::StartupPurpose,
};
use hyper_service::{filesystem, io::send_capabilities};

const READY: StartupPurpose<ByteChannelObject> = StartupPurpose::new(0x8008_0020);
const OWNERS: StartupPurpose<CapabilityChannelObject> = StartupPurpose::new(0x8008_0021);

pub(super) struct AdoptedManager {
    pub control: CapabilityChannel,
    _unretired_predecessors: Vec<Option<OwnedHandle<ProcessObject>>>,
}

pub(super) fn adopt(manager: &mut Manager<'_>) -> Result<AdoptedManager, String> {
    let descriptor = manager.startup.take(transfer::DESCRIPTION).map_err(show)?;
    let snapshot = SnapshotVmo::from_vmo(descriptor.as_handle_ref()).map_err(show)?;
    let mut length = [0; 8];
    snapshot.read_exact_at(0, &mut length).map_err(show)?;
    let length = usize::try_from(u64::from_le_bytes(length)).map_err(show)?;
    if length > transfer::MAX_BYTES {
        return Err("filesystem adoption description too large".into());
    }
    let mut bytes = vec![0; length];
    snapshot.read_exact_at(8, &mut bytes).map_err(show)?;
    let description = transfer::Description::parse(&bytes)?;
    drop(snapshot);
    drop(descriptor);
    let endpoint =
        CapabilityChannel::from_handle(manager.startup.take(transfer::ENDPOINT).map_err(show)?);
    let owners = CapabilityChannel::from_handle(manager.startup.take(OWNERS).map_err(show)?);
    let ready = manager.startup.take(READY).map_err(show)?;
    let config = Config::load(&manager.environment)?;
    // Prepare every worker while its source mount still serves requests. A
    // failed launch drops only replacement groups; predecessors remain active.
    for item in description.volumes {
        let geometry = VolumeInfo {
            sectors: item.sectors,
            read_only: item.read_only,
        };
        let worker = if let Some(mount) = item.mounted {
            let format = match mount.format.as_str() {
                "fat" => Format::Fat,
                "erofs" => Format::Erofs,
                "f2fs" => Format::F2fs,
                _ => return Err("unsupported adopted filesystem format".into()),
            };
            let target = adopted_target(&manager.root, &mount)?;
            let entry = Entry {
                source: format!("PARTUUID={}", hyper_block::Guid(item.id)),
                target,
                format: Some(format),
                read_only: mount.read_only,
                automatic: true,
                required: true,
            };
            let program = worker::open(&config, format, &entry.target, |source, path| {
                let directory = match source {
                    worker::Source::Environment => &manager.environment,
                    worker::Source::Namespace => &manager.root,
                };
                directory.open(path, hyper_service::process::EXECUTABLE_FILE_RIGHTS)
            })?;
            let mut worker = Worker::prepare(
                &manager.startup,
                &manager.root,
                &manager.monitor,
                program,
                format,
                geometry,
                &entry,
            )?;
            worker.mount_id = Some(mount.mount_id);
            worker.published = true;
            Some(worker)
        } else {
            None
        };
        manager.volumes.push(Volume {
            id: hyper_block::Guid(item.id),
            label: item.label,
            geometry,
            block: None,
            worker,
        });
    }
    send_capabilities(&endpoint, transfer::PREPARED, &mut [], deadline()?).map_err(show)?;
    let mut predecessors = Vec::new();
    for volume in &mut manager.volumes {
        let mut bytes = [MaybeUninit::uninit(); 64];
        if let Some(worker) = &mut volume.worker {
            let mut slots = [
                CapabilityReceiveSlot::new::<NativeBlockObject>(hyper_os::block::RIGHTS),
                CapabilityReceiveSlot::new::<ByteChannelObject>(filesystem::TRANSPORT_RIGHTS),
                CapabilityReceiveSlot::new::<VmoObject>(filesystem::SERVER_BUFFER_RIGHTS),
                CapabilityReceiveSlot::new::<ByteChannelObject>(filesystem::TRANSPORT_RIGHTS),
                CapabilityReceiveSlot::new::<VmoObject>(filesystem::BUFFER_RIGHTS),
                CapabilityReceiveSlot::new::<ProcessObject>(transfer::PROCESS_RIGHTS),
                CapabilityReceiveSlot::new::<BlockQueueObject>(filesystem::QUEUE_RIGHTS),
            ];
            let message = endpoint
                .receive(deadline()?, &mut bytes, &mut slots)
                .map_err(show)?;
            if message.bytes().len() != 16
                || &message.bytes()[..8] != filesystem::WORKER_STATE
                || message.capability_count() != 7
            {
                return Err("invalid mounted filesystem transfer".into());
            }
            let mut block = slots[0].take::<NativeBlockObject>().map_err(show)?;
            let mut server = slots[1].take::<ByteChannelObject>().map_err(show)?;
            let mut shared = slots[2].take::<VmoObject>().map_err(show)?;
            let mut queue = slots[6].take::<BlockQueueObject>().map_err(show)?;
            let channel = slots[3]
                .take::<ByteChannelObject>()
                .map_err(show)?
                .ok_or("missing client endpoint")?;
            let buffer = slots[4]
                .take::<VmoObject>()
                .map_err(show)?
                .ok_or("missing client buffer")?;
            let process = slots[5]
                .take::<ProcessObject>()
                .map_err(show)?
                .ok_or("missing old filesystem worker")?;
            worker.adopt_transport(
                message.bytes(),
                &mut block,
                &mut server,
                &mut shared,
                &mut queue,
                (channel, buffer),
            )?;
            predecessors.push(Some(process));
        } else {
            let mut slots = [CapabilityReceiveSlot::new::<NativeBlockObject>(
                hyper_os::block::RIGHTS,
            )];
            let message = endpoint
                .receive(deadline()?, &mut bytes, &mut slots)
                .map_err(show)?;
            if message.bytes() != transfer::VOLUME || message.capability_count() != 1 {
                return Err("invalid unmounted volume transfer".into());
            }
            volume.block = Some(NativeBlock::from_handle(
                slots[0]
                    .take::<NativeBlockObject>()
                    .map_err(show)?
                    .ok_or("missing volume block")?,
            ));
        }
    }
    let mut bytes = [MaybeUninit::uninit(); 64];
    let mut slots = [CapabilityReceiveSlot::new::<CapabilityChannelObject>(
        Rights::READ.union(Rights::WAIT).union(Rights::TRANSFER),
    )];
    let message = endpoint
        .receive(deadline()?, &mut bytes, &mut slots)
        .map_err(show)?;
    if message.bytes() != transfer::FINISH || message.capability_count() != 1 {
        return Err("filesystem manager transfer did not commit".into());
    }
    let control = CapabilityChannel::from_handle(
        slots[0]
            .take::<CapabilityChannelObject>()
            .map_err(show)?
            .ok_or("missing mount administrator endpoint")?,
    );
    // Export supervision; stopping predecessors is deliberately not this
    // service's decision. System init also owns the old manager process.
    if let Err(error) = export_predecessors(&owners, &mut predecessors) {
        eprintln!("HypeR fs-backend: ownership adopted, retirement receiver unavailable: {error}");
    }
    // FINISH already committed the live filesystem transport. A late init
    // timeout cannot reverse that move, and notification loss must not tear
    // down an otherwise healthy mount. Retain any unexported old Process
    // owners until the supervisor explicitly retires this service.
    if let Err(error) = ready.as_byte_channel().send(b"HYPER-SERVICE-ADOPTED/1") {
        eprintln!("HypeR fs-backend: ownership adopted, readiness receiver unavailable: {error:?}");
    }
    println!("HypeR fs-backend: filesystem ownership adopted");
    Ok(AdoptedManager {
        control,
        _unretired_predecessors: predecessors,
    })
}

fn export_predecessors(
    owners: &CapabilityChannel,
    predecessors: &mut [Option<OwnedHandle<ProcessObject>>],
) -> Result<(), String> {
    for process in predecessors {
        let mut offers = [CapabilityDisposition::move_handle(
            process,
            RightsOffer::Exact(transfer::PROCESS_RIGHTS),
        )
        .map_err(show)?];
        send_capabilities(owners, b"HYPER-BOOT-OWNER/1", &mut offers, deadline()?).map_err(show)?;
    }
    send_capabilities(owners, b"HYPER-BOOT-OWNERS-END/1", &mut [], deadline()?).map_err(show)
}

/// A namespace root switch changes paths, not mount identity. Only the exact
/// adopted root mount may become `/`; no ordinary administrator can mount there.
fn adopted_target(
    root: &hyper_os::fs::Directory,
    mount: &transfer::Mount,
) -> Result<String, String> {
    if root.info().map_err(show)?.location().mount().get() == mount.mount_id {
        return Ok("/".into());
    }
    let directory = root
        .open_directory(
            &mount.target,
            hyper_os::fs::DirectoryRights::READ.union(hyper_os::fs::DirectoryRights::INSPECT),
        )
        .map_err(show)?;
    if directory.info().map_err(show)?.location().mount().get() != mount.mount_id {
        return Err("filesystem handoff mount identity changed".into());
    }
    Ok(mount.target.clone())
}
