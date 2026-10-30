// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded fstab policy, independent of installed filesystem implementations.

use crate::format::Format;
#[cfg(target_os = "hyper")]
use std::io::Read;

pub const PATH: &str = "etc/fstab";
pub const MAX_BYTES: usize = 64 * 1024;
pub const MAX_ENTRIES: usize = 128;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    pub source: String,
    pub target: String,
    pub format: Option<Format>,
    pub read_only: bool,
    pub automatic: bool,
    pub required: bool,
}

pub fn format(name: &str) -> Result<Option<Format>, String> {
    match name {
        "auto" => Ok(None),
        "fat" | "vfat" => Ok(Some(Format::Fat)),
        "erofs" => Ok(Some(Format::Erofs)),
        "f2fs" => Ok(Some(Format::F2fs)),
        _ => Err(format!("unsupported filesystem type: {name}")),
    }
}

pub fn options(value: &str) -> Result<(bool, bool, bool), String> {
    let (mut read_only, mut automatic, mut required) = (false, true, true);
    for option in value.split(',') {
        match option {
            "defaults" => {}
            "ro" => read_only = true,
            "rw" => read_only = false,
            "noauto" => automatic = false,
            "auto" => automatic = true,
            "nofail" => required = false,
            _ => return Err(format!("unsupported mount option: {option}")),
        }
    }
    Ok((read_only, automatic, required))
}

#[cfg(target_os = "hyper")]
pub fn load(directory: &hyper_os::fs::Directory, path: &str) -> Result<Vec<Entry>, String> {
    let file = directory
        .open(path, hyper_os::fs::FileRights::READ)
        .map_err(|error| format!("open {path}: {error}"))?
        .into_std();
    let mut data = Vec::new();
    file.take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut data)
        .map_err(|error| error.to_string())?;
    parse(&data)
}

fn field(value: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let digits: String = chars.by_ref().take(3).collect();
        out.push(match digits.as_str() {
            "040" => ' ',
            "011" => '\t',
            "134" => '\\',
            _ => return Err("unsupported fstab escape".into()),
        });
    }
    Ok(out)
}

pub fn parse(data: &[u8]) -> Result<Vec<Entry>, String> {
    if data.len() > MAX_BYTES {
        return Err("fstab exceeds 64 KiB".into());
    }
    let text = std::str::from_utf8(data).map_err(|_| "fstab is not UTF-8")?;
    let mut entries: Vec<Entry> = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<_> = line
            .split_whitespace()
            .take_while(|part| !part.starts_with('#'))
            .collect();
        if parts.is_empty() {
            continue;
        }
        if parts.len() != 6 || parts[4] != "0" || parts[5] != "0" {
            return Err(format!(
                "fstab line {}: expected source target type options 0 0",
                number + 1
            ));
        }
        let source = field(parts[0])?;
        let target = field(parts[1])?;
        if hyper_block::Selector::parse(&source).is_none()
            || !crate::config::absolute_path(&target)
            || target == "/"
        {
            return Err(format!(
                "fstab line {}: invalid partition source or mount target",
                number + 1
            ));
        }
        if entries.len() == MAX_ENTRIES
            || entries
                .iter()
                .any(|entry| entry.source == source || entry.target == target)
        {
            return Err(format!(
                "fstab line {}: duplicate source/target or too many entries",
                number + 1
            ));
        }
        let (read_only, automatic, required) = options(parts[3])?;
        entries.push(Entry {
            source,
            target,
            format: format(parts[2])?,
            read_only,
            automatic,
            required,
        });
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_order_and_runtime_selection() -> Result<(), String> {
        let entries = parse(
            b"# bootstrap\nPARTLABEL=hyper-system /system auto ro 0 0\nPARTLABEL=hyper-data /data fat rw,noauto,nofail 0 0\n",
        )?;
        assert_eq!(entries[0].source, "PARTLABEL=hyper-system");
        assert_eq!(entries[0].format, None);
        assert!(entries[0].read_only);
        assert!(!entries[1].automatic && !entries[1].required);
        Ok(())
    }
    #[test]
    fn rejects_conflicts_and_unknown_options() {
        for text in [
            "PARTLABEL=hyper-data /data auto rw 1 1",
            "PARTLABEL=hyper-data /data auto banana 0 0",
            "PARTLABEL=hyper-data /data auto rw 0 0\nPARTLABEL=hyper-data /other auto rw 0 0",
            "../raw /data auto rw 0 0",
            "PARTLABEL=hyper-data / auto rw 0 0",
        ] {
            assert!(parse(text.as_bytes()).is_err(), "accepted {text}");
        }
        assert!(parse(&vec![b' '; MAX_BYTES + 1]).is_err());
    }
}
