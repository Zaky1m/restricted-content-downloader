//! Paths, free space and splitting of local files.

use std::path::{Path, PathBuf};

use sysinfo::Disks;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Room kept free for the rest of the server whatever the bot downloads.
pub const DISK_RESERVE_BYTES: u64 = 5 * 1024 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
#[error("need {needed} bytes, {free} free")]
pub struct NotEnoughDiskSpace {
    pub needed: u64,
    pub free: u64,
}

pub fn is_valid_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() > 0)
}

/// Removes a file (or a folder with everything in it) and also the `.part` file of a download.
pub fn cleanup(path: &Path) {
    let _ = std::fs::remove_file(path);
    let mut partial = path.as_os_str().to_owned();
    partial.push(".part");
    let _ = std::fs::remove_file(PathBuf::from(partial));
}

pub fn free_bytes(path: &Path) -> u64 {
    let disks = Disks::new_with_refreshed_list();
    let here = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    disks
        .list()
        .iter()
        .filter(|d| here.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map_or(u64::MAX, |d| d.available_space())
}

/// A file may be split, so twice its size must fit next to the reserve.
pub fn ensure_disk_space(dir: &Path, expected_size: u64) -> Result<(), NotEnoughDiskSpace> {
    ensure_with(free_bytes(dir), expected_size)
}

fn ensure_with(free: u64, expected_size: u64) -> Result<(), NotEnoughDiskSpace> {
    let needed = expected_size
        .saturating_mul(2)
        .saturating_add(DISK_RESERVE_BYTES);
    if expected_size > 0 && free < needed {
        Err(NotEnoughDiskSpace { needed, free })
    } else {
        Ok(())
    }
}

/// Cuts `path` into `part_size` byte pieces named `<name>_part_aa`, `_ab` ... inside `dir`
/// (the naming `split` used, so parts sort in order).
pub async fn split(path: &Path, dir: &Path, part_size: u64) -> std::io::Result<Vec<PathBuf>> {
    tokio::fs::create_dir_all(dir).await?;
    let base = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut source = tokio::fs::File::open(path).await?;
    let mut parts = Vec::new();
    let mut buffer = vec![0u8; 8 * 1024 * 1024];
    let mut index = 0usize;
    loop {
        let suffix = format!(
            "{}{}",
            (b'a' + (index / 26) as u8) as char,
            (b'a' + (index % 26) as u8) as char
        );
        let part_path = dir.join(format!("{base}_part_{suffix}"));
        let mut part = tokio::fs::File::create(&part_path).await?;
        let mut written = 0u64;
        while written < part_size {
            let want = buffer.len().min((part_size - written) as usize);
            let read = source.read(&mut buffer[..want]).await?;
            if read == 0 {
                break;
            }
            part.write_all(&buffer[..read]).await?;
            written += read as u64;
        }
        part.flush().await?;
        if written == 0 {
            tokio::fs::remove_file(&part_path).await.ok();
            break;
        }
        parts.push(part_path);
        index += 1;
        if written < part_size {
            break;
        }
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_rule() {
        assert!(ensure_with(100, 0).is_ok());
        assert!(ensure_with(DISK_RESERVE_BYTES + 2000, 1000).is_ok());
        assert!(ensure_with(DISK_RESERVE_BYTES + 1999, 1000).is_err());
    }

    #[tokio::test]
    async fn splits_into_ordered_parts_that_join_back() {
        let dir = std::env::temp_dir().join(format!("rcd_split_{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let source = dir.join("movie.bin");
        let data: Vec<u8> = (0..2500u32).map(|i| (i % 251) as u8).collect();
        tokio::fs::write(&source, &data).await.unwrap();
        let parts = split(&source, &dir.join("parts"), 1000).await.unwrap();
        let names: Vec<_> = parts
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            [
                "movie.bin_part_aa",
                "movie.bin_part_ab",
                "movie.bin_part_ac"
            ]
        );
        let mut joined = Vec::new();
        for part in &parts {
            joined.extend(tokio::fs::read(part).await.unwrap());
        }
        assert_eq!(joined, data);
        assert_eq!(tokio::fs::metadata(&parts[2]).await.unwrap().len(), 500);
        // An exact multiple makes no empty trailing part.
        tokio::fs::write(&source, vec![1u8; 2000]).await.unwrap();
        assert_eq!(
            split(&source, &dir.join("again"), 1000)
                .await
                .unwrap()
                .len(),
            2
        );
        std::fs::remove_dir_all(dir).ok();
    }
}
