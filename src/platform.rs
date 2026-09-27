//! OS-specific bits, kept in one small module so the rest stays generic.
//!
//! Windows behavior is unchanged; Unix branches add XDG paths,
//! `ip route` gateway detection, `xdg-open`, and VLC args that
//! work with VLC 3.x on Arch/Ubuntu.

use std::net::Ipv4Addr;
use std::path::PathBuf;

/// Base dir for `uuid` and `peers.txt`.
pub fn config_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            if !appdata.is_empty() {
                return PathBuf::from(appdata).join("playpnp");
            }
        }
        PathBuf::from(".").join("playpnp")
    }
    #[cfg(not(windows))]
    {
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
            && !xdg.is_empty()
        {
            return PathBuf::from(xdg).join("playpnp");
        }
        if let Ok(home) = std::env::var("HOME")
            && !home.is_empty()
        {
            return PathBuf::from(home).join(".config").join("playpnp");
        }
        PathBuf::from(".").join("playpnp")
    }
}

/// Machine name for the DLNA `friendlyName`.
pub fn hostname() -> String {
    if let Ok(h) = std::env::var("COMPUTERNAME")
        && !h.is_empty()
    {
        return h;
    }
    if let Ok(h) = std::env::var("HOSTNAME")
        && !h.is_empty()
    {
        return h;
    }
    // `sysinfo` is already a dependency; works when `HOSTNAME` is unset
    // and the `hostname(1)` binary is missing (minimal Arch installs).
    if let Some(sys) = sysinfo::System::host_name()
        && !sys.is_empty()
    {
        return sys;
    }
    // Last resort: /etc/hostname (Linux) — cheap sync read.
    #[cfg(unix)]
    if let Ok(name) = std::fs::read_to_string("/etc/hostname")
        && !name.trim().is_empty()
    {
        return name.trim().to_string();
    }
    "PC".to_string()
}

/// Default gateway for hotspot unicast NOTIFY.
pub fn default_gateway() -> Option<Ipv4Addr> {
    #[cfg(windows)]
    {
        windows_gateway()
    }
    #[cfg(not(windows))]
    {
        unix_gateway().or(windows_gateway())
    }
}

#[cfg(windows)]
fn windows_gateway() -> Option<Ipv4Addr> {
    parse_windows_route()
}

#[cfg(not(windows))]
fn windows_gateway() -> Option<Ipv4Addr> {
    // `route print` also exists on some Unix boxes; harmless fallback.
    parse_windows_route()
}

fn parse_windows_route() -> Option<Ipv4Addr> {
    let output = std::process::Command::new("route")
        .args(["print", "0.0.0.0"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        // 0.0.0.0  0.0.0.0  <gateway>  <interface>  <metric>
        if parts.len() >= 4
            && parts[0] == "0.0.0.0"
            && parts[1] == "0.0.0.0"
            && let Ok(gw) = parts[2].parse::<Ipv4Addr>()
            && !gw.is_unspecified()
            && !gw.is_loopback()
        {
            return Some(gw);
        }
    }
    None
}

#[cfg(not(windows))]
fn unix_gateway() -> Option<Ipv4Addr> {
    // Primary: `ip route show default` -> "default via 192.168.1.1 dev wlo1 ..."
    if let Ok(output) = std::process::Command::new("ip")
        .args(["route", "show", "default"])
        .output()
        && output.status.success()
    {
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            for (i, p) in parts.iter().enumerate() {
                if *p == "via"
                    && i + 1 < parts.len()
                    && let Ok(gw) = parts[i + 1].parse::<Ipv4Addr>()
                    && !gw.is_unspecified()
                    && !gw.is_loopback()
                {
                    return Some(gw);
                }
            }
        }
    }
    // Fallback: /proc/net/route — line starting with "00000000" (default),
    // gateway is 2nd column, little-endian hex.
    if let Ok(content) = std::fs::read_to_string("/proc/net/route") {
        for line in content.lines().skip(1) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3
                && parts[1] == "00000000"
                && let Ok(raw) = u32::from_str_radix(parts[2], 16)
            {
                let gw = Ipv4Addr::from(raw.to_le_bytes());
                if !gw.is_unspecified() && !gw.is_loopback() {
                    return Some(gw);
                }
            }
        }
    }
    None
}

