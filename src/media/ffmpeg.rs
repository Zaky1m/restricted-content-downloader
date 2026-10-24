//! `ffprobe` and `ffmpeg` run as external processes.

use std::path::{Path, PathBuf};

use serde_json::Value;
use tokio::process::Command;

pub struct Output {
    pub stdout: String,
    pub success: bool,
}

async fn run(program: &str, args: &[&str]) -> Output {
    match Command::new(program).args(args).output().await {
        Ok(out) => Output {
            stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
            success: out.status.success(),
        },
        Err(_) => Output {
            stdout: String::new(),
            success: false,
        },
    }
}

pub struct Format {
    pub duration: i64,
    pub artist: Option<String>,
    pub title: Option<String>,
}

fn number(value: &Value) -> f64 {
    match value {
        Value::String(s) => s.parse().unwrap_or(0.0),
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        _ => 0.0,
    }
}

pub fn parse_format(json: &str) -> Format {
    let none = Format {
        duration: 0,
        artist: None,
        title: None,
    };
    let Ok(value) = serde_json::from_str::<Value>(json) else {
        return none;
    };
    let format = value.get("format").cloned().unwrap_or(Value::Null);
    let tags = format.get("tags").cloned().unwrap_or(Value::Null);
    let tag = |lower: &str| {
        tags.get(lower)
            .or_else(|| tags.get(lower.to_uppercase()))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    Format {
        duration: number(format.get("duration").unwrap_or(&Value::Null)).round_ties_even() as i64,
        artist: tag("artist"),
        title: tag("title"),
    }
}

pub fn parse_dimensions(json: &str) -> (i32, i32) {
    let Ok(value) = serde_json::from_str::<Value>(json) else {
        return (0, 0);
    };
    let Some(stream) = value.get("streams").and_then(|s| s.get(0)) else {
        return (0, 0);
    };
    let dimension = |key: &str| number(stream.get(key).unwrap_or(&Value::Null)) as i32;
    (dimension("width"), dimension("height"))
}

pub async fn media_info(path: &Path) -> Format {
    let path = path.to_string_lossy();
    let out = run(
        "ffprobe",
        &[
            "-hide_banner",
            "-loglevel",
            "error",
            "-print_format",
            "json",
            "-show_format",
            &path,
        ],
    )
    .await;
    if !out.success || out.stdout.is_empty() {
        return Format {
            duration: 0,
            artist: None,
            title: None,
        };
    }
    parse_format(&out.stdout)
}

pub async fn video_dimensions(path: &Path) -> (i32, i32) {
    let path = path.to_string_lossy();
    let out = run(
        "ffprobe",
        &[
            "-hide_banner",
            "-loglevel",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "json",
            &path,
        ],
    )
    .await;
    if !out.success || out.stdout.is_empty() {
        return (0, 0);
    }
    parse_dimensions(&out.stdout)
}

/// A frame from the middle of the video, written into `dir`; `None` if ffmpeg could not make one.
pub async fn video_thumbnail(video: &Path, dir: &Path) -> Option<PathBuf> {
    if !std::fs::metadata(video).is_ok_and(|m| m.is_file() && m.len() > 0) {
        return None;
    }
    tokio::fs::create_dir_all(dir).await.ok()?;
    let thumb = dir.join(format!("thumb_{:032x}.jpg", rand::random::<u128>()));
    let duration = media_info(video).await.duration;
    let seek = if duration > 0 {
        (duration / 2).max(1)
    } else {
        1
    };
    let video_arg = video.to_string_lossy();
    let thumb_arg = thumb.to_string_lossy();
    let seek_arg = seek.to_string();
    let out = run(
        "ffmpeg",
        &[
            "-hide_banner",
            "-loglevel",
            "error",
            "-ss",
            &seek_arg,
            "-i",
            &video_arg,
            "-vframes",
            "1",
            "-q:v",
            "2",
            "-y",
            &thumb_arg,
        ],
    )
    .await;
    if out.success && std::fs::metadata(&thumb).is_ok_and(|m| m.is_file() && m.len() > 0) {
        Some(thumb)
    } else {
        tokio::fs::remove_file(&thumb).await.ok();
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ffprobe_format() {
        let f = parse_format(r#"{"format":{"duration":"12.5","tags":{"ARTIST":"A","title":"T"}}}"#);
        assert_eq!(
            (f.duration, f.artist.as_deref(), f.title.as_deref()),
            (12, Some("A"), Some("T"))
        );
        assert_eq!(
            parse_format(r#"{"format":{"duration":"13.5"}}"#).duration,
            14
        );
        assert_eq!(parse_format("not json").duration, 0);
        assert_eq!(parse_format(r#"{"format":{}}"#).duration, 0);
    }

    #[test]
    fn parses_dimensions() {
        assert_eq!(
            parse_dimensions(r#"{"streams":[{"width":1280,"height":720}]}"#),
            (1280, 720)
        );
        assert_eq!(parse_dimensions(r#"{"streams":[]}"#), (0, 0));
        assert_eq!(parse_dimensions("x"), (0, 0));
    }
}
