# playpnp — cast videos from your phone to your PC

`playpnp` turns your computer into a **DLNA MediaRenderer** (DMR) that shows up in **BubbleUPnP**, **mConnect**, and any UPnP/DLNA control point. Pick a video on your phone, hit cast — it plays on your PC in **real VLC**, with position tracking, seeking, volume, and pause/resume all in sync.

Windows and Linux (Arch, Ubuntu). Single Rust binary — just add VLC. (On Linux the tray build uses system GTK/AppIndicator libs; the `--no-default-features` build is dependency-free.)

---

## Why playpnp?

- **Real VLC playback, not a fake player.** Most lightweight renderers are audio-only (`gmrender-resurrect`, `upmpdcli`) or full media centers (Kodi, JRiver). playpnp drives the VLC you already have through its remote-control interface: video, audio, fullscreen.
- **Bidirectional state sync.** VLC is treated as authoritative — scrubbing, pausing, or closing the VLC window itself is reflected back to the controller via standard `LastChange` events. Position polling adapts (500 ms while playing, 2.5 s when idle) and never races ahead on stalled Wi-Fi.
- **Actually works on phone hotspots.** Android blocks SSDP multicast on hotspots. playpnp detects the gateway (your phone) and sends directed unicast + subnet broadcast so discovery is instant.
- **One binary, tray or headless.** Runs as a tray app (Windows, StatusNotifier bar widgets on Wayland) and degrades gracefully to headless where there's no tray. `serve` + the included systemd unit covers servers.
- **Helpful extras:** browser dashboard with remote controls, English/SDH subtitle and English audio auto-selection, dynamic `URLBase` so control URLs are always reachable on multi-homed machines, `playpnp diag` for network troubleshooting.

---

## 🎯 Connectivity

| Scenario | Status | How |
| :--- | :--- | :--- |
| **Common Wi-Fi** (phone + PC on one LAN) | ✅ Works | Multicast `239.255.255.250:1900` + subnet broadcast per interface. VirtualBox, WSL, Docker, link-local and virtual NICs filtered out automatically |
| **Mobile hotspot** (PC on phone's hotspot) | ✅ Works | Android blocks multicast, so playpnp unicasts NOTIFY to the gateway (the phone, e.g. `192.168.43.1:1900`) + subnet broadcast |
| **Tailscale / VPN** | ⚠️ Known limitation, see below | Unicast NOTIFY to peers is implemented (`tailscale status`, `PLAYPNP_PEERS`, `peers.txt`), but discovery over Tailscale does not complete in practice |

> ⚠️ **Tailscale doesn't work.** The unicast machinery is there and `diag` shows your peers, but phones behind Tailscale never discover the renderer — cause unknown, looks non-fixable from our side. Common Wi-Fi and mobile hotspot are the supported paths. If you crack it, PRs welcome.

---

## ✨ Features

- **Real VLC backend** — Play, Pause, Stop, Seek, Volume/Mute over VLC's RC interface (`127.0.0.1:52422`). Reuses one VLC instance across tracks, auto-closes it when idle. VLC is required.
- **Synced timeline** — duration from DIDL metadata or VLC probing, sub-second interpolation capped so weak Wi-Fi never makes the timeline jump.
- **Auto tracks** — prefers SDH English subtitles, then English; same for audio. Leaves your settings alone when there's no match.
- **Dynamic `URLBase`** — `description.xml` answers from the request's `Host`, so multi-NIC machines always hand out reachable control/event URLs.
- **Dashboard** — `http://<pc-ip>:<port>/` shows track, position, volume, endpoints, peers, and Play/Pause/Stop buttons.
- **Tray + CLI** — tray menu (Show Status / Stop / Quit), plus `playpnp status` and `playpnp stop` over a local control port.

---

## 🚀 Quick Start

Prerequisites: install **VLC** (`https://www.videolan.org` on Windows, `vlc` package on Linux).

### Windows

```powershell
cargo build --release

# Background with tray icon
.\target\release\playpnp.exe

# Or headless in the terminal
.\target\release\playpnp.exe serve

.\target\release\playpnp.exe diag    # network + VLC diagnostics
.\target\release\playpnp.exe status  # is it running?
.\target\release\playpnp.exe stop    # clean shutdown
```

### Linux (Arch / Ubuntu)

```bash
# Arch (paru works too; xdotool provides libxdo.so for the tray link)
sudo pacman -S vlc gtk3 libayatana-appindicator xdotool

# Ubuntu
sudo apt install vlc libgtk-3-dev libayatana-appindicator3-dev libxdo-dev

cargo build --release

# Foreground (recommended, works with systemd)
./target/release/playpnp serve

# Background with tray (falls back to headless if no tray host)
./target/release/playpnp

# Headless build for servers (no tray/GTK at all)
cargo build --release --no-default-features
```

### systemd user service (Linux)

```bash
mkdir -p ~/.config/systemd/user
cp contrib/playpnp.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now playpnp
```

---

## ⚙️ Configuration

| Variable | Description |
| :--- | :--- |
| `PLAYPNP_BIND_IP` | Pin a local IPv4 address (e.g. `192.168.43.100`) |
| `PLAYPNP_PEERS` | Extra IPs to unicast NOTIFY to, comma-separated (e.g. `192.168.43.1`) |
| `RUST_LOG` | Log level (`debug`, `info`, `warn`) |

Extra peers can also live in `peers.txt` — `%APPDATA%\playpnp\peers.txt` on Windows, `~/.config/playpnp/peers.txt` on Linux (one IP per line). The dashboard can add peers at runtime too. Device identity (`uuid`) persists next to it.

---

## 🛡️ Firewall

- **Windows:** allow inbound UDP 1900 + the ephemeral TCP port for `playpnp.exe`:
  ```powershell
  New-NetFirewallRule -DisplayName "playpnp release" -Direction Inbound -Program "$PWD\target\release\playpnp.exe" -Action Allow
  ```
  On a phone hotspot, set the network to **Private**: `Set-NetConnectionProfile -InterfaceAlias "Wi-Fi" -NetworkCategory Private`
- **Linux:** desktop installs need nothing — it works out of the box. Only if you run `ufw`/`nftables` yourself, open UDP 1900 in and the ephemeral TCP port.

---

## 📄 License

MIT
