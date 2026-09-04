use super::{MediaPlayer, PlaybackState, find_vlc};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

pub struct VlcPlayer {
    child: Option<Child>,
    rc_stream: Option<BufReader<TcpStream>>,
    rc_port: u16,
    uri: String,
    title: String,
    loaded_uri: Option<String>,
    duration: Duration,
    position: Duration,
    last_pos_time: Instant,
    pending_seek: Option<(Duration, Instant)>,
    state: PlaybackState,
    volume: u8,
    mute: bool,
    transition_start: Option<Instant>,
    subtitles_checked: bool,
    audio_checked: bool,
    stopped_at: Option<Instant>,
}

impl VlcPlayer {
    pub fn new() -> Self {
        Self {
            child: None,
            rc_stream: None,
            rc_port: 52422,
            uri: String::new(),
            title: String::new(),
            loaded_uri: None,
            duration: Duration::from_secs(0),
            position: Duration::from_secs(0),
            last_pos_time: Instant::now(),
            pending_seek: None,
            state: PlaybackState::Stopped,
            volume: 100,
            mute: false,
            transition_start: None,
            subtitles_checked: false,
            audio_checked: false,
            stopped_at: None,
        }
    }

    fn ensure_vlc(&mut self) -> bool {
        // Check if child is still running
        if let Some(ref mut child) = self.child {
            if let Ok(Some(_)) = child.try_wait() {
                tracing::info!("VLC process previously exited, will restart");
                self.child = None;
                self.rc_stream = None;
                self.loaded_uri = None;
                self.pending_seek = None;
                self.state = PlaybackState::Stopped;
                self.transition_start = None;
            }
        }

        if self.rc_stream.is_some() && self.child.is_some() {
            return true;
        }

        // A transient RC disconnect must not spawn a second VLC instance.
        // Reattach to the still-running process first.
        if self.child.is_some() && self.connect_rc() {
            tracing::info!("Reconnected to VLC RC interface");
            return true;
        }

        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
        }
        self.rc_stream = None;

        let vlc_path = match find_vlc() {
            Some(p) => p,
            None => return false,
        };

        let port = self.rc_port;
        tracing::info!("Launching VLC with RC interface on 127.0.0.1:{}", port);

