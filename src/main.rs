// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Resident mount manager. Exclusive media moves to one format worker per
//! mounted volume; filesystem requests bypass this control plane entirely.

use hyper_fs_backend::admission;
mod manager;
mod migration;
mod monitor;
mod process;

use hyper_fs_backend::format;

use hyper_os::{fs::Directory, startup};
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
    let monitor = monitor::Monitor::start(&startup)?;
    let mut manager = manager::Manager {
        startup,
        root,
        environment,
        volumes: Vec::new(),
        monitor,
    };
    let (control, _adoption) = migration::adopt(&mut manager)?;
    let listener = admission::Listener::start(control).map_err(show)?;
    loop {
        let admission::Request { connection, bytes } = listener.receive().map_err(show)?;
        let result = filesystem::control::decode(&bytes)
            .ok_or_else(|| "invalid mount command".to_string())
            .and_then(|request| manager.command(request));
        let response = match result {
            Ok(text) => format!("OK\n{text}"),
            Err(error) => format!("ERROR\n{error}"),
        };
        let response = if response.len() > filesystem::control::MAX_BYTES {
            "ERROR\nmount response exceeds size limit".to_string()
        } else {
            response
        };
        let reply_until = hyper_os::time::deadline_after(std::time::Duration::from_secs(5))
            .map_err(show)?
            .as_raw();
        if let Err(error) = connection
            .as_byte_channel()
            .send_until(response.as_bytes(), reply_until)
        {
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
