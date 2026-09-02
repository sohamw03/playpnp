use std::sync::{Arc, Mutex};
use std::time::Duration;

pub mod external;
pub mod mock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackState {
    Stopped,
    Playing,
    Paused,
    #[allow(dead_code)]
    Transitioning,
}

pub trait MediaPlayer: Send {
    fn set_uri(&mut self, uri: String, title: String);
    fn play(&mut self) -> anyhow::Result<()>;
    fn pause(&mut self) -> anyhow::Result<()>;
    fn stop(&mut self) -> anyhow::Result<()>;
    fn seek(&mut self, target: Duration) -> anyhow::Result<()>;
    fn set_volume(&mut self, vol: u8) -> anyhow::Result<()>;
    fn set_mute(&mut self, mute: bool) -> anyhow::Result<()>;
    fn get_position(&self) -> (Duration, Duration);
    fn get_state(&self) -> PlaybackState;
    #[allow(dead_code)]
    fn current_uri(&self) -> String;
    fn poll_sync(&mut self) {}
}

pub fn create_player() -> Arc<Mutex<Box<dyn MediaPlayer>>> {
    tracing::info!("Using VLC player backend (VLC is required for playback)");
    Arc::new(Mutex::new(Box::new(external::VlcPlayer::new())))
}

// Helper to detect VLC installation
pub fn find_vlc() -> Option<std::path::PathBuf> {
    // Check PATH
    if let Ok(path) = which::which("vlc") {
        return Some(path);
    }
    // Common Windows install paths
    let candidates = [
        r"C:\Program Files\VideoLAN\VLC\vlc.exe",
        r"C:\Program Files (x86)\VideoLAN\VLC\vlc.exe",
        r"C:\vlc\vlc.exe",
    ];
    for c in candidates {
        let p = std::path::PathBuf::from(c);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

// Simple which implementation without extra crate
mod which {
    use std::path::PathBuf;
    pub fn which(name: &str) -> Result<PathBuf, ()> {
        if let Ok(path_var) = std::env::var("PATH") {
            for dir in std::env::split_paths(&path_var) {
                let candidate = dir.join(format!("{}.exe", name));
                if candidate.exists() {
                    return Ok(candidate);
                }
                let candidate2 = dir.join(name);
                if candidate2.exists() {
                    return Ok(candidate2);
                }
            }
        }
        Err(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpStream;
    use std::process::Command;

    #[test]
    fn test_vlc_rc() {
        let vlc_path = match find_vlc() {
            Some(p) => p,
            None => {
                println!("VLC not found");
                return;
            }
        };
        let port = 52425;
        let mut child = Command::new(&vlc_path)
            .args([
                "--extraintf",
                "rc",
                "--rc-host",
                &format!("127.0.0.1:{}", port),
                "--rc-quiet",
                "--no-video-title-show",
            ])
            .spawn()
            .expect("spawn VLC");

        std::thread::sleep(std::time::Duration::from_millis(800));

        let stream = match TcpStream::connect(format!("127.0.0.1:{}", port)) {
            Ok(s) => s,
            Err(e) => {
                println!("Connect error: {}", e);
                let _ = child.kill();
                return;
            }
        };
        stream
            .set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;

        let _ = writer.write_all(b"status\n");
        let _ = writer.flush();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let mut line = String::new();
        while let Ok(n) = reader.read_line(&mut line) {
            if n == 0 {
                break;
            }
            print!("VLC status output: {}", line);
            line.clear();
        }

        let _ = writer.write_all(b"quit\n");
        let _ = writer.flush();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let _ = child.kill();
    }
}
