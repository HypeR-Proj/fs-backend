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


The `feat/fuchsia-f2fs` working branch also recognizes F2FS superblocks and starts
`/svc/fs-f2fs` when that optional worker is installed. It preserves the ordinary
worker memory ceiling while admitting the media cache's additional heap mappings.
F2FS volumes participate in the same handoff and mount-table protocol.
