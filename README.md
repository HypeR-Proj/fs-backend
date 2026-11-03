<!--
SPDX-FileCopyrightText: 2026 roolrz
SPDX-License-Identifier: Apache-2.0
-->

# fs-backend

A HypeR component. See [HypeR](https://github.com/HypeR-Proj/HypeR) for
workspace setup and the installed SDK build boundary.

## Workspace tests

Use the [HypeR workspace](https://github.com/HypeR-Proj/HypeR) to resolve dependencies:

```sh
python3 project.py test fs-backend
```

Paths in the documentation are relative to the repo-managed workspace root.
Edit this repository and prepare the workspace again before building.


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
