use super::{MediaPlayer, PlaybackState, find_mpv};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const QUERY_TIMEOUT: Duration = Duration::from_millis(900);
const RETRY_DELAY: Duration = Duration::from_secs(2);
const TRANSITION_TIMEOUT: Duration = Duration::from_secs(35);

pub struct MpvPlayer {
    child: Option<Child>,
    ipc_endpoint: String,
    uri: String,
    title: String,
    loaded_uri: Option<String>,
    duration: Duration,
    position: Duration,
    last_pos_time: Instant,
    state: PlaybackState,
    volume: u8,
    mute: bool,
    transition_start: Option<Instant>,
    next_retry: Instant,
    failures: u32,
    request_id: u64,
}

impl MpvPlayer {
    pub fn new() -> Self {
        Self {
            child: None,
            ipc_endpoint: crate::platform::mpv_ipc_endpoint(),
            uri: String::new(),
            title: String::new(),
            loaded_uri: None,
            duration: Duration::ZERO,
            position: Duration::ZERO,
            last_pos_time: Instant::now(),
            state: PlaybackState::Stopped,
            volume: 100,
            mute: false,
            transition_start: None,
            next_retry: Instant::now(),
            failures: 0,
            request_id: 1,
        }
    }

    fn child_alive(&mut self) -> bool {
        match self.child.as_mut() {
            Some(c) => match c.try_wait() {
                Ok(None) => true,
                _ => {
                    self.child = None;
                    false
                }
            },
            None => false,
        }
    }

