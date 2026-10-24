//! What a Telegram message carries: its kind, size, name and where to fetch the bytes from.

use grammers_tl_types as tl;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Photo,
    Video,
    VideoNote,
    Animation,
    Audio,
    Voice,
    Sticker,
    Document,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub kind: Kind,
    /// Size Telegram reports; for photos it is the size of the largest variant and not a reliable total.
    pub size: u64,
    pub name: Option<String>,
    pub mime: String,
    pub location: tl::enums::InputFileLocation,
    pub dc_id: i32,
    pub duration: f64,
    pub width: i32,
    pub height: i32,
    pub performer: Option<String>,
    pub title: Option<String>,
    pub video_sticker: bool,
}

impl Item {
    /// Bytes a finished download must have, or 0 when the reported size cannot be trusted (photos).
    pub fn expected_size(&self) -> u64 {
        if self.kind == Kind::Photo {
            0
        } else {
            self.size
        }
    }
}

fn largest_photo_size(sizes: &[tl::enums::PhotoSize]) -> Option<(String, u64, i32, i32)> {
    use tl::enums::PhotoSize as P;
    sizes
        .iter()
        .filter_map(|size| match size {
            P::Size(s) => Some((s.r#type.clone(), s.size.max(0) as u64, s.w, s.h)),
            P::Progressive(s) => Some((
                s.r#type.clone(),
                s.sizes.iter().copied().max().unwrap_or(0).max(0) as u64,
                s.w,
                s.h,
            )),
            _ => None,
        })
        .max_by_key(|(_, size, w, h)| (*size, i64::from(*w) * i64::from(*h)))
}

fn photo_item(photo: &tl::enums::Photo) -> Option<Item> {
    let tl::enums::Photo::Photo(p) = photo else {
        return None;
    };
    let (thumb_type, size, width, height) = largest_photo_size(&p.sizes)?;
    Some(Item {
        kind: Kind::Photo,
        size,
        name: None,
        mime: "image/jpeg".into(),
        location: tl::types::InputPhotoFileLocation {
            id: p.id,
            access_hash: p.access_hash,
            file_reference: p.file_reference.clone(),
            thumb_size: thumb_type,
        }
        .into(),
        dc_id: p.dc_id,
        duration: 0.0,
        width,
        height,
        performer: None,
        title: None,
        video_sticker: false,
    })
}

fn document_item(document: &tl::enums::Document) -> Option<Item> {
    use tl::enums::DocumentAttribute as A;
    let tl::enums::Document::Document(d) = document else {
        return None;
    };
    let mut item = Item {
        kind: Kind::Document,
        size: d.size.max(0) as u64,
        name: None,
        mime: d.mime_type.clone(),
        location: tl::types::InputDocumentFileLocation {
            id: d.id,
            access_hash: d.access_hash,
            file_reference: d.file_reference.clone(),
            thumb_size: String::new(),
        }
        .into(),
        dc_id: d.dc_id,
        duration: 0.0,
        width: 0,
        height: 0,
        performer: None,
        title: None,
        video_sticker: false,
    };
    let (mut audio, mut animated, mut sticker, mut video) = (None, false, false, None);
    for attribute in &d.attributes {
        match attribute {
            A::Filename(f) => item.name = Some(f.file_name.clone()),
            A::Audio(a) => audio = Some(a),
            A::Animated => animated = true,
            A::Sticker(_) => sticker = true,
            A::Video(v) => video = Some(v),
            _ => {}
        }
    }
    // The order Pyrogram uses to name a document: audio, animation, sticker, video, then plain file.
    if let Some(a) = audio {
        item.kind = if a.voice { Kind::Voice } else { Kind::Audio };
        item.duration = f64::from(a.duration);
        item.performer = a.performer.clone();
        item.title = a.title.clone();
    } else if animated {
        item.kind = Kind::Animation;
        if let Some(v) = video {
            item.duration = v.duration;
            item.width = v.w;
            item.height = v.h;
        }
    } else if sticker {
        item.kind = Kind::Sticker;
        item.video_sticker = d.mime_type == "video/webm";
    } else if let Some(v) = video {
        item.kind = if v.round_message {
            Kind::VideoNote
        } else {
            Kind::Video
        };
        item.duration = v.duration;
        item.width = v.w;
        item.height = v.h;
    }
    Some(item)
}

/// The downloadable file of one media value (photo or document); other media have none.
pub fn item_from_media(media: &tl::enums::MessageMedia) -> Option<Item> {
    match media {
        tl::enums::MessageMedia::Photo(p) => p.photo.as_ref().and_then(photo_item),
        tl::enums::MessageMedia::Document(d) => d.document.as_ref().and_then(document_item),
        _ => None,
    }
}

/// Purchased items of a paid-media post, with their zero based index. Items not bought show only a
/// blurred preview and have no file, so they are left out.
pub fn paid_items(message: &tl::types::Message) -> Vec<(usize, Item)> {
    let Some(tl::enums::MessageMedia::PaidMedia(paid)) = &message.media else {
        return Vec::new();
    };
    paid.extended_media
        .iter()
        .enumerate()
        .filter_map(|(index, extended)| match extended {
            tl::enums::MessageExtendedMedia::Media(m) => {
                item_from_media(&m.media).map(|item| (index, item))
            }
            tl::enums::MessageExtendedMedia::Preview(_) => None,
        })
        .collect()
}

pub fn has_paid_media(message: &tl::types::Message) -> bool {
    matches!(message.media, Some(tl::enums::MessageMedia::PaidMedia(_)))
}

/// The first downloadable file of a message (a paid post yields its first purchased item).
pub fn downloadable(message: &tl::types::Message) -> Option<Item> {
    if let Some((_, item)) = paid_items(message).into_iter().next() {
        return Some(item);
    }
    message.media.as_ref().and_then(item_from_media)
}

/// `videos`, `photos`, `audio`, `files`, `text` or `other`, as the filters name them.
pub fn message_type(message: &tl::types::Message) -> Option<&'static str> {
    let paid = paid_items(message);
    if !paid.is_empty() {
        let has_video = paid
            .iter()
            .any(|(_, i)| matches!(i.kind, Kind::Video | Kind::VideoNote | Kind::Animation));
        return Some(if has_video { "videos" } else { "photos" });
    }
    if has_paid_media(message) {
        return Some("other");
    }
    if let Some(item) = message.media.as_ref().and_then(item_from_media) {
        return Some(match item.kind {
            Kind::Photo => "photos",
            Kind::Video | Kind::VideoNote | Kind::Animation => "videos",
            Kind::Audio | Kind::Voice => "audio",
            Kind::Document => "files",
            Kind::Sticker => "other",
        });
    }
    if matches!(message.media, Some(tl::enums::MessageMedia::Poll(_))) {
        return Some("other");
    }
    let text_only = matches!(
        message.media,
        None | Some(tl::enums::MessageMedia::WebPage(_)) | Some(tl::enums::MessageMedia::Empty)
    );
    if text_only && !message.message.is_empty() {
        Some("text")
    } else {
        None
    }
}

pub fn file_name(message_id: i32, item: &Item) -> String {
    let named = |fallback: String| {
        item.name
            .clone()
            .filter(|n| !n.is_empty())
            .unwrap_or(fallback)
    };
    match item.kind {
        Kind::Document => named(format!("{message_id}.bin")),
        Kind::Video => named(format!("{message_id}.mp4")),
        Kind::Audio => named(format!("{message_id}.mp3")),
        Kind::Voice => format!("{message_id}.ogg"),
        Kind::VideoNote => format!("{message_id}_videonote.mp4"),
        Kind::Animation => named(format!("{message_id}.gif")),
        Kind::Sticker => format!(
            "{message_id}.{}",
            if item.video_sticker { "webm" } else { "webp" }
        ),
        Kind::Photo => format!("{message_id}.jpg"),
    }
}

/// Name of a purchased item: its own, else `<id>_paid_<n>.mp4|jpg`.
pub fn paid_file_name(message_id: i32, index: usize, item: &Item) -> String {
    if let Some(name) = item.name.clone().filter(|n| !n.is_empty()) {
        return name;
    }
    let extension = if matches!(item.kind, Kind::Video | Kind::VideoNote | Kind::Animation) {
        "mp4"
    } else {
        "jpg"
    };
    format!("{message_id}_paid_{}.{extension}", index + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(
        attributes: Vec<tl::enums::DocumentAttribute>,
        mime: &str,
    ) -> tl::enums::MessageMedia {
        tl::types::MessageMediaDocument {
            nopremium: false,
            spoiler: false,
            video: false,
            round: false,
            voice: false,
            document: Some(
                tl::types::Document {
                    id: 1,
                    access_hash: 2,
                    file_reference: vec![9],
                    date: 0,
                    mime_type: mime.into(),
                    size: 100,
                    thumbs: None,
                    video_thumbs: None,
                    dc_id: 4,
                    attributes,
                }
                .into(),
            ),
            alt_documents: None,
            video_cover: None,
            video_timestamp: None,
            ttl_seconds: None,
        }
        .into()
    }

    fn video_attr(round: bool) -> tl::enums::DocumentAttribute {
        tl::types::DocumentAttributeVideo {
            round_message: round,
            supports_streaming: true,
            nosound: false,
            duration: 12.0,
            w: 640,
            h: 360,
            preload_prefix_size: None,
            video_start_ts: None,
            video_codec: None,
        }
        .into()
    }

    #[test]
    fn classifies_documents_like_pyrogram() {
        let kind = |attrs, mime| item_from_media(&document(attrs, mime)).unwrap().kind;
        assert_eq!(kind(vec![video_attr(false)], "video/mp4"), Kind::Video);
        assert_eq!(kind(vec![video_attr(true)], "video/mp4"), Kind::VideoNote);
        assert_eq!(
            kind(
                vec![tl::enums::DocumentAttribute::Animated, video_attr(false)],
                "video/mp4"
            ),
            Kind::Animation
        );
        assert_eq!(kind(vec![], "application/pdf"), Kind::Document);
        let voice = tl::types::DocumentAttributeAudio {
            voice: true,
            duration: 3,
            title: None,
            performer: None,
            waveform: None,
        };
        assert_eq!(kind(vec![voice.into()], "audio/ogg"), Kind::Voice);
        let music = tl::types::DocumentAttributeAudio {
            voice: false,
            duration: 3,
            title: Some("t".into()),
            performer: None,
            waveform: None,
        };
        assert_eq!(kind(vec![music.into()], "audio/mpeg"), Kind::Audio);
    }

    #[test]
    fn names_follow_the_python_rules() {
        let named = tl::types::DocumentAttributeFilename {
            file_name: "a.pdf".into(),
        };
        let item = item_from_media(&document(vec![named.into()], "application/pdf")).unwrap();
        assert_eq!(file_name(7, &item), "a.pdf");
        let plain = item_from_media(&document(vec![], "application/pdf")).unwrap();
        assert_eq!(file_name(7, &plain), "7.bin");
        let note = item_from_media(&document(vec![video_attr(true)], "video/mp4")).unwrap();
        assert_eq!(file_name(7, &note), "7_videonote.mp4");
        assert_eq!(paid_file_name(7, 0, &plain), "7_paid_1.jpg");
        let video = item_from_media(&document(vec![video_attr(false)], "video/mp4")).unwrap();
        assert_eq!(paid_file_name(7, 1, &video), "7_paid_2.mp4");
    }

    #[test]
    fn photos_report_the_largest_size_but_no_expected_size() {
        let small = tl::types::PhotoSize {
            r#type: "m".into(),
            w: 320,
            h: 240,
            size: 1000,
        };
        let large = tl::types::PhotoSize {
            r#type: "y".into(),
            w: 1280,
            h: 960,
            size: 90_000,
        };
        let photo = tl::types::MessageMediaPhoto {
            spoiler: false,
            photo: Some(
                tl::types::Photo {
                    has_stickers: false,
                    id: 5,
                    access_hash: 6,
                    file_reference: vec![],
                    date: 0,
                    sizes: vec![small.into(), large.into()],
                    video_sizes: None,
                    dc_id: 2,
                }
                .into(),
            ),
            ttl_seconds: None,
            live_photo: false,
            video: None,
        };
        let item = item_from_media(&photo.into()).unwrap();
        assert_eq!(
            (item.size, item.expected_size(), item.width),
            (90_000, 0, 1280)
        );
        let tl::enums::InputFileLocation::InputPhotoFileLocation(location) = &item.location else {
            panic!("wrong location")
        };
        assert_eq!(location.thumb_size, "y");
    }
}
