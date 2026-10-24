//! Logs go three places: the console (pm2), `logs.txt` in the data folder, and, for real faults, the log channel.
//!
//! Connection chatter and automatic retries are dropped everywhere. The same message with different numbers reaches
//! the console once a minute and the channel once every ten minutes. `logs.txt` is sent to the owners and emptied
//! whenever it reaches a megabyte, so it never grows on the server.

use std::{
    collections::HashMap,
    fmt::Write as _,
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use tokio::sync::mpsc;
use tracing::{
    Event, Level, Subscriber,
    field::{Field, Visit},
};
use tracing_subscriber::{
    EnvFilter, Layer,
    layer::{Context, SubscriberExt},
    registry::LookupSpan,
    util::SubscriberInitExt,
};

pub const UPLOAD_THRESHOLD: u64 = 1024 * 1024;
const CONSOLE_INTERVAL: Duration = Duration::from_secs(60);
const CHANNEL_INTERVAL: Duration = Duration::from_secs(10 * 60);
const MAX_TRACKED: usize = 500;
const MAX_CHANNEL_TEXT: usize = 3900;

/// Connection chatter and automatic retries carry no action for anyone.
const NOISE: [&str; 21] = [
    "retrying",
    "waiting for",
    "seconds before continuing",
    "connection failed! trying again",
    "unable to connect due to network",
    "connecting...",
    "connected!",
    "disconnected",
    "tcp connected",
    "network task started",
    "ping task started",
    "session started",
    "session stopped",
    "restarting session due to",
    "persistent_timestamp_outdated",
    "getchanneldifference",
    "socket.send() raised exception",
    "cannot write to closing transport",
    "connection reset by peer",
    "connection lost",
    "transport flood",
];
const FLOOD: [&str; 4] = [
    "floodwait",
    "flood_wait",
    "floodpremiumwait",
    "flood_premium_wait",
];

#[derive(Default)]
struct Fields {
    message: String,
    user_caused: bool,
    always_show: bool,
}

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        match field.name() {
            "message" => {
                let _ = write!(self.message, "{value:?}");
            }
            "user_caused" => self.user_caused = format!("{value:?}") == "true",
            "always_show" => self.always_show = format!("{value:?}") == "true",
            name => {
                let _ = write!(self.message, " {name}={value:?}");
            }
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            let _ = write!(self.message, " {}={value}", field.name());
        }
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        match field.name() {
            "user_caused" => self.user_caused = value,
            "always_show" => self.always_show = value,
            name => {
                let _ = write!(self.message, " {name}={value}");
            }
        }
    }
}

fn fields_of(event: &Event<'_>) -> Fields {
    let mut fields = Fields::default();
    event.record(&mut fields);
    fields
}

/// Digits become `N`, so "task 12 failed" and "task 99 failed" count as one kind of message.
pub fn signature(target: &str, text: &str) -> String {
    let squeezed: String = text
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_digit() { 'N' } else { c })
        .collect();
    let mut collapsed = String::new();
    let mut last_digit = false;
    for c in squeezed.chars() {
        if c == 'N' {
            if !last_digit {
                collapsed.push('N');
            }
            last_digit = true;
        } else {
            collapsed.push(c);
            last_digit = false;
        }
    }
    let one_line = collapsed.split_whitespace().collect::<Vec<_>>().join(" ");
    format!(
        "{target}|{}",
        one_line.chars().take(500).collect::<String>()
    )
}

pub fn is_noise(text: &str) -> bool {
    let lowered = text.to_lowercase();
    NOISE.iter().any(|p| lowered.contains(p))
}

fn is_flood(text: &str) -> bool {
    let lowered = text.to_lowercase();
    FLOOD.iter().any(|p| lowered.contains(p))
}

/// Lets an event through at most once per `interval` per signature.
struct Throttle {
    seen: Mutex<HashMap<String, Instant>>,
    interval: Duration,
}

impl Throttle {
    fn new(interval: Duration) -> Self {
        Self {
            seen: Mutex::new(HashMap::new()),
            interval,
        }
    }

    fn allow(&self, key: String) -> bool {
        let Ok(mut seen) = self.seen.lock() else {
            return true;
        };
        let now = Instant::now();
        if seen
            .get(&key)
            .is_some_and(|at| now.duration_since(*at) < self.interval)
        {
            return false;
        }
        if seen.len() > MAX_TRACKED {
            seen.retain(|_, at| now.duration_since(*at) < self.interval);
        }
        seen.insert(key, now);
        true
    }
}

/// Writes each event once to the console and to `logs.txt`. Noise is dropped everywhere; a repeat of the same
/// message within a minute is kept out of the console only. Filtering here, inside one layer, avoids the
/// per-layer filter state that can make tracing-subscriber lose unrelated events.
struct Output {
    console: Throttle,
    file: Arc<Mutex<File>>,
}