        let child = match Command::new(&vlc_path)
            .args([
                "--extraintf",
                "rc",
                "--rc-host",
                &format!("127.0.0.1:{}", port),
                "--rc-quiet",
                "--no-video-title-show",
                "--fullscreen",
                "--audio-language=en,eng,English",
                // Hardware acceleration for video decoding on Windows (Direct3D11 / DXVA2)
                "--avcodec-hw=any",
                // Enhanced buffering and resilience for weak Wi-Fi / lossy connections
                "--network-caching=5000",
                "--file-caching=3000",
                "--live-caching=3000",
                "--http-reconnect",
                "--clock-jitter=5000",
            ])
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("Failed to spawn VLC: {}", e);
                return false;
            }
        };
        self.child = Some(child);

        if self.connect_rc() {
            tracing::info!("Connected to VLC RC interface");
            let vlc_vol = (self.volume as u32 * 256) / 100;
            let _ = self.send_cmd(&format!("volume {}", vlc_vol));
            true
        } else {
            tracing::error!("Could not connect to VLC RC port {}", port);
            if let Some(mut c) = self.child.take() {
                let _ = c.kill();
            }
            false
        }
    }

    fn connect_rc(&mut self) -> bool {
        let addr = format!("127.0.0.1:{}", self.rc_port);
        for _ in 0..5 {
            if let Ok(stream) = TcpStream::connect(&addr) {
                let _ = stream.set_read_timeout(Some(Duration::from_millis(300)));
                let _ = stream.set_write_timeout(Some(Duration::from_millis(300)));
                self.rc_stream = Some(BufReader::new(stream));
                return true;
            }
            std::thread::sleep(Duration::from_millis(75));
        }
        false
    }

    fn send_cmd(&mut self, cmd: &str) -> Vec<String> {
        let mut responses = Vec::new();
        let mut failed = false;
        let is_single_line = matches!(cmd, "is_playing" | "get_time" | "get_length");
        if let Some(reader) = self.rc_stream.as_mut() {
            let msg = format!("{}\n", cmd);
            let stream = reader.get_mut();
            if stream.write_all(msg.as_bytes()).is_err() || stream.flush().is_err() {
                failed = true;
            } else {
                let mut line = String::new();
                loop {
                    match reader.read_line(&mut line) {
                        Ok(0) => break,
                        Ok(_) => {
                            let trimmed = line.trim().to_string();
                            let command_finished = trimmed.starts_with("status: returned")
                                || trimmed.contains("end of stream info");
                            if !trimmed.is_empty() {
                                let is_digit_answer = trimmed.chars().all(|c| c.is_ascii_digit());
                                responses.push(trimmed);
                                // For single-line numeric queries (get_time, get_length, is_playing),
                                // return immediately once the numeric response arrives instead of
                                // waiting for socket read timeouts.
                                if is_single_line && is_digit_answer {
                                    break;
                                }
                            }
                            line.clear();
                            if command_finished {
                                break;
                            }
                        }
                        Err(e)
                            if e.kind() == std::io::ErrorKind::TimedOut
                                || e.kind() == std::io::ErrorKind::WouldBlock =>
                        {
                            break;
                        }
                        Err(_) => {
                            failed = true;
                            break;
                        }
                    }
                }
            }
        }
        if failed {
            self.rc_stream = None;
        }
        responses
    }

    /// Send a write-only command without blocking on read timeouts (VLC RC does not reply to write commands)
    fn send_cmd_fire_and_forget(&mut self, cmd: &str) -> bool {
        if let Some(reader) = self.rc_stream.as_mut() {
            let msg = format!("{}\n", cmd);
            let stream = reader.get_mut();
            if stream.write_all(msg.as_bytes()).is_ok() && stream.flush().is_ok() {
                return true;
            }
        }
        self.rc_stream = None;
        false
    }

    fn disconnect(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
        }
        self.rc_stream = None;
        self.loaded_uri = None;
        self.pending_seek = None;
        self.state = PlaybackState::Stopped;
        self.transition_start = None;
        self.subtitles_checked = false;
        self.audio_checked = false;
    }

    fn poll_sync_internal(&mut self) {
        // VLC is authoritative.  Poll while paused/stopped as well so a
        // click in VLC's own window is reflected in the DLNA state.
        if self.rc_stream.is_none() {
            return;
        }

        let status_res = self.send_cmd("status");
        if self.rc_stream.is_none() {
            self.disconnect();
            return;
        }
        // VLC's RC `is_playing` is true for both playing and paused input.
        // It prevents an old asynchronous `status` line from making us
        // issue a contradictory command.
        let active_res = self.send_cmd("is_playing");
        if self.rc_stream.is_none() {
            self.disconnect();
            return;
        }
        let previous_position = self.position;
        let reported_state = parse_vlc_state(&status_res);
        let active = parse_rc_bool(&active_res);

        // Allow up to 35 seconds for initial connection & buffer fill
        let transition_timed_out = self
            .transition_start
            .map(|start| start.elapsed() > Duration::from_secs(35))
            .unwrap_or(false);
        // During the first 3 seconds of a new track loading, ignore transient "stopped" lines from VLC
        let just_started_transition = self
            .transition_start
            .map(|start| start.elapsed() < Duration::from_secs(3))
            .unwrap_or(false);

        let next_state = match (reported_state, active) {
            // While VLC is opening or buffering (state: 1 or 2), is_playing is 0.
            // Never treat buffering as Stopped!
            (Some(PlaybackState::Transitioning), _) => {
                if transition_timed_out {
                    Some(PlaybackState::Stopped)
                } else {
                    Some(PlaybackState::Transitioning)
                }
            }
            (Some(PlaybackState::Stopped), Some(true)) => {
                // `is_playing=1` means VLC still has an active item (including
                // paused input), so a stale stopped event cannot override it.
                Some(if self.state == PlaybackState::Paused {
                    PlaybackState::Paused
                } else {
                    PlaybackState::Playing
                })
            }
            (Some(PlaybackState::Playing), Some(true)) => Some(PlaybackState::Playing),
            (Some(PlaybackState::Paused), Some(true)) => Some(PlaybackState::Paused),
            (Some(PlaybackState::Stopped), Some(false) | None) => {
                if self.state == PlaybackState::Transitioning && just_started_transition {
                    Some(PlaybackState::Transitioning)
                } else {
                    Some(PlaybackState::Stopped)
                }
            }
            (Some(PlaybackState::Playing | PlaybackState::Paused), Some(false)) => {
                if self.state == PlaybackState::Transitioning && just_started_transition {
                    Some(PlaybackState::Transitioning)
                } else {
                    Some(PlaybackState::Stopped)
                }
            }
            (Some(state), None) => Some(state),
            (None, Some(false)) => {
                // When loading over weak Wi-Fi, status may not emit a new state line each tick.
                // Keep Transitioning during the loading grace period instead of prematurely stopping.
                if self.state == PlaybackState::Transitioning && !transition_timed_out {
                    Some(PlaybackState::Transitioning)
                } else {
                    Some(PlaybackState::Stopped)
                }
            }
            (None, Some(true)) if self.state == PlaybackState::Paused => {
                Some(PlaybackState::Paused)
            }
            (None, Some(true)) => Some(PlaybackState::Playing),
            (None, None) => None,
        };
        if let Some(state) = next_state {
            if self.state != state {
                tracing::info!("VLC state changed: {:?} -> {:?}", self.state, state);
                if state == PlaybackState::Playing {
                    self.transition_start = None;
                    self.stopped_at = None;
                    self.last_pos_time = Instant::now();
                } else if state == PlaybackState::Transitioning {
                    self.stopped_at = None;
                    self.subtitles_checked = false;
                    self.audio_checked = false;
                } else if state == PlaybackState::Stopped {
                    self.stopped_at = Some(Instant::now());
                }
            }
            self.state = state;
        }

        // Close VLC if it has been continuously stopped/idle for over 3 seconds
        if self.state == PlaybackState::Stopped {
            if let Some(stopped_at) = self.stopped_at {
                if stopped_at.elapsed() > Duration::from_secs(3) {
                    tracing::info!("VLC idle for >3s, closing player");
                    let _ = self.send_cmd_fire_and_forget("quit");
                    if let Some(mut child) = self.child.take() {
                        let _ = child.kill();
                    }
                    self.rc_stream = None;
                    self.loaded_uri = None;
                    self.pending_seek = None;
                    self.transition_start = None;
                    self.stopped_at = None;
                    self.subtitles_checked = false;
                    self.audio_checked = false;
                }
            }
        }

        // Also check if VLC reported a new input in status lines (track changed)
        for line in &status_res {
            if line.to_ascii_lowercase().contains("new input:") {
                self.subtitles_checked = false;
                self.audio_checked = false;
            }
        }

        // Search for subtitles: primarily 'sdh' + 'english', then 'english'; otherwise do not set.
        if self.state == PlaybackState::Playing && !self.subtitles_checked {
            let strack_res = self.send_cmd("strack");
            if let Some(track_id) = find_best_subtitle_track(&strack_res) {
                tracing::info!("Auto-selected subtitle track: {}", track_id);
                self.send_cmd_fire_and_forget(&format!("strack {}", track_id));
                self.subtitles_checked = true;
            } else if !strack_res.is_empty() {
                tracing::info!("No matching SDH/English subtitle track found; leaving disabled");
                self.subtitles_checked = true;
            }
        }

        // Search for audio: find track with 'eng' or 'english' and apply it
        if self.state == PlaybackState::Playing && !self.audio_checked {
            let atrack_res = self.send_cmd("atrack");
            if let Some(track_id) = find_best_audio_track(&atrack_res) {
                tracing::info!("Auto-selected English audio track: {}", track_id);
                self.send_cmd_fire_and_forget(&format!("atrack {}", track_id));
                self.audio_checked = true;
            } else if !atrack_res.is_empty() {
                tracing::info!("No English audio track found; leaving default");
                self.audio_checked = true;
            }
        }

        // Read the actual clock even when paused or buffering.  Never
        // extrapolate while VLC reports either of those states.
        let time_res = self.send_cmd("get_time");
        if self.rc_stream.is_none() {
            self.disconnect();
            return;
        }
        if let Some(sample) = parse_rc_seconds(&time_res) {
            // VLC applies a seek asynchronously.  During that short window
            // get_time can still report the old position.  Keep reporting the
            // requested target until VLC catches up, then return to samples.
            let seek_is_settling = self
                .pending_seek
                .map(|(_, issued)| issued.elapsed() < Duration::from_millis(1200))
                .unwrap_or(false);
            let accept_sample = match self.pending_seek {
                Some((target, issued)) if issued.elapsed() < Duration::from_millis(1200) => {
                    target.abs_diff(sample) <= Duration::from_secs(2)
                }
                _ if self.state == PlaybackState::Transitioning
                    && !seek_is_settling
                    && sample + Duration::from_secs(2) < previous_position =>
                {
                    // During network rebuffering VLC can briefly report zero
                    // (or an old timestamp). Preserve the last known clock
                    // instead of making the controller jump backwards.
                    false
                }
                _ if sample == Duration::ZERO
                    && previous_position > Duration::from_secs(3)
                    && !seek_is_settling =>
                {
                    // Ignore spurious 0s when VLC reconnects stream over weak Wi-Fi
                    false
                }
                _ => true,
            };
            if accept_sample {
                self.position = sample;
                self.pending_seek = None;
                self.last_pos_time = Instant::now();
            }
        }

        if self.duration.as_secs() == 0 {
            let len_res = self.send_cmd("get_length");
            if self.rc_stream.is_none() {
                self.disconnect();
                return;
            }
            if let Some(length) = parse_rc_seconds(&len_res) {
                if length > Duration::ZERO {
                    self.duration = length;
                }
            }
        }
    }
}

