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
                let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
                let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
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
                            let command_finished = trimmed.starts_with("status: returned");
                            if !trimmed.is_empty() {
                                responses.push(trimmed);
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

    fn disconnect(&mut self) {
        self.rc_stream = None;
        self.loaded_uri = None;
        self.pending_seek = None;
        self.state = PlaybackState::Stopped;
    }

    pub fn poll_sync(&mut self) {
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
        let next_state = match (reported_state, active) {
            (Some(PlaybackState::Stopped), Some(true)) => {
                // `is_playing=1` means VLC still has an active item (including
                // paused input), so a stale stopped event cannot override it.
                Some(if self.state == PlaybackState::Paused {
                    PlaybackState::Paused
                } else {
                    PlaybackState::Playing
                })
            }
            (Some(state), Some(true)) => Some(state),
            (Some(PlaybackState::Stopped), Some(false)) => Some(PlaybackState::Stopped),
            (Some(PlaybackState::Playing), Some(false)) => Some(PlaybackState::Stopped),
            (Some(PlaybackState::Paused), Some(false)) => Some(PlaybackState::Stopped),
            (Some(PlaybackState::Transitioning), Some(false)) => Some(PlaybackState::Stopped),
            (Some(state), None) => Some(state),
            (None, Some(false)) => Some(PlaybackState::Stopped),
            (None, Some(true)) if self.state == PlaybackState::Paused => {
                Some(PlaybackState::Paused)
            }
            (None, Some(true)) => Some(PlaybackState::Playing),
            (None, None) => None,
        };
        if let Some(state) = next_state {
            if self.state != state {
                tracing::info!("VLC state changed: {:?} -> {:?}", self.state, state);
            }
            self.state = state;
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
            let _ = self.send_cmd("stop");
            let _ = self.send_cmd("clear");
        }

        self.uri = uri;
        self.title = title;
        self.loaded_uri = None;
        self.position = Duration::from_secs(0);
        self.duration = Duration::from_secs(0);
        self.last_pos_time = Instant::now();
        self.pending_seek = None;
        self.state = PlaybackState::Stopped;
    }

    fn play(&mut self) -> anyhow::Result<()> {
        if self.uri.is_empty() {
            anyhow::bail!("No URI set");
        }

        if !self.ensure_vlc() {
            anyhow::bail!("VLC is not available");
        }

        self.poll_sync();
        if self.rc_stream.is_none() {
            anyhow::bail!("VLC remote-control connection was lost");
        }

        match self.state {
            PlaybackState::Paused => {
                tracing::info!("VlcPlayer resuming playback");
                // `pause` is a toggle in VLC's RC interface. `play` resumes
                // the current playlist item without inverting an already
                // playing state.
                self.send_cmd("pause");
                if self.rc_stream.is_none() {
                    self.disconnect();
                    anyhow::bail!("VLC remote-control connection was lost");
                }
                self.state = PlaybackState::Playing;
                self.last_pos_time = Instant::now();
            }
            PlaybackState::Playing | PlaybackState::Transitioning
                if self.loaded_uri.as_deref() == Some(self.uri.as_str()) =>
            {
                // Play is idempotent.  In particular, never implement it as
                // a blind VLC `pause` toggle when VLC is already playing.
            }
            _ => {
                tracing::info!("VlcPlayer starting playback for {}", self.uri);
                self.send_cmd("clear");
                self.send_cmd(&format!("add {}", self.uri));
                if self.rc_stream.is_none() {
                    self.disconnect();
                    anyhow::bail!("VLC remote-control connection was lost");
                }
                self.loaded_uri = Some(self.uri.clone());
                self.state = PlaybackState::Playing;
                self.last_pos_time = Instant::now();

                if let Some((target, _)) = self.pending_seek {
                    self.send_cmd(&format!("seek {}", target.as_secs()));
                    self.pending_seek = Some((target, Instant::now()));
                }
            }
        }
        Ok(())
    }

    fn pause(&mut self) -> anyhow::Result<()> {
        if self.rc_stream.is_some() {
            self.poll_sync();
        }
        if self.state == PlaybackState::Playing || self.state == PlaybackState::Transitioning {
            tracing::info!("VlcPlayer pausing playback");
            self.send_cmd("pause");
            if self.rc_stream.is_none() {
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
            self.send_cmd("stop");
            if self.rc_stream.is_none() {
                self.disconnect();
            }
        }
        self.state = PlaybackState::Stopped;
        self.position = Duration::from_secs(0);
        self.last_pos_time = Instant::now();
        self.pending_seek = None;
        self.loaded_uri = None;
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
            self.send_cmd(&format!("seek {}", target.as_secs()));
            if self.rc_stream.is_none() {
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
        self.send_cmd(&format!("volume {}", vlc_vol));
        Ok(())
    }

    fn set_mute(&mut self, mute: bool) -> anyhow::Result<()> {
        self.mute = mute;
        tracing::info!("VlcPlayer set_mute {}", mute);
        if mute {
            self.send_cmd("volume 0");
        } else {
            let vlc_vol = (self.volume as u32 * 256) / 100;
            self.send_cmd(&format!("volume {}", vlc_vol));
        }
        Ok(())
    }

    fn get_position(&self) -> (Duration, Duration) {
        if self.state == PlaybackState::Playing {
            let elapsed = self.last_pos_time.elapsed();
            let estimated = self.position + elapsed;
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
}

impl Drop for VlcPlayer {
    fn drop(&mut self) {
        let _ = self.send_cmd("quit");
        if let Some(ref mut child) = self.child {
            let _ = child.kill();
        }
    }
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
}
