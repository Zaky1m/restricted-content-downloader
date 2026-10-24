//! Downloads one file with `upload.getFile`, half a megabyte at a time.
//!
//! grammers' own `download_media` runs worker tasks that cannot be cancelled and `unwrap` on a closed
//! channel, so this loop is used instead: it reports progress, gives up when Telegram stops answering,
//! and stops the moment the future is dropped (task cancel).

use std::{path::Path, time::Duration};

use grammers_client::{Client, InvocationError};
use grammers_tl_types as tl;
use tokio::io::AsyncWriteExt;

use super::info::Item;

/// grammers' own download size. A 1 MB answer can arrive inside a container larger than grammers accepts, and
/// its reader then panics and drops the whole connection (every request on it, and the account's updates).
const CHUNK: i32 = 512 * 1024;
/// A request with no answer for this long counts as a stalled download.
pub const STALL_SECONDS: u64 = 90;

#[derive(Debug)]
pub enum Failure {
    Stalled,
    Incomplete { got: u64, expected: u64 },
    Flood(u32),
    FileReferenceExpired,
    SessionDead,
    Io(String),
    Other(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Stalled => write!(f, "no data for {STALL_SECONDS} seconds"),
            Failure::Incomplete { got, expected } => write!(f, "got {got} of {expected} bytes"),
            Failure::Flood(s) => write!(f, "flood wait {s}s"),
            Failure::FileReferenceExpired => write!(f, "file reference expired"),
            Failure::SessionDead => write!(f, "session is no longer authorized"),
            Failure::Io(e) | Failure::Other(e) => write!(f, "{e}"),
        }
    }
}

pub fn classify(error: &InvocationError) -> Failure {
    if let InvocationError::Rpc(rpc) = error {
        if rpc.name == "FLOOD_WAIT"
            || rpc.name == "FLOOD_PREMIUM_WAIT"
            || rpc.name == "SLOWMODE_WAIT"
        {
            return Failure::Flood(rpc.value.unwrap_or(30).max(1));
        }
        if rpc.name.starts_with("FILE_REFERENCE") {
            return Failure::FileReferenceExpired;
        }
        if matches!(
            rpc.name.as_str(),
            "AUTH_KEY_UNREGISTERED"
                | "SESSION_REVOKED"
                | "USER_DEACTIVATED"
                | "USER_DEACTIVATED_BAN"
                | "AUTH_KEY_DUPLICATED"
        ) {
            return Failure::SessionDead;
        }
    }
    Failure::Other(error.to_string())
}

async fn chunk(
    client: &Client,
    dc: i32,
    location: &tl::enums::InputFileLocation,
    offset: i64,
) -> Result<Vec<u8>, Failure> {
    let request = tl::functions::upload::GetFile {
        precise: false,
        cdn_supported: false,
        location: location.clone(),
        offset,
        limit: CHUNK,
    };
    let mut dc = dc;
    let mut copied = false;
    // A file stored in another data center answers once with a migrate error naming it.
    for _ in 0..3 {
        let answer = tokio::time::timeout(
            Duration::from_secs(STALL_SECONDS),
            client.invoke_in_dc(dc, &request),
        )
        .await
        .map_err(|_| Failure::Stalled)?;
        match answer {
            Ok(tl::enums::upload::File::File(file)) => return Ok(file.bytes),
            Ok(tl::enums::upload::File::CdnRedirect(_)) => {
                return Err(Failure::Other("unexpected CDN redirect".into()));
            }
            Err(InvocationError::Rpc(rpc)) if rpc.name == "FILE_MIGRATE" && rpc.value.is_some() => {
                dc = rpc.value.unwrap_or(dc as u32) as i32;
            }
            // Another data center does not know this login until it is copied there, as grammers' own
            // download does. This is not a dead session, so it must never be treated as one.
            Err(InvocationError::Rpc(rpc)) if rpc.name == "AUTH_KEY_UNREGISTERED" && !copied => {
                copied = true;
                copy_login_to(client, dc).await?;
            }
            Err(error) => return Err(classify(&error)),
        }
    }
    Err(Failure::Other(
        "file keeps moving between data centers".into(),
    ))
}

/// Exports the account's login from its home data center and imports it into `dc`.
async fn copy_login_to(client: &Client, dc: i32) -> Result<(), Failure> {
    let exported = client
        .invoke(&tl::functions::auth::ExportAuthorization { dc_id: dc })
        .await
        .map_err(|error| classify(&error))?;
    let tl::enums::auth::ExportedAuthorization::Authorization(exported) = exported;
    client
        .invoke_in_dc(
            dc,
            &tl::functions::auth::ImportAuthorization {
                id: exported.id,
                bytes: exported.bytes,
            },
        )
        .await
        .map_err(|error| classify(&error))?;
    tracing::info!(dc, "login copied to another data center");
    Ok(())
}

/// Writes the file to `path` (through `path.part`) and returns its size. `on_progress(done, total)` is
/// called after every megabyte; `total` is 0 when Telegram's size is not reliable.
pub async fn fetch(
    client: &Client,
    item: &Item,
    path: &Path,
    mut on_progress: impl FnMut(u64, u64),
) -> Result<u64, Failure> {
    let mut partial = path.as_os_str().to_owned();
    partial.push(".part");
    let partial = std::path::PathBuf::from(partial);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| Failure::Io(e.to_string()))?;
    }
    let mut file = tokio::fs::File::create(&partial)
        .await
        .map_err(|e| Failure::Io(e.to_string()))?;
    let total = item.expected_size();
    let mut offset: u64 = 0;
    let outcome: Result<(), Failure> = async {
        loop {
            let bytes = chunk(client, item.dc_id, &item.location, offset as i64).await?;
            if bytes.is_empty() {
                break;
            }
            file.write_all(&bytes)
                .await
                .map_err(|e| Failure::Io(e.to_string()))?;
            offset += bytes.len() as u64;
            on_progress(offset, total);
            if (bytes.len() as i32) < CHUNK || (total > 0 && offset >= total) {
                break;
            }
        }
        file.flush().await.map_err(|e| Failure::Io(e.to_string()))?;
        Ok(())
    }
    .await;
    drop(file);
    if let Err(failure) = outcome {
        tokio::fs::remove_file(&partial).await.ok();
        return Err(failure);
    }
    // A file shorter than Telegram's reported size is a broken transfer, never something to upload.
    if total > 0 && offset < total {
        tokio::fs::remove_file(&partial).await.ok();
        return Err(Failure::Incomplete {
            got: offset,
            expected: total,
        });
    }
    tokio::fs::rename(&partial, path)
        .await
        .map_err(|e| Failure::Io(e.to_string()))?;
    Ok(offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rpc(name: &str, value: Option<u32>) -> InvocationError {
        InvocationError::Rpc(grammers_client::sender::RpcError {
            code: 420,
            name: name.into(),
            value,
            caused_by: None,
        })
    }

    #[test]
    fn errors_are_sorted_into_what_the_caller_does() {
        assert!(matches!(
            classify(&rpc("FLOOD_WAIT", Some(12))),
            Failure::Flood(12)
        ));
        assert!(matches!(
            classify(&rpc("FILE_REFERENCE_EXPIRED", None)),
            Failure::FileReferenceExpired
        ));
        assert!(matches!(
            classify(&rpc("AUTH_KEY_UNREGISTERED", None)),
            Failure::SessionDead
        ));
        assert!(matches!(
            classify(&rpc("CHANNEL_PRIVATE", None)),
            Failure::Other(_)
        ));
    }
}
