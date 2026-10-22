# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

$(eval $(call native-rust-app,svc/fs-backend,hyper-fs-backend-system,fs-backend-system,svc/fs-backend,development))
