#![allow(dead_code)]

use super::{MediaPlayer, PlaybackState};
use std::time::{Duration, Instant};

pub struct MockPlayer {
    uri: String,
    title: String,
    uri_duration: Duration,
    state: PlaybackState,
    volume: u8,
    mute: bool,
    // position tracking
    base_position: Duration,
    play_start: Option<Instant>,
}

impl MockPlayer {
    pub fn new() -> Self {
        Self {
            uri: String::new(),
            title: String::new(),
            uri_duration: Duration::from_secs(0),
            state: PlaybackState::Stopped,
            volume: 100,
            mute: false,
            base_position: Duration::from_secs(0),
            play_start: None,
        }
    }

    fn current_pos(&self) -> Duration {
        if self.state == PlaybackState::Playing {
            if let Some(start) = self.play_start {
                let elapsed = start.elapsed();
                let pos = self.base_position + elapsed;
                if self.uri_duration.as_secs() > 0 && pos > self.uri_duration {
                    return self.uri_duration;
                }
                return pos;
            }
        }
        self.base_position
    }

    fn spawn_external(&self) {
        if self.uri.is_empty() {
            return;
        }
        // Try VLC first, else system default
        if let Some(vlc_path) = super::find_vlc() {
            tracing::info!("Spawning VLC: {:?} {}", vlc_path, self.uri);
            let uri = self.uri.clone();
            std::thread::spawn(move || {
                let _ = std::process::Command::new(vlc_path)
                    .arg("--play-and-exit")
                    .arg(uri)
                    .spawn();
            });
        } else {
            // Fallback: open with the system default handler.
            let uri = self.uri.clone();
            tracing::info!("Spawning system default player for {}", uri);
            std::thread::spawn(move || {
                crate::platform::open_url(&uri);
            });
        }
    }

    fn kill_external(&self) {
        // We do not aggressively kill external players on stop/pause for MVP
        // "keep ready if window closes" means we don't kill on pause.
        // On stop, we could try to kill vlc but we leave it to user to close window.
        // This matches user choice: keep daemon ready even if window closed.
    }
}

impl MediaPlayer for MockPlayer {
    fn set_uri(&mut self, uri: String, title: String) {
        tracing::info!("Player set_uri: {} title={}", uri, title);
        self.uri = uri;
        self.title = title;
        self.base_position = Duration::from_secs(0);
        self.play_start = None;
        self.state = PlaybackState::Stopped;
        // heuristic duration: unknown -> 0, let BubbleUPnP handle
        self.uri_duration = Duration::from_secs(0);
        // close previous? not needed
    }

    fn play(&mut self) -> anyhow::Result<()> {
        if self.uri.is_empty() {
            anyhow::bail!("no uri set");
        }
        tracing::info!("Player play: {}", self.uri);
        if self.state == PlaybackState::Paused {
            // resume
            self.play_start = Some(Instant::now());
            self.state = PlaybackState::Playing;
        } else if self.state == PlaybackState::Stopped {
            // fresh play -> spawn external window
            self.base_position = Duration::from_secs(0);
            self.play_start = Some(Instant::now());
            self.state = PlaybackState::Playing;
            self.spawn_external();
        } else if self.state == PlaybackState::Playing {
            // already playing
        }
        Ok(())
    }

    fn pause(&mut self) -> anyhow::Result<()> {
        tracing::info!("Player pause");
        if self.state == PlaybackState::Playing {
            let pos = self.current_pos();
            self.base_position = pos;
            self.play_start = None;
            self.state = PlaybackState::Paused;
        }
        Ok(())
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        tracing::info!("Player stop");
        self.base_position = Duration::from_secs(0);
        self.play_start = None;
        self.state = PlaybackState::Stopped;
        // We could kill external player, but per "keep ready" we don't force close window on stop from BubbleUPnP?
        // Actually Stop from BubbleUPnP should stop playback but not kill daemon. We leave window open - user closes manually.
        // Optionally kill vlc if we spawned it, but we choose not to for now.
        self.kill_external();
        Ok(())
    }

    fn seek(&mut self, target: Duration) -> anyhow::Result<()> {
        tracing::info!("Player seek to {:?}", target);
        self.base_position = target;
        if self.state == PlaybackState::Playing {
            self.play_start = Some(Instant::now());
        }
        // For external player, seek is not propagated to real window in MVP (limitation).
        // TODO: implement via VLC RC / MPV IPC for accurate seek.
        Ok(())
    }

    fn set_volume(&mut self, vol: u8) -> anyhow::Result<()> {
        self.volume = vol.min(100);
        tracing::info!("Player set_volume {}", self.volume);
        Ok(())
    }

    fn set_mute(&mut self, mute: bool) -> anyhow::Result<()> {
        self.mute = mute;
        tracing::info!("Player set_mute {}", mute);
        Ok(())
    }

    fn get_position(&self) -> (Duration, Duration) {
        (self.current_pos(), self.uri_duration)
    }

    fn get_state(&self) -> PlaybackState {
        self.state
    }

    fn current_uri(&self) -> String {
        self.uri.clone()
    }
}
