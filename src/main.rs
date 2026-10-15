// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Resident mount manager. Exclusive media moves to one format worker per
//! mounted volume; filesystem requests bypass this control plane entirely.

mod manager;
mod migration;
mod process;

use hyper_fs_backend::format;

use core::mem::MaybeUninit;
use hyper_os::{
    capability_channel::CapabilityReceiveSlot,
    fs::Directory,
    handle::{ByteChannelObject, Rights},
    startup,
};
use hyper_service::filesystem;

fn show(error: impl core::fmt::Debug) -> String {
    format!("{error:?}")
}
fn filesystem_error(error: hyper_os::Error) -> String {
    use hyper_os::{Error, Status};
    let message = match error {
        Error::Status(Status::BUSY) => {
            "filesystem is in use; close files, directory capabilities or nested mounts and retry"
        }
        Error::Status(Status::ACCESS_DENIED) => "mount authority denied",
        Error::Status(Status::READ_ONLY) => "filesystem or volume is read-only",
        Error::Status(Status::NOT_FOUND) => "mount target or volume was not found",
        Error::Status(Status::NOT_SUPPORTED) => "filesystem operation is not supported",
        Error::Status(Status::TIMED_OUT) => "filesystem worker did not respond before the deadline",
        Error::Status(Status::PEER_CLOSED) => "filesystem worker disconnected",
        Error::Status(Status::IO_ERROR) => "filesystem backend reported an I/O failure",
        Error::Status(Status::INVALID_ARGUMENT) => "invalid mount target or filesystem request",
        _ => return show(error),
    };
    message.to_string()
}
fn deadline() -> Result<u64, String> {
    hyper_os::time::deadline_after(std::time::Duration::from_secs(120))
        .map(|value| value.as_raw())
        .map_err(show)
}

fn run() -> Result<(), String> {
    let mut startup = hyper_rt::process::startup().map_err(show)?;
    hyper_os::memory::lock_current_memory(startup.borrow(startup::ROOT_VMAR).map_err(show)?)
        .map_err(show)?;
    let root = Directory::from_handle(startup.take(startup::ROOT_DIRECTORY).map_err(show)?);
    let environment = Directory::from_handle(
        startup
            .take(hyper_service::process::ENVIRONMENT_DIRECTORY)
            .map_err(show)?,
    );
    let mut manager = manager::Manager {
        startup,
        root,
        environment,
        volumes: Vec::new(),
    };
    let adoption = migration::adopt(&mut manager)?;
    let control = &adoption.control;
    let mut record = [MaybeUninit::uninit(); filesystem::ATTACH_BYTES];
    loop {
        let mut slots = [CapabilityReceiveSlot::new::<ByteChannelObject>(
            Rights::READ.union(Rights::WRITE).union(Rights::WAIT),
        )];
        let message = match control.receive(hyper_os::DEADLINE_INFINITE, &mut record, &mut slots) {
            Ok(message) => message,
            Err(hyper_os::Error::Status(status))
                if matches!(
                    status,
                    hyper_os::Status::INVALID_ARGUMENT
                        | hyper_os::Status::BAD_HANDLE
                        | hyper_os::Status::ACCESS_DENIED
                        | hyper_os::Status::BUFFER_TOO_SMALL
                        | hyper_os::Status::FAULT
                ) =>
            {
                // A rejected sender transaction installs no capabilities and
                // must not terminate unrelated mounted filesystem workers.
                eprintln!("HypeR fs-backend: rejected control message: {status:?}");
                continue;
            }
            Err(error) => return Err(show(error)),
        };
        if message.bytes() != filesystem::CONTROL_CONNECT || message.capability_count() != 1 {
            continue;
        }
        let Some(connection) = slots[0].take::<ByteChannelObject>().map_err(show)? else {
            continue;
        };
        let result = (|| {
            hyper_os::wait::wait_many(
                &[hyper_os::wait::WaitItem::new(
                    connection.as_handle_ref(),
                    hyper_os::wait::ObjectSignals::<ByteChannelObject>::READABLE
                        .union(hyper_os::wait::ObjectSignals::<ByteChannelObject>::PEER_CLOSED),
                )],
                deadline()?,
            )
            .map_err(show)?;
            let mut bytes = vec![0; filesystem::control::MAX_BYTES];
            let count = connection
                .as_byte_channel()
                .try_receive(&mut bytes)
                .map_err(show)?;
            let request =
                filesystem::control::decode(&bytes[..count]).ok_or("invalid mount command")?;
            manager.command(request)
        })();
        let response = match result {
            Ok(text) => format!("OK\n{text}"),
            Err(error) => format!("ERROR\n{error}"),
        };
        let response = if response.len() > filesystem::control::MAX_BYTES {
            "ERROR\nmount response exceeds size limit".to_string()
        } else {
            response
        };
        if let Err(error) = connection.as_byte_channel().send(response.as_bytes()) {
            eprintln!("HypeR fs-backend: response lost: {error:?}");
        }
    }
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("HypeR fs-backend: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