    fn spawn(&mut self) -> bool {
        if Instant::now() < self.next_retry {
            return false;
        }
        self.next_retry = Instant::now() + RETRY_DELAY;
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
        }
        #[cfg(not(windows))]
        {
            let sock = std::path::Path::new(&self.ipc_endpoint);
            if sock.exists() {
                let _ = std::fs::remove_file(sock);
            }
        }
        let mpv_path = match find_mpv() {
            Some(p) => p,
            None => return false,
        };
        let mut child = match Command::new(&mpv_path)
            .args(crate::platform::mpv_args(&self.ipc_endpoint))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("Failed to spawn mpv: {}", e);
                return false;
            }
        };
        // Surface early-exit diagnostics (bad args, missing DLLs, no desktop).
        std::thread::sleep(Duration::from_millis(400));
        if let Ok(Some(status)) = child.try_wait() {
            let mut err = String::new();
            if let Some(stderr) = child.stderr.as_mut() {
                use std::io::Read as _;
                let _ = stderr.read_to_string(&mut err);
            }
            eprintln!("mpv exited early status={} stderr={:.1500}", status, err);
            return false;
        }
        // Diagnostics go nowhere from here; drop the pipe so mpv can't block.
        child.stderr = None;
        self.child = Some(child);
        // Wait for the IPC endpoint to accept connections. First-ever mpv
        // start can take several seconds (font cache), so allow ~10s.
        for i in 0..100 {
            if self.ipc_ping() {
                tracing::info!("Connected to mpv IPC at {}", self.ipc_endpoint);
                let vol = self.volume;
                let rid = self.next_id();
                let _ = self.send_cmd(
                    &format!(
                        r#"{{"command":["set_property","volume",{}],"request_id":{}}}"#,
                        vol, rid
                    ),
                    rid,
                );
                self.failures = 0;
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
            if i == 30 {
                tracing::warn!("mpv IPC not up after 3s, still waiting...");
            }
        }
        tracing::warn!("mpv started but IPC never came up");
        false
    }

    fn next_id(&mut self) -> u64 {
        let id = self.request_id;
        self.request_id = self.request_id.wrapping_add(1);
        id
    }

    fn ipc_ping(&mut self) -> bool {
        let id = self.next_id();
        let line = format!(r#"{{"command":["get_property","mpv-version"],"request_id":{}}}"#, id);
        match self.roundtrip(vec![line], 1, QUERY_TIMEOUT) {
            Some(lines) => reply_ok(&lines, id),
            None => false,
        }
    }

    fn send_cmd(&mut self, line: &str, id: u64) -> bool {
        self.roundtrip(vec![line.to_string()], 1, QUERY_TIMEOUT)
            .map(|lines| reply_ok(&lines, id))
            .unwrap_or(false)
    }

    fn roundtrip(&self, lines: Vec<String>, expect: usize, timeout: Duration) -> Option<Vec<String>> {
        let mut conn = Conn::open(&self.ipc_endpoint)?;
        for line in &lines {
            if conn.write_line(line).is_err() {
                return None;
            }
        }
        conn.read_lines_timeout(expect, timeout)
    }

    fn query_props(&mut self) -> Option<MpvSnapshot> {
        let ids: [u64; 8] = std::array::from_fn(|_| self.next_id());
        let cmds = vec![
            format!(r#"{{"command":["get_property","time-pos"],"request_id":{}}}"#, ids[0]),
            format!(r#"{{"command":["get_property","duration"],"request_id":{}}}"#, ids[1]),
            format!(r#"{{"command":["get_property","pause"],"request_id":{}}}"#, ids[2]),
            format!(r#"{{"command":["get_property","idle-active"],"request_id":{}}}"#, ids[3]),
            format!(r#"{{"command":["get_property","eof-reached"],"request_id":{}}}"#, ids[4]),
            format!(r#"{{"command":["get_property","volume"],"request_id":{}}}"#, ids[5]),
            format!(r#"{{"command":["get_property","mute"],"request_id":{}}}"#, ids[6]),
            format!(r#"{{"command":["get_property","seekable"],"request_id":{}}}"#, ids[7]),
        ];
        let lines = self.roundtrip(cmds, 8, QUERY_TIMEOUT)?;
        Some(parse_snapshot(&lines, &ids))
    }

    fn load_current(&mut self) {
        let id = self.next_id();
        // mpv takes a plain path/URL string here, not JSON-escaped separately.
        let payload = serde_json::json!({
            "command": ["loadfile", self.uri, "replace"],
            "request_id": id,
        });
        let _ = self.roundtrip(vec![payload.to_string()], 1, QUERY_TIMEOUT);
        self.loaded_uri = Some(self.uri.clone());
        self.state = PlaybackState::Transitioning;
        self.transition_start = Some(Instant::now());
        self.last_pos_time = Instant::now();
    }

    fn restore_after_respawn(&mut self, resume_pos: Duration, was_paused: bool) {
        self.load_current();
        // loadfile is async and a seek issued while mpv is still opening the
        // file is silently dropped. Wait for the demuxer (seekable), then
        // seek with verify-and-retry so resume always lands near the target.
        for _ in 0..40 {
            match self.query_props() {
                Some(snap) if snap.seekable == Some(true) => break,
                _ => std::thread::sleep(Duration::from_millis(200)),
            }
        }
        if resume_pos > Duration::from_secs(1) {
            for _ in 0..3 {
                let id = self.next_id();
                let payload = serde_json::json!({
                    "command": ["seek", resume_pos.as_secs_f64(), "absolute"],
                    "request_id": id,
                });
                let _ = self.roundtrip(vec![payload.to_string()], 1, QUERY_TIMEOUT);
                std::thread::sleep(Duration::from_millis(300));
                if let Some(snap) = self.query_props()
                    && let Some(t) = snap.time_pos
                    && t + Duration::from_secs(3) >= resume_pos
                {
                    break;
                }
            }
            self.position = resume_pos;
        }
        if was_paused {
            let id = self.next_id();
            let payload = serde_json::json!({
                "command": ["set_property", "pause", true],
                "request_id": id,
            });
            let _ = self.roundtrip(vec![payload.to_string()], 1, QUERY_TIMEOUT);
            self.state = PlaybackState::Paused;
        }
        self.last_pos_time = Instant::now();
    }

    fn ensure_mpv(&mut self) -> bool {
        if self.child_alive() {
            if self.ipc_ping() {
                return true;
            }
            // Process alive but IPC dead: restart it.
            if let Some(mut c) = self.child.take() {
                let _ = c.kill();
            }
        }
        self.spawn()
    }

    fn poll_sync_internal(&mut self) {
        // Supervisor: respawn dead mpv, resume where we were.
        if !self.child_alive() {
            let had_media = !self.uri.is_empty();
            let was_playing =
                had_media && matches!(self.state, PlaybackState::Playing | PlaybackState::Paused | PlaybackState::Transitioning);
            let resume_pos = self.position;
            let was_paused = self.state == PlaybackState::Paused;
            if self.spawn() {
                if was_playing {
                    tracing::info!("mpv respawned, resuming {}", self.uri);
                    self.restore_after_respawn(resume_pos, was_paused);
                } else {
                    self.state = PlaybackState::Stopped;
                }
            } else if was_playing {
                // Still retrying: show buffering, not stopped.
                self.state = PlaybackState::Transitioning;
                if self.transition_start.is_none() {
                    self.transition_start = Some(Instant::now());
                }
            }
            return;
        }

        let snap = match self.query_props() {
            Some(s) => {
                self.failures = 0;
                s
            }
            None => {
                self.failures += 1;
                if self.failures >= 3 {
                    tracing::warn!("mpv IPC failing, restarting mpv");
                    if let Some(mut c) = self.child.take() {
                        let _ = c.kill();
                    }
                    self.child = None;
                    self.failures = 0;
                    self.next_retry = Instant::now() + RETRY_DELAY;
                }
                return;
            }
        };

        if let Some(v) = snap.volume {
            self.volume = v.clamp(0.0, 100.0) as u8;
        }
        if let Some(m) = snap.mute {
            self.mute = m;
        }

        // Natural end of file: mpv goes idle after playing the item.
        if snap.eof == Some(true) || (snap.idle == Some(true) && self.loaded_uri.is_some()) {
            if self.state != PlaybackState::Stopped {
                tracing::info!("mpv reached end of file");
            }
            self.state = PlaybackState::Stopped;
            self.position = Duration::ZERO;
            self.loaded_uri = None;
            self.transition_start = None;
            self.last_pos_time = Instant::now();
            return;
        }
        if snap.idle == Some(true) {
            if self.state != PlaybackState::Stopped {
                self.state = PlaybackState::Stopped;
                self.last_pos_time = Instant::now();
            }
            self.transition_start = None;
            return;
        }

        if let Some(d) = snap.duration {
            if d > Duration::ZERO {
                self.duration = d;
            }
        }
        if let Some(t) = snap.time_pos {
            self.position = t;
            self.last_pos_time = Instant::now();
        }

        let next = match (snap.pause, self.loaded_uri.is_some()) {
            (Some(true), true) => PlaybackState::Paused,
            (Some(false), true) => PlaybackState::Playing,
            // Fresh loadfile still opening (no clock yet): buffering.
            (_, false) if !self.uri.is_empty() => PlaybackState::Transitioning,
            (None, _) if self.position == Duration::ZERO && self.duration == Duration::ZERO => {
                if self.transition_timed_out() {
                    PlaybackState::Stopped
                } else {
                    PlaybackState::Transitioning
                }
            }
            (None, _) => self.state,
            (_, _) => self.state,
        };
        if next != self.state {
            // First clock sample ends the transition.
            if next == PlaybackState::Playing {
                self.transition_start = None;
            }
            self.state = next;
        }
        if self.transition_timed_out() && self.state == PlaybackState::Transitioning {
            tracing::warn!("mpv load timed out, stopping");
            self.state = PlaybackState::Stopped;
            self.transition_start = None;
        }
    }

    fn transition_timed_out(&self) -> bool {
        self.transition_start
            .map(|t| t.elapsed() > TRANSITION_TIMEOUT)
            .unwrap_or(false)
    }
}

#[derive(Debug, Default)]
struct MpvSnapshot {
    time_pos: Option<Duration>,
    duration: Option<Duration>,
    pause: Option<bool>,
    idle: Option<bool>,
    eof: Option<bool>,
    volume: Option<f64>,
    mute: Option<bool>,
    seekable: Option<bool>,
}

fn parse_snapshot(lines: &[String], ids: &[u64; 8]) -> MpvSnapshot {
    let mut snap = MpvSnapshot::default();
    for line in lines {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let rid = v.get("request_id").and_then(|r| r.as_u64()).unwrap_or(0);
        let data = &v["data"];
        if rid == ids[0] {
            snap.time_pos = as_duration(data);
        } else if rid == ids[1] {
            snap.duration = as_duration(data);
        } else if rid == ids[2] {
            snap.pause = data.as_bool();
        } else if rid == ids[3] {
            snap.idle = data.as_bool();
        } else if rid == ids[4] {
            snap.eof = data.as_bool();
        } else if rid == ids[5] {
            snap.volume = data.as_f64();
        } else if rid == ids[6] {
            snap.mute = data.as_bool();
        } else if rid == ids[7] {
            snap.seekable = data.as_bool();
        }
    }
    snap
}

fn as_duration(v: &serde_json::Value) -> Option<Duration> {
    let secs = v.as_f64()?;
    if !secs.is_finite() || secs < 0.0 {
        return None;
    }
    Some(Duration::from_secs_f64(secs))
}

// True when some reply line is the success response to our request id.
// (Matched structurally, not by substring: property payloads can contain
// any digits, e.g. version strings.)
fn reply_ok(lines: &[String], id: u64) -> bool {
    lines.iter().any(|line| {
        serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .map_or(false, |v| {
                v.get("request_id").and_then(|r| r.as_u64()) == Some(id)
                    && v.get("error").and_then(|e| e.as_str()) == Some("success")
            })
    })
}

impl MediaPlayer for MpvPlayer {
    fn set_uri(&mut self, uri: String, title: String) {
        if self.uri != uri && self.loaded_uri.is_some() {
            let id = self.next_id();
            let payload = serde_json::json!({"command": ["stop"], "request_id": id});
            let _ = self.roundtrip(vec![payload.to_string()], 1, QUERY_TIMEOUT);
        }
        self.uri = uri;
        self.title = title;
        self.loaded_uri = None;
        self.position = Duration::ZERO;
        self.duration = Duration::ZERO;
        self.state = PlaybackState::Stopped;
        self.transition_start = None;
        self.last_pos_time = Instant::now();
    }

    fn play(&mut self) -> anyhow::Result<()> {
        if self.uri.is_empty() {
            anyhow::bail!("No URI set");
        }
        if !self.ensure_mpv() {
            anyhow::bail!("mpv is not available");
        }
        if self.state == PlaybackState::Paused && self.loaded_uri.as_deref() == Some(self.uri.as_str()) {
            let id = self.next_id();
            let payload = serde_json::json!({"command": ["set_property", "pause", false], "request_id": id});
            let _ = self.roundtrip(vec![payload.to_string()], 1, QUERY_TIMEOUT);
            self.state = PlaybackState::Playing;
            self.last_pos_time = Instant::now();
            return Ok(());
        }
        if matches!(self.state, PlaybackState::Playing | PlaybackState::Transitioning)
            && self.loaded_uri.as_deref() == Some(self.uri.as_str())
        {
            return Ok(());
        }
        self.load_current();
        Ok(())
    }

    fn pause(&mut self) -> anyhow::Result<()> {
        if matches!(self.state, PlaybackState::Playing | PlaybackState::Transitioning) {
            let id = self.next_id();
            let payload = serde_json::json!({"command": ["set_property", "pause", true], "request_id": id});
            let _ = self.roundtrip(vec![payload.to_string()], 1, QUERY_TIMEOUT);
            self.state = PlaybackState::Paused;
            self.last_pos_time = Instant::now();
        }
        Ok(())
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        let id = self.next_id();
        let payload = serde_json::json!({"command": ["stop"], "request_id": id});
        let _ = self.roundtrip(vec![payload.to_string()], 1, QUERY_TIMEOUT);
        self.state = PlaybackState::Stopped;
        self.position = Duration::ZERO;
        self.loaded_uri = None;
        self.transition_start = None;
        self.last_pos_time = Instant::now();
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
        let id = self.next_id();
        let payload = serde_json::json!({
            "command": ["seek", target.as_secs_f64(), "absolute"],
            "request_id": id,
        });
        let _ = self.roundtrip(vec![payload.to_string()], 1, QUERY_TIMEOUT);
        self.position = target;
        self.last_pos_time = Instant::now();
        Ok(())
    }

    fn set_volume(&mut self, vol: u8) -> anyhow::Result<()> {
        self.volume = vol.min(100);
        let id = self.next_id();
        let payload = serde_json::json!({"command": ["set_property", "volume", self.volume], "request_id": id});
        let _ = self.roundtrip(vec![payload.to_string()], 1, QUERY_TIMEOUT);
        Ok(())
    }

    fn set_mute(&mut self, mute: bool) -> anyhow::Result<()> {
        self.mute = mute;
        let id = self.next_id();
        let payload = serde_json::json!({"command": ["set_property", "mute", mute], "request_id": id});
        let _ = self.roundtrip(vec![payload.to_string()], 1, QUERY_TIMEOUT);
        Ok(())
    }

    fn get_position(&self) -> (Duration, Duration) {
        if self.state == PlaybackState::Playing {
            let extra = self.last_pos_time.elapsed().min(Duration::from_millis(500));
            let estimated = self.position + extra;
            if self.duration > Duration::ZERO && estimated > self.duration {
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

    fn get_volume_mute(&self) -> Option<(u8, bool)> {
        Some((self.volume, self.mute))
    }

    fn poll_sync(&mut self) {
        self.poll_sync_internal();
    }
}

impl Drop for MpvPlayer {
    fn drop(&mut self) {
        // Daemon shutdown: ask our mpv to quit, then kill it so no orphan
        // keeps playing audio after playpnp exits.
        if self.child.is_some() {
            let id = self.request_id;
            let payload = serde_json::json!({"command": ["quit"], "request_id": id});
            let endpoint = self.ipc_endpoint.clone();
            if let Some(mut conn) = Conn::open(&endpoint) {
                let _ = conn.write_line(&payload.to_string());
            }
            if let Some(ref mut child) = self.child {
                let _ = child.kill();
            }
        }
    }
}

// Cross-platform IPC connection: Windows named pipe, Unix socket.
enum Conn {
    #[cfg(windows)]
    Pipe(BufReader<std::fs::File>),
    #[cfg(not(windows))]
    Sock(BufReader<std::os::unix::net::UnixStream>),
}

impl Conn {
    fn open(endpoint: &str) -> Option<Self> {
        #[cfg(windows)]
        {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(endpoint)
                .ok()?;
            Some(Conn::Pipe(BufReader::new(file)))
        }
        #[cfg(not(windows))]
        {
            let stream = std::os::unix::net::UnixStream::connect(endpoint).ok()?;
            let _ = stream.set_read_timeout(Some(QUERY_TIMEOUT));
            let _ = stream.set_write_timeout(Some(QUERY_TIMEOUT));
            Some(Conn::Sock(BufReader::new(stream)))
        }
    }

    fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        match self {
            #[cfg(windows)]
            Conn::Pipe(r) => {
                let msg = format!("{}\n", line);
                r.get_mut().write_all(msg.as_bytes())?;
                r.get_mut().flush()
            }
            #[cfg(not(windows))]
            Conn::Sock(r) => {
                let msg = format!("{}\n", line);
                r.get_mut().write_all(msg.as_bytes())?;
                r.get_mut().flush()
            }
        }
    }

    // Read reply lines with a hard timeout. Each command gets exactly one
    // reply (we register no observe_property subscriptions, so there are no
    // unsolicited events); stop after `expect` lines so single-command
    // queries don't wait for more. Windows File has no timeout, so the read
    // runs on a helper thread and the caller stops waiting.
    fn read_lines_timeout(&mut self, expect: usize, timeout: Duration) -> Option<Vec<String>> {
        match self {
            #[cfg(not(windows))]
            Conn::Sock(r) => {
                let mut out = Vec::new();
                let mut line = String::new();
                for _ in 0..(expect + 4) {
                    line.clear();
                    match r.read_line(&mut line) {
                        Ok(0) => break,
                        Ok(_) => {
                            if !line.trim().is_empty() {
                                out.push(line.trim().to_string());
                            }
                            if out.len() >= expect.max(1) {
                                break;
                            }
                        }
                        Err(e)
                            if e.kind() == std::io::ErrorKind::TimedOut
                                || e.kind() == std::io::ErrorKind::WouldBlock =>
                        {
                            break;
                        }
                        Err(_) => return None,
                    }
                }
                Some(out)
            }
            #[cfg(windows)]
            Conn::Pipe(r) => {
                let (tx, rx) = mpsc::channel();
                // The reader owns a cloned handle; the original is dropped
                // with this connection, so a slow mpv can't block us past
                // the timeout. The thread exits on its own once a line or
                // EOF arrives (server death closes the pipe).
                let mut file = r.get_mut().try_clone().ok()?;
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(&mut file);
                    let mut out = Vec::new();
                    let mut line = String::new();
                    for _ in 0..(expect + 4) {
                        line.clear();
                        match reader.read_line(&mut line) {
                            Ok(0) => break,
                            Ok(_) => {
                                if !line.trim().is_empty() {
                                    out.push(line.trim().to_string());
                                }
                                if out.len() >= expect.max(1) {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let _ = tx.send(out);
                });
                rx.recv_timeout(timeout).ok()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_snapshot_replies() {
        let lines = vec![
            r#"{"request_id":1,"error":"success","data":12.34}"#.to_string(),
            r#"{"request_id":2,"error":"success","data":3600.0}"#.to_string(),
            r#"{"request_id":3,"error":"success","data":false}"#.to_string(),
            r#"{"request_id":4,"error":"success","data":false}"#.to_string(),
            r#"{"request_id":5,"error":"success","data":false}"#.to_string(),
            r#"{"request_id":6,"error":"success","data":100.0}"#.to_string(),
            r#"{"request_id":7,"error":"success","data":false}"#.to_string(),
            r#"{"request_id":8,"error":"success","data":true}"#.to_string(),
        ];
        let snap = parse_snapshot(&lines, &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(snap.time_pos, Some(Duration::from_millis(12340)));
        assert_eq!(snap.duration, Some(Duration::from_secs(3600)));
        assert_eq!(snap.pause, Some(false));
        assert_eq!(snap.volume, Some(100.0));
        assert_eq!(snap.seekable, Some(true));
    }

    #[test]
    fn null_clock_means_no_sample() {
        let lines = vec![
            r#"{"request_id":1,"error":"success","data":null}"#.to_string(),
            r#"{"request_id":2,"error":"success","data":null}"#.to_string(),
            r#"{"request_id":3,"error":"success","data":true}"#.to_string(),
            r#"{"request_id":4,"error":"success","data":true}"#.to_string(),
            r#"{"request_id":5,"error":"success","data":false}"#.to_string(),
            r#"{"request_id":6,"error":"success","data":80.0}"#.to_string(),
            r#"{"request_id":7,"error":"success","data":false}"#.to_string(),
            r#"{"request_id":8,"error":"success","data":false}"#.to_string(),
        ];
        let snap = parse_snapshot(&lines, &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(snap.time_pos, None);
        assert_eq!(snap.idle, Some(true));
    }

    // Live drive test against real mpv. Needs mpv installed + sample file.
    // Run: MPV_SAMPLE=C:\...\sample.mp4 cargo test -- --nocapture --ignored live
    #[test]
    #[ignore]
    fn live_drive_play_pause_seek_reconnect() {
        let sample = std::env::var("MPV_SAMPLE").expect("set MPV_SAMPLE to a media file");
        let mut p = MpvPlayer::new();
        p.set_uri(sample.clone(), "sample".to_string());
        p.play().expect("play");
        for _ in 0..20 {
            p.poll_sync();
            let (_, dur) = p.get_position();
            if p.get_state() == PlaybackState::Playing && dur > Duration::from_secs(5) {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        assert_eq!(p.get_state(), PlaybackState::Playing, "mpv should be playing");
        let (pos1, dur) = p.get_position();
        println!("playing at {:?} / {:?}", pos1, dur);
        assert!(dur > Duration::from_secs(5), "duration should be known");

        p.pause().expect("pause");
        p.poll_sync();
        assert_eq!(p.get_state(), PlaybackState::Paused);

        p.seek(Duration::from_secs(10)).expect("seek");
        p.poll_sync();
        let (pos2, _) = p.get_position();
        println!("after seek: {:?}", pos2);
        assert!(pos2 >= Duration::from_secs(8), "seek should jump forward");

        p.play().expect("resume");
        p.poll_sync();
        assert_eq!(p.get_state(), PlaybackState::Playing);

        // Kill mpv mid-play; supervisor must respawn and resume position.
        let killed_pos = p.get_position().0;
        if let Some(ref mut child) = p.child {
            let _ = child.kill();
        }
        std::thread::sleep(Duration::from_millis(500));
        for _ in 0..20 {
            p.poll_sync();
            if p.get_state() == PlaybackState::Playing {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        assert_eq!(p.get_state(), PlaybackState::Playing, "should resume after kill");
        let (pos3, _) = p.get_position();
        println!("resumed at {:?} (was {:?})", pos3, killed_pos);
        assert!(
            pos3 + Duration::from_secs(5) >= killed_pos,
            "resume should be near kill position"
        );

        p.stop().expect("stop");
        p.poll_sync();
        assert_eq!(p.get_state(), PlaybackState::Stopped);
        println!("LIVE DRIVE OK");
    }
}