fn parse_rc_seconds(lines: &[String]) -> Option<Duration> {
    lines
        .iter()
        .find_map(|line| line.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

fn parse_rc_bool(lines: &[String]) -> Option<bool> {
    lines.iter().rev().find_map(|line| match line.trim() {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    })
}

fn parse_vlc_state(lines: &[String]) -> Option<PlaybackState> {
    // RC can return more than one asynchronous state event. The final state
    // in the response is the one that should drive the renderer.
    for line in lines.iter().rev() {
        let lower = line.to_ascii_lowercase();
        if let Some(index) = lower.find("state:") {
            let digits: String = lower[index + "state:".len()..]
                .trim_start()
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(code) = digits.parse::<u8>() {
                return Some(match code {
                    1 | 2 => PlaybackState::Transitioning,
                    3 => PlaybackState::Playing,
                    4 => PlaybackState::Paused,
                    0 | 5 | 6 | 7 => PlaybackState::Stopped,
                    _ => return None,
                });
            }
        }
    }

    let text = lines.join(" ").to_ascii_lowercase();
    if text.contains("buffer") || text.contains("opening") {
        Some(PlaybackState::Transitioning)
    } else if text.contains("playing") {
        Some(PlaybackState::Playing)
    } else if text.contains("paused") {
        Some(PlaybackState::Paused)
    } else if text.contains("stopped") || text.contains("ended") || text.contains("error") {
        Some(PlaybackState::Stopped)
    } else {
        None
    }
}

impl MediaPlayer for VlcPlayer {
    fn set_uri(&mut self, uri: String, title: String) {
        tracing::info!("VlcPlayer set_uri: {} (title: {})", uri, title);

        // A SetAVTransportURI is a replacement, not just metadata.  Clear
        // VLC's old playlist so a later Play can never resume the old item.
        if self.uri != uri && self.rc_stream.is_some() {
            let _ = self.send_cmd_fire_and_forget("stop");
            let _ = self.send_cmd_fire_and_forget("clear");
        }

        self.uri = uri;
        self.title = title;
        self.loaded_uri = None;
        self.position = Duration::from_secs(0);
        self.duration = Duration::from_secs(0);
        self.last_pos_time = Instant::now();
        self.pending_seek = None;
        self.state = PlaybackState::Stopped;
        self.transition_start = None;
        self.subtitles_checked = false;
        self.audio_checked = false;
    }

    fn play(&mut self) -> anyhow::Result<()> {
        if self.uri.is_empty() {
            anyhow::bail!("No URI set");
        }

        if !self.ensure_vlc() {
            anyhow::bail!("VLC is not available");
        }

        match self.state {
            PlaybackState::Paused => {
                tracing::info!("VlcPlayer resuming playback");
                // `pause` is a toggle in VLC's RC interface.
                if !self.send_cmd_fire_and_forget("pause") {
                    self.disconnect();
                    anyhow::bail!("VLC remote-control connection was lost");
                }
                self.state = PlaybackState::Playing;
                self.last_pos_time = Instant::now();
            }
            PlaybackState::Playing | PlaybackState::Transitioning
                if self.loaded_uri.as_deref() == Some(self.uri.as_str()) =>
            {
                // Play is idempotent.
            }
            _ => {
                tracing::info!("VlcPlayer starting playback for {}", self.uri);
                let _ = self.send_cmd_fire_and_forget("clear");
                if !self.send_cmd_fire_and_forget(&format!("add {}", self.uri)) {
                    self.disconnect();
                    anyhow::bail!("VLC remote-control connection was lost");
                }
                self.loaded_uri = Some(self.uri.clone());
                self.state = PlaybackState::Transitioning;
                self.transition_start = Some(Instant::now());
                self.stopped_at = None;
                self.last_pos_time = Instant::now();
                self.subtitles_checked = false;
                self.audio_checked = false;

                if let Some((target, _)) = self.pending_seek {
                    let _ = self.send_cmd_fire_and_forget(&format!("seek {}", target.as_secs()));
                    self.pending_seek = Some((target, Instant::now()));
                }
            }
        }
        Ok(())
    }

    fn pause(&mut self) -> anyhow::Result<()> {
        if self.state == PlaybackState::Playing || self.state == PlaybackState::Transitioning {
            tracing::info!("VlcPlayer pausing playback");
            if !self.send_cmd_fire_and_forget("pause") {
                self.disconnect();
                anyhow::bail!("VLC remote-control connection was lost");
            }
            self.state = PlaybackState::Paused;
            self.last_pos_time = Instant::now();
        }
        Ok(())
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        tracing::info!("VlcPlayer stopping playback");
        if self.rc_stream.is_some() {
            let _ = self.send_cmd_fire_and_forget("stop");
        }
        self.state = PlaybackState::Stopped;
        self.stopped_at = Some(Instant::now());
        self.position = Duration::from_secs(0);
        self.last_pos_time = Instant::now();
        self.pending_seek = None;
        self.loaded_uri = None;
        self.transition_start = None;
        self.subtitles_checked = false;
        self.audio_checked = false;
        Ok(())
    }

    fn seek(&mut self, target: Duration) -> anyhow::Result<()> {
        if self.uri.is_empty() {
            anyhow::bail!("No URI set");
        }
        let target = if self.duration > Duration::ZERO {
            target.min(self.duration)
        } else {
            target
        };
        tracing::info!("VlcPlayer seek to {}s", target.as_secs());
        if self.rc_stream.is_some() {
            if !self.send_cmd_fire_and_forget(&format!("seek {}", target.as_secs())) {
                self.disconnect();
                anyhow::bail!("VLC remote-control connection was lost");
            }
        }
        self.position = target;
        self.last_pos_time = Instant::now();
        self.pending_seek = Some((target, Instant::now()));
        Ok(())
    }

    fn set_volume(&mut self, vol: u8) -> anyhow::Result<()> {
        self.volume = vol.min(100);
        let vlc_vol = (self.volume as u32 * 256) / 100;
        tracing::info!("VlcPlayer set_volume {}% (vlc: {})", self.volume, vlc_vol);
        self.send_cmd_fire_and_forget(&format!("volume {}", vlc_vol));
        Ok(())
    }

    fn set_mute(&mut self, mute: bool) -> anyhow::Result<()> {
        self.mute = mute;
        tracing::info!("VlcPlayer set_mute {}", mute);
        if mute {
            self.send_cmd_fire_and_forget("volume 0");
        } else {
            let vlc_vol = (self.volume as u32 * 256) / 100;
            self.send_cmd_fire_and_forget(&format!("volume {}", vlc_vol));
        }
        Ok(())
    }

    fn get_position(&self) -> (Duration, Duration) {
        if self.state == PlaybackState::Playing {
            // Cap subsecond extrapolation to 950ms.
            // On weak Wi-Fi connections, network stalls or packet delays must
            // never cause the estimated timeline to race ahead of what VLC
            // is actually rendering, preventing timeline jumps and desync.
            let subsecond = self.last_pos_time.elapsed().min(Duration::from_millis(950));
            let estimated = self.position + subsecond;
            if self.duration.as_secs() > 0 && estimated > self.duration {
                (self.duration, self.duration)
            } else {
                (estimated, self.duration)
            }
        } else {
            (self.position, self.duration)
        }
    }

    fn get_state(&self) -> PlaybackState {
        self.state
    }

    fn current_uri(&self) -> String {
        self.uri.clone()
    }

    fn poll_sync(&mut self) {
        self.poll_sync_internal();
    }
}

impl Drop for VlcPlayer {
    fn drop(&mut self) {
        let _ = self.send_cmd_fire_and_forget("quit");
        if let Some(ref mut child) = self.child {
            let _ = child.kill();
        }
    }
}

pub fn find_best_subtitle_track(lines: &[String]) -> Option<i32> {
    let mut sdh_english = None;
    let mut english = None;

    for line in lines {
        let trimmed = line.trim();
        let clean = trimmed.strip_prefix('|').unwrap_or(trimmed).trim();
        if let Some((id_str, desc)) = clean.split_once(" - ") {
            let id_clean = id_str.trim().trim_matches('*').trim();
            if let Ok(id) = id_clean.parse::<i32>() {
                if id < 0 {
                    continue; // Skip "-1 - Disable"
                }
                let desc_lower = desc.to_lowercase();
                let is_eng = desc_lower.contains("english") || desc_lower.contains("eng");
                let is_sdh = desc_lower.contains("sdh");

                if is_sdh && is_eng && sdh_english.is_none() {
                    sdh_english = Some(id);
                } else if is_eng && english.is_none() {
                    english = Some(id);
                }
            }
        }
    }

    sdh_english.or(english)
}

pub fn find_best_audio_track(lines: &[String]) -> Option<i32> {
    for line in lines {
        let trimmed = line.trim();
        let clean = trimmed.strip_prefix('|').unwrap_or(trimmed).trim();
        if let Some((id_str, desc)) = clean.split_once(" - ") {
            let id_clean = id_str.trim().trim_matches('*').trim();
            if let Ok(id) = id_clean.parse::<i32>() {
                if id < 0 {
                    continue; // Skip "-1 - Disable"
                }
                let desc_lower = desc.to_lowercase();
                if desc_lower.contains("english") || desc_lower.contains("eng") {
                    return Some(id);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn parses_vlc_numeric_states() {
        assert_eq!(
            parse_vlc_state(&lines(&["status change: ( state: 3 )"])),
            Some(PlaybackState::Playing)
        );
        assert_eq!(
            parse_vlc_state(&lines(&["status change: ( state: 4 )"])),
            Some(PlaybackState::Paused)
        );
        assert_eq!(
            parse_vlc_state(&lines(&["status change: ( stop state: 5 )"])),
            Some(PlaybackState::Stopped)
        );
        assert_eq!(
            parse_vlc_state(&lines(&["status change: ( state: 2 )"])),
            Some(PlaybackState::Transitioning)
        );
    }

    #[test]
    fn parses_vlc_text_states() {
        assert_eq!(
            parse_vlc_state(&lines(&["buffering network input"])),
            Some(PlaybackState::Transitioning)
        );
        assert_eq!(
            parse_vlc_state(&lines(&["playing"])),
            Some(PlaybackState::Playing)
        );
        assert_eq!(parse_vlc_state(&lines(&["unknown response"])), None);
    }

    #[test]
    fn test_position_capped_during_stall() {
        let mut player = VlcPlayer::new();
        player.state = PlaybackState::Playing;
        player.position = Duration::from_secs(10);
        player.duration = Duration::from_secs(100);
        // Simulate a 5-second Wi-Fi stall/rebuffering
        player.last_pos_time = Instant::now() - Duration::from_secs(5);

        let (pos, _) = player.get_position();
        // Extrapolation must be capped under 1s (10s + 950ms) rather than drifting to 15s
        assert!(pos <= Duration::from_millis(10950));
        assert!(pos >= Duration::from_secs(10));
    }

    #[test]
    fn test_find_best_subtitle_track() {
        // Case 1: Both SDH English and regular English -> choose SDH English
        let sample_both = lines(&[
            "+----[ Subtitles track ]",
            "| -1 - Disable",
            "| 1 - English [eng]",
            "| 2 - English (SDH) [eng]",
            "| 3 - Spanish [spa]",
            "+----[ end of stream info ]",
        ]);
        assert_eq!(find_best_subtitle_track(&sample_both), Some(2));

        // Case 2: Only regular English -> choose English
        let sample_eng_only = lines(&[
            "+----[ Subtitles track ]",
            "| -1 - Disable",
            "| 1* - English [eng]",
            "| 2 - French [fre]",
            "+----[ end of stream info ]",
        ]);
        assert_eq!(find_best_subtitle_track(&sample_eng_only), Some(1));

        // Case 3: No English at all -> choose None (do not set)
        let sample_no_eng = lines(&[
            "+----[ Subtitles track ]",
            "| -1 - Disable",
            "| 1 - Spanish [spa]",
            "| 2 - Japanese [jpn]",
            "+----[ end of stream info ]",
        ]);
        assert_eq!(find_best_subtitle_track(&sample_no_eng), None);
    }

    #[test]
    fn test_find_best_audio_track() {
        // Case 1: Multiple audio tracks with English
        let sample = lines(&[
            "+----[ Audio track ]",
            "| -1 - Disable",
            "| 1 - Hindi [hin]",
            "| 2* - English [eng]",
            "| 3 - Spanish [spa]",
            "+----[ end of stream info ]",
        ]);
        assert_eq!(find_best_audio_track(&sample), Some(2));

        // Case 2: No English audio track -> returns None
        let sample_no_eng = lines(&[
            "+----[ Audio track ]",
            "| -1 - Disable",
            "| 1 - Japanese [jpn]",
            "+----[ end of stream info ]",
        ]);
        assert_eq!(find_best_audio_track(&sample_no_eng), None);
    }
}