/// Virtual / container interfaces to skip for SSDP + primary-IP selection.
/// Shared across OSes so behavior stays consistent.
pub fn is_virtual_iface(name_lower: &str) -> bool {
    const PATTERNS: &[&str] = &[
        "wsl",
        "hyper-v",
        "hyperv",
        "vmware",
        "virtualbox",
        "vbox",
        "docker",
        "veth",
        "virbr",
        "vibr",
        "lxc",
        "lxdbr",
        "br-",
        "wg",
        "tun",
    ];
    PATTERNS.iter().any(|p| name_lower.contains(p))
}

/// Open a URL in the default browser / handler.
pub fn open_url(url: &str) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}

/// Candidate VLC binaries (after `$PATH` lookup).
pub fn vlc_candidates() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        vec![
            PathBuf::from(r"C:\Program Files\VideoLAN\VLC\vlc.exe"),
            PathBuf::from(r"C:\Program Files (x86)\VideoLAN\VLC\vlc.exe"),
            PathBuf::from(r"C:\vlc\vlc.exe"),
        ]
    }
    #[cfg(not(windows))]
    {
        vec![
            PathBuf::from("/usr/bin/vlc"),
            PathBuf::from("/usr/local/bin/vlc"),
            PathBuf::from("/snap/bin/vlc"),
            PathBuf::from("/var/lib/flatpak/exports/bin/org.videolan.VLC"),
        ]
    }
}

/// Candidate mpv binaries (after `$PATH` lookup).
pub fn mpv_candidates() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        vec![
            PathBuf::from(r"C:\Program Files\MPV Player\mpv.exe"),
            PathBuf::from(r"C:\Program Files\mpv\mpv.exe"),
        ]
    }
    #[cfg(not(windows))]
    {
        vec![
            PathBuf::from("/usr/bin/mpv"),
            PathBuf::from("/usr/local/bin/mpv"),
            PathBuf::from("/var/lib/flatpak/exports/bin/io.mpv.Mpv"),
        ]
    }
}

/// IPC endpoint mpv listens on. Unique per daemon process so a stale
/// mpv from a previous run can never block a new one from binding.
/// Windows uses a named pipe, Unix a socket file.
pub fn mpv_ipc_endpoint() -> String {
    let pid = std::process::id();
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\playpnp-mpv-{}", pid)
    }
    #[cfg(not(windows))]
    {
        let uid = unsafe { libc::getuid() };
        format!("/tmp/playpnp-mpv-{}-{}.sock", uid, pid)
    }
}

/// mpv launch args for renderer use: stay idle with no window until a
/// file is loaded, then fullscreen. English audio/subs preferred natively.
pub fn mpv_args(ipc_endpoint: &str) -> Vec<String> {
    vec![
        "--idle=yes".to_string(),
        "--force-window=no".to_string(),
        "--fullscreen".to_string(),
        "--no-terminal".to_string(),
        format!("--input-ipc-server={}", ipc_endpoint),
        "--slang=en,eng,English".to_string(),
        "--alang=en,eng,English".to_string(),
        "--sub-auto=all".to_string(),
        // Like VLC's --no-video-title-show: no filename flash on the
        // screen when a new file starts. Empty string disables it.
        "--osd-playing-msg=".to_string(),
    ]
}

/// VLC launch args for the RC interface. `intf` is `rc` normally,
/// `oldrc` as fallback on Linux VLC 3.x where the module kept its old name.
pub fn vlc_args(rc_port: u16, intf: &str) -> Vec<String> {
    #[allow(unused_mut)]
    let mut args = vec![
        "--extraintf".to_string(),
        intf.to_string(),
        "--rc-host".to_string(),
        format!("127.0.0.1:{}", rc_port),
        "--no-video-title-show".to_string(),
        // User choice: always start fullscreen.
        "--fullscreen".to_string(),
        "--audio-language=en,eng,English".to_string(),
        "--network-caching=5000".to_string(),
        "--file-caching=3000".to_string(),
        "--live-caching=3000".to_string(),
        "--http-reconnect".to_string(),
        "--clock-jitter=5000".to_string(),
        // Renderer use: never pop up the "Your input can't be opened"
        // Errors dialog (e.g. when the phone serving the file sleeps and
        // its HTTP server goes away). `qt-error-dialogs` defaults to true
        // (modules/gui/qt/qt.cpp); the dialog's own "Hide future errors"
        // checkbox flips the same setting.
        "--no-qt-error-dialogs".to_string(),
    ];
    // `--rc-quiet` exists on Windows builds but makes Linux VLC 3.0.x
    // abort with "unknown option". Same for the D3D11 hint.
    #[cfg(windows)]
    {
        args.push("--rc-quiet".to_string());
        args.push("--avcodec-hw=any".to_string());
    }
    args
}

