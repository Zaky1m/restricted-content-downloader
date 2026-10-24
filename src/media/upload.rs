//! Uploading a local file and sending it as a photo, video, audio or document.

use std::{
    io,
    path::{Path, PathBuf},
};

use grammers_client::{
    Client, InvocationError,
    media::Uploaded,
    message::{InputMessage, Message, ReplyMarkup},
    session::types::PeerRef,
};
use grammers_tl_types as tl;
use tokio::io::AsyncRead;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendAs {
    Photo,
    Video,
    Audio,
    Document,
}

impl SendAs {
    /// The word the Python bot used for the media type (`photo`, `video`, `audio`, `document`).
    pub fn from_word(word: &str) -> Self {
        match word {
            "photo" => SendAs::Photo,
            "video" => SendAs::Video,
            "audio" => SendAs::Audio,
            _ => SendAs::Document,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Outgoing {
    pub path: PathBuf,
    pub send_as: SendAs,
    pub caption_html: String,
    pub thumbnail: Option<PathBuf>,
    pub duration: i64,
    pub width: i32,
    pub height: i32,
    pub performer: Option<String>,
    pub title: Option<String>,
}

const PART: usize = 512 * 1024;
/// Files above this go up as "big" files (`upload.saveBigFilePart`), as grammers does.
const BIG_FILE: u64 = 10 * 1024 * 1024;
/// Parts in flight for a big file, as grammers sends them.
const WORKERS: usize = 4;
/// Longest single wait worth sitting out for one part.
const MAX_PART_WAIT: u32 = 120;
/// Total waiting allowed for one file before the upload is given up.
const MAX_TOTAL_WAIT: u64 = 30 * 60;

/// Reads the next part (512 KB, the last one shorter). `None` at the end of the file.
async fn next_part<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    use tokio::io::AsyncReadExt;
    let mut part = vec![0u8; PART];
    let mut filled = 0;
    while filled < PART {
        let read = reader.read(&mut part[filled..]).await?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    part.truncate(filled);
    Ok((filled > 0).then_some(part))
}

/// Sends one part, sitting out Telegram's waits. A non-Premium account is slowed down with
/// FLOOD_PREMIUM_WAIT on uploads all the time (thousands of times a day in the Python bot's log); grammers
/// sleeps only once per request, so a part is retried here until it goes through or waiting gets too long.
async fn send_part<R: grammers_client::tl::RemoteCall<Return = bool>>(
    client: &Client,
    request: &R,
    waited: &std::sync::atomic::AtomicU64,
    throttled: &std::sync::atomic::AtomicBool,
) -> io::Result<()> {
    use std::sync::atomic::Ordering;
    let mut failures = 0;
    loop {
        match client.invoke(request).await {
            Ok(true) => return Ok(()),
            Ok(false) => return Err(io::Error::other("server failed to store uploaded data")),
            Err(InvocationError::Rpc(rpc)) if rpc.code == 420 => {
                if rpc.name.starts_with("FLOOD_PREMIUM_WAIT") {
                    throttled.store(true, Ordering::Relaxed);
                }
                let wait = rpc.value.unwrap_or(5).max(1);
                let total = waited.fetch_add(u64::from(wait), Ordering::Relaxed) + u64::from(wait);
                if wait > MAX_PART_WAIT || total > MAX_TOTAL_WAIT {
                    return Err(io::Error::other(format!(
                        "upload rate limited: wait {wait}s, {total}s in total"
                    )));
                }
                tokio::time::sleep(std::time::Duration::from_secs(u64::from(wait) + 1)).await;
            }
            Err(error) => {
                failures += 1;
                if failures >= 3 {
                    return Err(io::Error::other(error));
                }
                tokio::time::sleep(std::time::Duration::from_secs(2 * failures)).await;
            }
        }
    }
}

/// Uploads `path`, calling `on_progress(bytes_sent)`.
///
/// Instead of grammers' `upload_stream`, which sends four parts at once and fails the whole file on the
/// second rate-limit answer for any part: a part is retried on its own, and once Telegram says the account
/// is slowed down for lacking Premium, only one part is in flight from then on.
pub async fn upload(
    client: &Client,
    path: &Path,
    mut on_progress: impl FnMut(u64) + Send + 'static,
) -> io::Result<Uploaded> {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    };
    let mut file = tokio::fs::File::open(path).await?;
    let size = file.metadata().await?.len();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "a".to_string());
    let file_id: i64 = rand::random();
    let total_parts = size.div_ceil(PART as u64).max(1) as i32;
    let waited = Arc::new(AtomicU64::new(0));
    let throttled = Arc::new(AtomicBool::new(false));

    if size <= BIG_FILE {
        let mut index = 0;
        let mut done = 0u64;
        while let Some(bytes) = next_part(&mut file).await? {
            let length = bytes.len() as u64;
            let request = tl::functions::upload::SaveFilePart {
                file_id,
                file_part: index,
                bytes,
            };
            send_part(client, &request, &waited, &throttled).await?;
            index += 1;
            done += length;
            on_progress(done);
        }
        return Ok(Uploaded::from_raw(
            tl::types::InputFile {
                id: file_id,
                parts: index.max(1),
                name,
                // Optional; Telegram checks the file only when one is given.
                md5_checksum: String::new(),
            }
            .into(),
        ));
    }

    let reader = Arc::new(tokio::sync::Mutex::new((file, 0i32)));
    let done = Arc::new(AtomicU64::new(0));
    let progress = Arc::new(std::sync::Mutex::new(on_progress));
    let mut tasks = tokio::task::JoinSet::new();
    for worker in 0..WORKERS {
        let throttled = throttled.clone();
        let reader = reader.clone();
        let done = done.clone();
        let progress = progress.clone();
        let waited = waited.clone();
        let client = client.clone();
        tasks.spawn(async move {
            loop {
                if worker > 0 && throttled.load(Ordering::Relaxed) {
                    return io::Result::Ok(());
                }
                let (index, bytes) = {
                    let mut guard = reader.lock().await;
                    let Some(bytes) = next_part(&mut guard.0).await? else {
                        return io::Result::Ok(());
                    };
                    let index = guard.1;
                    guard.1 += 1;
                    (index, bytes)
                };
                let length = bytes.len() as u64;
                let request = tl::functions::upload::SaveBigFilePart {
                    file_id,
                    file_part: index,
                    file_total_parts: total_parts,
                    bytes,
                };
                send_part(&client, &request, &waited, &throttled).await?;
                let sent = done.fetch_add(length, Ordering::Relaxed) + length;
                (progress.lock().unwrap())(sent);
            }
        });
    }
    // Dropping the set (task cancelled, or a part failed) aborts the other workers.
    while let Some(result) = tasks.join_next().await {
        result.map_err(io::Error::other)??;
    }
    Ok(Uploaded::from_raw(
        tl::types::InputFileBig {
            id: file_id,
            parts: total_parts,
            name,
        }
        .into(),
    ))
}

fn mime_of(path: &Path) -> String {
    mime_guess::from_path(path)
        .first_or_octet_stream()
        .essence_str()
        .to_string()
}

fn attributes(out: &Outgoing) -> Vec<tl::enums::DocumentAttribute> {
    let name = out
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut list: Vec<tl::enums::DocumentAttribute> = Vec::new();
    match out.send_as {
        SendAs::Video => list.push(
            tl::types::DocumentAttributeVideo {
                round_message: false,
                supports_streaming: true,
                nosound: false,
                duration: out.duration as f64,
                w: out.width,
                h: out.height,
                preload_prefix_size: None,
                video_start_ts: None,
                video_codec: None,
            }
            .into(),
        ),
        SendAs::Audio => list.push(
            tl::types::DocumentAttributeAudio {
                voice: false,
                duration: out.duration as i32,
                title: out.title.clone(),
                performer: out.performer.clone(),
                waveform: None,
            }
            .into(),
        ),
        _ => {}
    }
    list.push(tl::types::DocumentAttributeFilename { file_name: name }.into());
    list
}

/// Sends an already uploaded file.
pub async fn send(
    client: &Client,
    peer: PeerRef,
    out: &Outgoing,
    file: Uploaded,
    thumbnail: Option<Uploaded>,
    markup: Option<ReplyMarkup>,
) -> Result<Message, InvocationError> {
    let media: tl::enums::InputMedia = match out.send_as {
        SendAs::Photo => tl::types::InputMediaUploadedPhoto {
            spoiler: false,
            live_photo: false,
            file: file.raw,
            stickers: None,
            ttl_seconds: None,
            video: None,
        }
        .into(),
        _ => tl::types::InputMediaUploadedDocument {
            nosound_video: false,
            force_file: false,
            spoiler: false,
            file: file.raw,
            thumb: thumbnail.map(|t| t.raw),
            mime_type: mime_of(&out.path),
            attributes: attributes(out),
            stickers: None,
            video_cover: None,
            video_timestamp: None,
            ttl_seconds: None,
        }
        .into(),
    };
    let mut message = InputMessage::new().html(&out.caption_html).media(media);
    if let Some(markup) = markup {
        message = message.reply_markup(markup);
    }
    client.send_message(peer, message).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outgoing(send_as: SendAs, name: &str) -> Outgoing {
        Outgoing {
            path: PathBuf::from(name),
            send_as,
            caption_html: String::new(),
            thumbnail: None,
            duration: 61,
            width: 1280,
            height: 720,
            performer: Some("P".into()),
            title: None,
        }
    }

    #[test]
    fn video_attributes_carry_size_and_streaming() {
        let attrs = attributes(&outgoing(SendAs::Video, "dir/a.mp4"));
        assert!(
            matches!(&attrs[0], tl::enums::DocumentAttribute::Video(v) if v.w == 1280 && v.supports_streaming && v.duration == 61.0)
        );
        assert!(
            matches!(&attrs[1], tl::enums::DocumentAttribute::Filename(f) if f.file_name == "a.mp4")
        );
    }

    #[test]
    fn audio_attributes_and_mime() {
        let attrs = attributes(&outgoing(SendAs::Audio, "a.mp3"));
        assert!(
            matches!(&attrs[0], tl::enums::DocumentAttribute::Audio(a) if a.performer.as_deref() == Some("P") && a.duration == 61)
        );
        assert_eq!(mime_of(Path::new("a.mp3")), "audio/mpeg");
        assert_eq!(
            mime_of(Path::new("a.unknownext")),
            "application/octet-stream"
        );
    }

    #[test]
    fn words_map_to_kinds() {
        assert_eq!(SendAs::from_word("video"), SendAs::Video);
        assert_eq!(SendAs::from_word("whatever"), SendAs::Document);
    }
}
