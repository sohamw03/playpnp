use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportState {
    Stopped,
    Playing,
    PausedPlayback,
    Transitioning,
    NoMediaPresent,
}

impl TransportState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Stopped => "STOPPED",
            Self::Playing => "PLAYING",
            Self::PausedPlayback => "PAUSED_PLAYBACK",
            Self::Transitioning => "TRANSITIONING",
            Self::NoMediaPresent => "NO_MEDIA_PRESENT",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AVState {
    pub transport_state: TransportState,
    pub current_uri: String,
    pub current_metadata: String, // DIDL-Lite escaped
    pub title: String,
    pub duration: Duration,
    pub position: Duration,
    pub volume: u8, // 0-100
    pub mute: bool,
    pub track: u32,
    #[allow(dead_code)]
    pub last_change_seq: u32,
    #[allow(dead_code)]
    pub last_updated: Instant,
    #[allow(dead_code)]
    pub seekable: bool,
}

impl Default for AVState {
    fn default() -> Self {
        Self {
            transport_state: TransportState::Stopped,
            current_uri: String::new(),
            current_metadata: String::new(),
            title: String::new(),
            duration: Duration::from_secs(0),
            position: Duration::from_secs(0),
            volume: 50,
            mute: false,
            track: 0,
            last_change_seq: 0,
            last_updated: Instant::now(),
            seekable: true,
        }
    }
}

pub type SharedState = Arc<RwLock<AVState>>;

pub fn new_shared_state() -> SharedState {
    Arc::new(RwLock::new(AVState::default()))
}

pub fn format_time(d: Duration) -> String {
    let total = d.as_secs();
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    format!("{h:01}:{m:02}:{s:02}")
}

pub fn parse_time(s: &str) -> Option<Duration> {
    // UPnP time values are H+:MM:SS[.fraction]. BubbleUPnP commonly
    // includes milliseconds while scrubbing, so do not reject those values.
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let h: u64 = parts[0].parse().ok()?;
    let m: u64 = parts[1].parse().ok()?;
    let (whole_seconds, fraction) = parts[2].split_once('.').unwrap_or((parts[2], ""));
    let sec: u64 = whole_seconds.parse().ok()?;
    let millis = if fraction.is_empty() {
        0
    } else {
        let digits = fraction.chars().take(3).collect::<String>();
        let value: u64 = digits.parse().ok()?;
        value * 10u64.pow(3u32.saturating_sub(digits.len() as u32))
    };
    Some(Duration::from_secs(h * 3600 + m * 60 + sec) + Duration::from_millis(millis))
}

pub fn extract_title_from_didl(didl_escaped: &str) -> String {
    // DIDL is escaped once: &lt;DIDL-Lite ...&gt;<dc:title>Foo</dc:title>
    // quick approach: unescape then search for <dc:title> or <upnp:class>
    let unescaped = didl_escaped
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&apos;", "'");
    // naive search
    if let Some(start) = unescaped.find("<dc:title>") {
        if let Some(end) = unescaped[start..].find("</dc:title>") {
            let title = &unescaped[start + "<dc:title>".len()..start + end];
            if !title.trim().is_empty() {
                return title.trim().to_string();
            }
        }
    }
    if let Some(start) = unescaped.find("<title>") {
        if let Some(end) = unescaped[start..].find("</title>") {
            let title = &unescaped[start + "<title>".len()..start + end];
            if !title.trim().is_empty() {
                return title.trim().to_string();
            }
        }
    }
    String::new()
}

pub fn extract_artist_from_didl(didl_escaped: &str) -> String {
    let unescaped = didl_escaped
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&apos;", "'");
    for tag in ["<upnp:artist>", "<dc:creator>", "<artist>"] {
        let close_tag = format!("</{}>", &tag[1..tag.len() - 1]);
        if let Some(start) = unescaped.find(tag) {
            if let Some(end) = unescaped[start..].find(&close_tag) {
                let val = &unescaped[start + tag.len()..start + end];
                if !val.trim().is_empty() {
                    return val.trim().to_string();
                }
            }
        }
    }
    String::new()
}

pub fn extract_duration_from_didl(didl_escaped: &str) -> Option<Duration> {
    let unescaped = didl_escaped
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&apos;", "'");
    if let Some(pos) = unescaped.find("duration=\"") {
        let after = &unescaped[pos + 10..];
        if let Some(end) = after.find('"') {
            let dur_str = &after[..end];
            return parse_time(dur_str);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fractional_upnp_time() {
        assert_eq!(
            parse_time("1:02:03.5"),
            Some(Duration::from_millis(3_723_500))
        );
        assert_eq!(parse_time("0:00:01.25"), Some(Duration::from_millis(1_250)));
        assert_eq!(parse_time("0:00:01"), Some(Duration::from_secs(1)));
    }
}