/// Bring the VLC window to the foreground.
/// Best-effort, never fails the build or playback — logs only on debug.
pub fn focus_vlc(pid: Option<u32>) {
    #[cfg(windows)]
    {
        windows_focus_vlc(pid);
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        unix_focus_vlc();
    }
}

#[cfg(windows)]
fn windows_focus_vlc(pid: Option<u32>) {
    // Raw user32 FFI — no extra deps, works with default features.
    #[link(name = "user32")]
    unsafe extern "system" {
        fn EnumWindows(cb: unsafe extern "system" fn(u64, u64) -> i32, param: u64) -> i32;
        fn GetWindowThreadProcessId(hwnd: u64, pid_out: *mut u32) -> u32;
        fn IsWindowVisible(hwnd: u64) -> i32;
        fn ShowWindow(hwnd: u64, cmd: i32) -> i32;
        fn SetForegroundWindow(hwnd: u64) -> i32;
        fn BringWindowToTop(hwnd: u64) -> i32;
        fn GetWindowTextW(hwnd: u64, buf: *mut u16, max: i32) -> i32;
    }
    const SW_RESTORE: i32 = 9;

    struct Ctx {
        pid: Option<u32>,
        found: Vec<u64>,
    }

    unsafe extern "system" fn enum_cb(hwnd: u64, param: u64) -> i32 {
        let ctx = unsafe { &mut *(param as *mut Ctx) };
        unsafe {
            if IsWindowVisible(hwnd) == 0 {
                return 1;
            }
            let mut wpid: u32 = 0;
            GetWindowThreadProcessId(hwnd, &mut wpid as *mut u32);
            let matches = match ctx.pid {
                Some(p) => wpid == p,
                None => {
                    // Fallback: match VLC window title when pid unknown.
                    let mut buf = [0u16; 256];
                    let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), 256);
                    if n <= 0 {
                        return 1;
                    }
                    let title = String::from_utf16_lossy(&buf[..n as usize]);
                    let t = title.to_ascii_lowercase();
                    t.contains("vlc")
                }
            };
            if matches {
                ctx.found.push(hwnd);
            }
        }
        1
    }

    let mut ctx = Ctx {
        pid,
        found: Vec::new(),
    };
    unsafe {
        EnumWindows(enum_cb, &mut ctx as *mut Ctx as u64);
        for hwnd in ctx.found {
            ShowWindow(hwnd, SW_RESTORE);
            BringWindowToTop(hwnd);
            SetForegroundWindow(hwnd);
        }
    }
}

#[cfg(not(windows))]
fn unix_focus_vlc() {
    // Wayland/X11: try whatever raiser is installed. All best-effort.
    for (prog, args) in [
        ("xdotool", vec!["search", "--onlyvisible", "--class", "vlc", "windowactivate", "windowraise"]),
        ("wmctrl", vec!["-a", "VLC"]),
    ] {
        let _ = std::process::Command::new(prog)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}
pub fn server_header() -> &'static str {
    #[cfg(windows)]
    {
        "Windows/10.0 UPnP/1.0 playpnp/1.0"
    }
    #[cfg(not(windows))]
    {
        "Linux UPnP/1.0 playpnp/1.0"
    }
}

/// Where `peers.txt` lives, for help text.
pub fn peers_hint() -> &'static str {
    #[cfg(windows)]
    {
        "%APPDATA%\\playpnp\\peers.txt"
    }
    #[cfg(not(windows))]
    {
        "~/.config/playpnp/peers.txt"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_iface_filter_covers_linux_names() {
        for name in [
            "docker0", "vethabc", "virbr0", "br-1234", "wg0", "tun0", "lxcbr0",
        ] {
            assert!(is_virtual_iface(name), "{}", name);
        }
        for name in ["wlo1", "wlan0", "eth0", "enp3s0", "tailscale0"] {
            assert!(!is_virtual_iface(name), "{}", name);
        }
    }

    #[test]
    fn vlc_args_differ_per_os() {
        let args = vlc_args(52422, "rc");
        assert!(args.contains(&"--fullscreen".to_string()));
        // Error popup must stay suppressed on every OS (phone-sleep disconnects).
        assert!(args.contains(&"--no-qt-error-dialogs".to_string()));
        #[cfg(windows)]
        assert!(args.contains(&"--rc-quiet".to_string()));
        #[cfg(not(windows))]
        assert!(!args.contains(&"--rc-quiet".to_string()));
    }
}