impl<S: Subscriber> Layer<S> for Output {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let fields = fields_of(event);
        if is_noise(&fields.message) {
            return;
        }
        let meta = event.metadata();
        let line = format!(
            "{} {:>5} {}: {}\n",
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
            meta.level(),
            meta.target(),
            fields.message.trim()
        );
        if let Ok(mut file) = self.file.lock() {
            let _ = file.write_all(line.as_bytes());
        }
        if fields.always_show
            || self
                .console
                .allow(signature(meta.target(), &fields.message))
        {
            let _ = std::io::stdout().write_all(line.as_bytes());
        }
    }
}

/// Real faults go to the log channel through this queue.
struct ChannelLayer {
    tx: mpsc::UnboundedSender<String>,
    throttle: Throttle,
}

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for ChannelLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        if *event.metadata().level() != Level::ERROR {
            return;
        }
        let fields = fields_of(event);
        if fields.user_caused
            || is_flood(&fields.message)
            || fields
                .message
                .to_lowercase()
                .contains("unknown constructor")
        {
            return;
        }
        if !self
            .throttle
            .allow(signature(event.metadata().target(), &fields.message))
        {
            return;
        }
        let line = format!(
            "ERROR {}: {}",
            event.metadata().target(),
            fields.message.trim()
        );
        let _ = self.tx.send(line);
    }
}

pub struct Logs {
    pub path: PathBuf,
    file: Arc<Mutex<File>>,
    channel_rx: Mutex<Option<mpsc::UnboundedReceiver<String>>>,
}

static LOGS: OnceLock<Arc<Logs>> = OnceLock::new();

/// Installs the subscriber. Call once, first thing in `main`.
pub fn init(data_dir: &Path) -> std::io::Result<Arc<Logs>> {
    std::fs::create_dir_all(data_dir)?;
    let path = data_dir.join("logs.txt");
    let file = Arc::new(Mutex::new(
        OpenOptions::new().create(true).append(true).open(&path)?,
    ));
    let (tx, rx) = mpsc::unbounded_channel();
    tracing_subscriber::registry()
        .with(level_filter())
        .with(Output {
            console: Throttle::new(CONSOLE_INTERVAL),
            file: file.clone(),
        })
        .with(ChannelLayer {
            tx,
            throttle: Throttle::new(CHANNEL_INTERVAL),
        })
        .try_init()
        .ok();
    let logs = Arc::new(Logs {
        path,
        file,
        channel_rx: Mutex::new(Some(rx)),
    });
    let _ = LOGS.set(logs.clone());
    Ok(logs)
}

fn level_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(
            "info,sqlx=warn,grammers_mtsender=warn,grammers_mtproto=warn,grammers_session=warn,grammers_client=warn,html5ever=warn",
        )
    })
}

impl Logs {
    /// The queue the channel sender drains (taken once).
    pub fn take_channel(&self) -> Option<mpsc::UnboundedReceiver<String>> {
        self.channel_rx.lock().ok().and_then(|mut rx| rx.take())
    }

    pub fn size(&self) -> u64 {
        std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0)
    }

    /// Moves the current log aside so it can be sent, and starts an empty one. Returns the path of the old part.
    pub fn rotate(&self) -> std::io::Result<Option<PathBuf>> {
        let mut guard = self.file.lock().unwrap_or_else(|e| e.into_inner());
        guard.flush()?;
        if std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0) == 0 {
            return Ok(None);
        }
        let part = self.path.with_file_name(format!(
            "logs_part_{}.txt",
            chrono::Utc::now().format("%Y%m%d_%H%M%S")
        ));
        std::fs::rename(&self.path, &part)?;
        *guard = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        Ok(Some(part))
    }
}

pub fn escape_pre(text: &str) -> String {
    let cut: String = text.chars().take(MAX_CHANNEL_TEXT).collect();
    let cut = if text.chars().count() > MAX_CHANNEL_TEXT {
        format!("{cut}\n... [truncated]")
    } else {
        cut
    };
    cut.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_do_not_make_messages_different() {
        assert_eq!(
            signature("a", "Task 12 failed for user 998877"),
            signature("a", "task 7 failed for user 5")
        );
        assert_ne!(signature("a", "boom"), signature("b", "boom"));
    }

    #[test]
    fn noise_is_recognised() {
        assert!(is_noise("Connection reset by peer"));
        assert!(is_noise("RETRYING in 5"));
        assert!(!is_noise("database is down"));
    }

    #[test]
    fn throttle_lets_one_through_per_window() {
        let t = Throttle::new(Duration::from_secs(60));
        assert!(t.allow("x".into()));
        assert!(!t.allow("x".into()));
        assert!(t.allow("y".into()));
    }

    #[test]
    fn channel_text_is_escaped_and_cut() {
        assert_eq!(escape_pre("<b>&"), "&lt;b&gt;&amp;");
        assert!(escape_pre(&"x".repeat(5000)).ends_with("[truncated]"));
    }
}
