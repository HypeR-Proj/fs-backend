// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded first-command workers; only the caller mutates mount state.

use core::mem::MaybeUninit;
use hyper_os::capability_channel::{CapabilityChannel, CapabilityReceiveSlot};
use hyper_os::handle::{ByteChannelObject, OwnedHandle, Rights};
use hyper_os::wait::{ObjectSignals, WaitItem, wait_many};
use hyper_os::{Error, Result, Status};
use hyper_service::filesystem;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread::JoinHandle;
use std::time::Duration;

const HANDSHAKES: usize = 8;
const FIRST_COMMAND_SECONDS: u64 = 5;
const CANCEL_POLL: Duration = Duration::from_millis(250);

pub struct Request {
    pub connection: OwnedHandle<ByteChannelObject>,
    pub bytes: Vec<u8>,
}

pub struct Listener {
    ready: Option<mpsc::Receiver<Result<Request>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Listener {
    pub fn start(endpoint: CapabilityChannel) -> Result<Self> {
        let (send, ready) = mpsc::sync_channel(HANDSHAKES);
        let stop = Arc::new(AtomicBool::new(false));
        let canceled = stop.clone();
        let worker = std::thread::Builder::new()
            .name("mount-admit".into())
            .spawn(move || {
                let mut workers: Vec<JoinHandle<()>> = Vec::new();
                while !canceled.load(Ordering::Acquire) {
                    if let Err(error) = reap(&mut workers) {
                        let _ = send.send(Err(error));
                        break;
                    }
                    let connection = match accept(&endpoint) {
                        Ok(Some(connection)) => connection,
                        Ok(None) | Err(Error::Status(Status::TIMED_OUT)) => continue,
                        Err(error) => {
                            let _ = send.send(Err(error));
                            break;
                        }
                    };
                    if let Err(error) = reap(&mut workers) {
                        let _ = send.send(Err(error));
                        break;
                    }
                    if workers.len() == HANDSHAKES {
                        let _ = connection
                            .as_byte_channel()
                            .try_send(b"ERROR\nmount admission capacity reached");
                        continue;
                    }
                    let limit = match hyper_os::time::deadline_after(Duration::from_secs(
                        FIRST_COMMAND_SECONDS,
                    )) {
                        Ok(limit) => limit.as_raw(),
                        Err(error) => {
                            let _ = send.send(Err(error));
                            break;
                        }
                    };
                    let send = send.clone();
                    let canceled = canceled.clone();
                    match std::thread::Builder::new()
                        .name("mount-first-command".into())
                        .spawn(move || match first_command(&connection, limit, &canceled) {
                            Ok(Some(bytes)) => {
                                let _ = send.send(Ok(Request { connection, bytes }));
                            }
                            Ok(None) => {}
                            Err(Error::Status(Status::TIMED_OUT)) => {
                                let _ = connection
                                    .as_byte_channel()
                                    .try_send(b"ERROR\nmount request timed out");
                            }
                            Err(Error::Status(Status::PEER_CLOSED)) => {}
                            Err(Error::MessageTooLarge { .. }) => {
                                let _ = connection
                                    .as_byte_channel()
                                    .try_send(b"ERROR\nmount request exceeds size limit");
                            }
                            Err(error) => {
                                let _ = send.send(Err(error));
                            }
                        }) {
                        Ok(worker) => workers.push(worker),
                        Err(error) => eprintln!("mount admission worker unavailable: {error}"),
                    }
                }
                // No published mount is owned by these workers. Stop their finite
                // receive slices and recover every private connection before exit.
                canceled.store(true, Ordering::Release);
                for worker in workers {
                    if worker.join().is_err() {
                        eprintln!("mount admission worker terminated unexpectedly");
                    }
                }
            })
            .map_err(|_| Error::Status(Status::NO_MEMORY))?;
        Ok(Self {
            ready: Some(ready),
            stop,
            worker: Some(worker),
        })
    }

    pub fn receive(&self) -> Result<Request> {
        self.ready
            .as_ref()
            .ok_or(Error::InvalidResponse)?
            .recv()
            .map_err(|_| Error::Status(Status::PEER_CLOSED))?
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Unblock workers queued behind a caller which is no longer receiving.
        self.ready.take();
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            eprintln!("mount admission owner terminated unexpectedly");
        }
    }
}

fn accept(endpoint: &CapabilityChannel) -> Result<Option<OwnedHandle<ByteChannelObject>>> {
    let mut bytes = [MaybeUninit::uninit(); filesystem::ATTACH_BYTES];
    let mut slots = [CapabilityReceiveSlot::new::<ByteChannelObject>(
        Rights::READ.union(Rights::WRITE).union(Rights::WAIT),
    )];
    let until = hyper_os::time::deadline_after(CANCEL_POLL)?.as_raw();
    let message = match endpoint.receive(until, &mut bytes, &mut slots) {
        Ok(message) => message,
        Err(Error::ReceiveRejected(_)) => return Ok(None),
        Err(error) => return Err(error),
    };
    if message.bytes() != filesystem::CONTROL_CONNECT || message.capability_count() != 1 {
        return Ok(None);
    }
    Ok(Some(slots[0].take()?.ok_or(Error::MissingHandle)?))
}

fn first_command(
    connection: &OwnedHandle<ByteChannelObject>,
    limit: u64,
    canceled: &AtomicBool,
) -> Result<Option<Vec<u8>>> {
    let mut bytes = vec![0; filesystem::control::MAX_BYTES];
    while !canceled.load(Ordering::Acquire) {
        if hyper_os::time::monotonic_now()?.as_nanoseconds() >= limit {
            return Err(Error::Status(Status::TIMED_OUT));
        }
        match connection.as_byte_channel().try_receive(&mut bytes) {
            Ok(length) => {
                bytes.truncate(length);
                return Ok(Some(bytes));
            }
            Err(Error::Status(Status::WOULD_BLOCK)) => {}
            Err(error) => return Err(error),
        }
        let until = hyper_os::time::deadline_after(CANCEL_POLL)?
            .as_raw()
            .min(limit);
        match wait_many(
            &[WaitItem::new(
                connection.as_handle_ref(),
                ObjectSignals::<ByteChannelObject>::READABLE
                    .union(ObjectSignals::<ByteChannelObject>::PEER_CLOSED),
            )],
            until,
        ) {
            Ok(_) | Err(Error::Status(Status::TIMED_OUT)) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

fn reap(workers: &mut Vec<JoinHandle<()>>) -> Result<()> {
    let mut index = 0;
    while index < workers.len() {
        if workers[index].is_finished() {
            workers
                .swap_remove(index)
                .join()
                .map_err(|_| Error::Status(Status::INTERNAL))?;
        } else {
            index += 1;
        }
    }
    Ok(())
}
