<!--
SPDX-FileCopyrightText: 2026 roolrz
SPDX-License-Identifier: Apache-2.0
-->

# fs-backend

Resident filesystem mount manager.

Owns mount configuration, worker selection, admission and publication. Each mounted
volume retains its exclusive NativeBlock in the manager and delegates one revocable
BlockQueue to its format worker. Ordinary file requests bypass this control plane.

See the [implementation entry](src/main.rs) and [component declaration](component.json)
for the public interfaces, build dependencies and delivered artifacts.

## Boundaries

Workers implement media semantics. Mount and unmount require manager authority; open
references and nested mounts can prevent graceful detach. Bootstrap has a separate
static implementation rather than a dependency on this repository.

## Build and test

Use this checkout inside the [repo-managed workspace](https://github.com/HypeR-Proj/HypeR).
From this repository directory:

```sh
make
make test
```

An absolute `OUT=/path/to/out` selects a shared output directory. The module
entry delegates to build/system, which prepares the installed SDK and declared
dependencies. Native binaries require HypeR; host module checks cover the tests
selected by component.json. A standalone clone does not contain the workspace’s
other source repositories or Native SDK.



The manager recognizes F2FS superblocks and starts
`/svc/fs-f2fs` when that optional worker is installed. It preserves the ordinary
worker memory ceiling while admitting the media cache's additional heap mappings.
F2FS volumes participate in the same handoff and mount-table protocol.

## Control admission

A malformed control request or rejected matched transfer is discarded without
withdrawing the shared mount listener. Receiver invocation and endpoint failures
are returned to the service owner. An accepted private mount handshake remains
separate from the public transfer and owns its temporary connection resources.

At most eight private first-command workers run concurrently. Each has a
five-second deadline measured from public admission, and completed commands enter
an eight-entry queue. Excess handshakes receive a best-effort capacity error and
close. Expiry or peer closure releases the worker and connection. The manager
alone executes commands and mutates mount state. Global bounds do not provide
per-client fairness. Dropping the listener cancels and joins all pending workers.

Control replies use a finite five-second send deadline. A client which fills its
return queue and stops reading cannot retain the mount-state owner indefinitely.
Expiry closes that private reply connection and does not undo an applied command.

## Cancellation and partial mount results

The mount owner checks a private client's closure after dequeue and after
read-only preparation, before creating directories or starting a worker. That
check accepts one mount operation. Unmount checks before namespace detachment.
After acceptance, loss of the reply does not undo the operation or bypass worker
and media retirement. Inspect `mount` and the requested target before retrying.

`mount -a` is a sequence of independent entries, not a transaction. Earlier
mounts survive a later error or client cancellation; closure stops accepting
subsequent entries. Retrying the same table skips matching live mounts; it must
not assume that an error or timeout rolled back earlier entries.

## Failed-provider recovery

`umount --force TARGET` is an explicitly lossy recovery command for a permanently
failed provider. Healthy providers, the namespace root and mounts containing nested
mounts are refused. Ordinary `umount` still requires idle references and a successful
synchronous flush; it never discards dirty pages to report success.

The manager detaches the exact failed mount identity, records discarded VFS bytes
(additional filesystem metadata loss remains unknown), stops and retires the worker,
and revokes its queue. Only successful physical queue retirement permits a fresh
queue and a later `mount`. Device failure, timeout or cancellation leaves the volume
unavailable. Old references remain failed, and old queue aliases cannot submit into
the replacement. NativeBlock never moves into a formal worker. Bootstrap transfers
its original NativeBlock into this manager while the existing transport is adopted.

Worker death and recovery stages produce authenticated CRITICAL diagnostics,
independent of the component's critical setting. Reporting runs outside mount-control
ownership; the component manager retains its direct Console fallback when journal
or filesystem services fail. `mount` retains the latest recovery record per volume,
including mount, volume, worker identity/generation, stage, loss and reuse status.
