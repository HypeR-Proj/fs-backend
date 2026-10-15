// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Versioned service-private transfer description. This is not a kernel ABI.
use hyper_os::{
    handle::{CapabilityChannelObject, Rights, VmoObject},
    startup::StartupPurpose,
};
use serde::{Deserialize, Serialize};

pub const ENDPOINT: StartupPurpose<CapabilityChannelObject> = StartupPurpose::new(0x8008_0011);
pub const DESCRIPTION: StartupPurpose<VmoObject> = StartupPurpose::new(0x8008_0013);
pub const DESCRIBE: &[u8] = b"HYPER-FS-DESCRIBE/1";
pub const PREPARED: &[u8] = b"HYPER-FS-PREPARED/1";
pub const FINISH: &[u8] = b"HYPER-FS-CONTROL/1";
pub const VOLUME: &[u8] = b"HYPER-FS-VOLUME/1";
pub const MAX_BYTES: usize = 64 * 1024;
pub const PROCESS_RIGHTS: Rights = Rights::TRANSFER
    .union(Rights::WAIT)
    .union(Rights::INSPECT)
    .union(Rights::REQUEST_STOP);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Description {
    pub version: u32,
    pub volumes: Vec<Volume>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Volume {
    pub id: [u8; 16],
    pub label: String,
    pub sectors: u64,
    pub read_only: bool,
    pub mounted: Option<Mount>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mount {
    pub mount_id: u64,
    pub target: String,
    pub format: String,
    pub read_only: bool,
}
impl Description {
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_BYTES {
            return Err("filesystem transfer description too large".into());
        }
        let value: Self = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        if value.version != 1 || value.volumes.len() > 128 {
            return Err("unsupported filesystem transfer description".into());
        }
        for (index, volume) in value.volumes.iter().enumerate() {
            if hyper_service::filesystem::encode_attach(
                hyper_block::Guid(volume.id),
                &volume.label,
                hyper_os::block::VolumeInfo {
                    sectors: volume.sectors,
                    read_only: volume.read_only,
                },
            )
            .is_none()
                || value.volumes[..index].iter().any(|old| old.id == volume.id)
            {
                return Err("invalid transferred volume identity".into());
            }
            if let Some(mount) = &volume.mounted
                && (!crate::config::absolute_path(&mount.target)
                    || mount.mount_id == 0
                    || !matches!(mount.format.as_str(), "fat" | "erofs")
                    || (volume.read_only || mount.format == "erofs") && !mount.read_only
                    || value.volumes[..index].iter().any(|old| {
                        old.mounted.as_ref().is_some_and(|old| {
                            old.target == mount.target || old.mount_id == mount.mount_id
                        })
                    }))
            {
                return Err("invalid transferred mount".into());
            }
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn description() -> serde_json::Value {
        serde_json::json!({"version":1,"volumes":[{"id":[1,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"label":"hyper-system",
            "sectors":4096,"read_only":true,"mounted":{"mount_id":17,"target":"/system","format":"erofs","read_only":true}}]})
    }
    fn accepted(value: &serde_json::Value) -> bool {
        Description::parse(value.to_string().as_bytes()).is_ok()
    }
    #[test]
    fn validates_complete_mount_inventory_before_admitting_transfer() {
        let value = description();
        assert!(accepted(&value));
        let mut value = description();
        value["version"] = 2.into();
        assert!(!accepted(&value));
        let mut value = description();
        value["volumes"][0]["mounted"]["read_only"] = false.into();
        assert!(!accepted(&value));
        let mut value = description();
        value["volumes"][0]["mounted"]["format"] = "unknown".into();
        assert!(!accepted(&value));
        let mut value = description();
        value["volumes"][0]["sectors"] = 0.into();
        assert!(!accepted(&value));
        for target in ["/system/../boot", "relative"] {
            let mut value = description();
            value["volumes"][0]["mounted"]["target"] = target.into();
            assert!(!accepted(&value));
        }
    }
    #[test]
    fn rejects_duplicate_mounts_and_volume_identities() {
        let mut value = description();
        let first = value["volumes"][0].clone();
        if let Some(volumes) = value["volumes"].as_array_mut() {
            volumes.push(first);
        }
        assert!(!accepted(&value));
        value["volumes"][1]["id"][0] = 2.into();
        assert!(!accepted(&value));
        value["volumes"][1]["mounted"] = serde_json::Value::Null;
        assert!(accepted(&value));
    }
    #[test]
    fn bounds_metadata_and_rejects_unknown_fields() {
        assert!(Description::parse(&vec![b' '; MAX_BYTES + 1]).is_err());
        let mut value = description();
        value["hidden"] = true.into();
        assert!(!accepted(&value));
        assert!(Description::parse(b"{").is_err());
    }
}
