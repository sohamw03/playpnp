//! Single-instance guard: file lock (startup race) + TCP bind (truth).
//!
//! File lock covers the window before `:52411` is bound; the TCP bind in
//! `run_daemon_with_channels` is authoritative. Guard must live as long
//! as the daemon. Dropping (or crashing) releases the OS lock.

use fs2::FileExt;

pub struct SingleInstanceGuard {
    _file: std::fs::File,
}

impl SingleInstanceGuard {
    /// `Ok(Some)` = owner. `Ok(None)` = another instance holds it.
    pub fn acquire() -> anyhow::Result<Option<Self>> {
        let dir = crate::platform::config_dir();
        std::fs::create_dir_all(&dir)
            .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", dir.display()))?;
        let path = dir.join("playpnp.lock");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)
            .map_err(|e| anyhow::anyhow!("cannot open {}: {e}", path.display()))?;

        match file.try_lock_exclusive() {
            Ok(()) => {
                use std::io::Write;
                let mut f = &file;
                let _ = writeln!(f, "pid={}", std::process::id());
                Ok(Some(Self { _file: file }))
            }
            // fs2 maps lock contention to WouldBlock on Unix + Windows.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(anyhow::anyhow!("cannot lock {}: {e}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_reports_already_running() {
        let first = SingleInstanceGuard::acquire().expect("no hard error");
        let second = SingleInstanceGuard::acquire().expect("no hard error");
        if first.is_some() {
            assert!(second.is_none(), "second acquire must return None");
        }
    }
}
