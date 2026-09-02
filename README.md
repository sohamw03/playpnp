# playpnp — DLNA MediaRenderer for Windows

Headless tray daemon that makes your Windows PC appear as a robust **DLNA MediaRenderer** (DMR) to **BubbleUPnP**, **mConnect**, **VLC**, and any UPnP/DLNA control point.

Cast videos, music, and streams directly to your Windows PC with real playback in **VLC media player**, featuring synchronized position tracking, volume control, seeking, and pause/resume.

---

## 🎯 Supported Connectivity Scenarios

`playpnp` is architected to work seamlessly across all three core connectivity topologies:

| Scenario | Network Setup | Discovery & Routing Mechanism |
| :--- | :--- | :--- |
| **1. Mobile Hotspot** | Phone emits mobile hotspot; PC connects to hotspot Wi-Fi | Android kernel blocks L2 multicast (`239.255.255.250`). `playpnp` detects the default gateway (the phone's IP, e.g. `192.168.43.1`) and sends **directed unicast NOTIFY** to `gateway:1900` + **subnet broadcast** (`192.168.43.255:1900`). BubbleUPnP discovers the PC instantly. |
| **2. Tailscale VPN** | Phone & PC on separate internet connections, connected via Tailscale | Tailscale does not route multicast. `playpnp` discovers Tailscale peers via `tailscale status --json`, `PLAYPNP_PEERS=100.x.y.z`, or `%APPDATA%\playpnp\peers.txt`, and sends **unicast NOTIFY** directly to `peer_ip:1900`. `pick_ip_for_target()` ensures `LOCATION` URLs advertise the `100.x` Tailscale IP. |
| **3. Common Wi-Fi** | Phone & PC on the same Wi-Fi network / LAN | Outgoing UDP sockets bind with `IP_MULTICAST_IF` physically on the Wi-Fi adapter. Sends standard multicast `239.255.255.250:1900` + directed subnet broadcast. VirtualBox (`192.168.56.x`), WSL, and link-local interfaces are automatically filtered out. |

---

## ✨ Features

- **Real VLC Player Backend (`VlcPlayer`):**
  - Manages VLC via its remote control TCP socket interface (`--extraintf rc --rc-host 127.0.0.1:52422`).
  - Supports real **Play**, **Pause**, **Stop**, **Seek** (`seek <seconds>`), **Volume** (`volume <0-512>`), and Track Duration polling.
  - VLC is the supported playback backend and is required. Screenbox/Windows Media Player are not used because they do not expose a compatible control/state API here.
  - No duplicate windows: reuses the VLC instance started by playpnp when switching tracks.
- **Dynamic `<URLBase>` UPnP Generation:**
  - `GET /description.xml` dynamically inspects the HTTP `Host` header (e.g. `192.168.1.102:port`, `192.168.43.x:port`, or `100.x.y.z:port`) and returns `<URLBase>http://{host}/</URLBase>`.
  - Guarantees control and event subscription URLs are reachable regardless of which interface the phone used.
- **Web Dashboard & Remote Controls:**
  - Open `http://<pc-ip>:<port>/` in any browser (desktop or mobile).
  - Inspect current playback status (track title, artist, elapsed/total duration, volume).
  - Control playback remotely with Play, Pause, and Stop buttons.
  - View all active network endpoints and manually add peers for unicast notification.
- **System Tray & Local IPC:**
  - Runs unobtrusively in the Windows system tray.
  - Local control CLI via `playpnp status` and `playpnp stop` on `127.0.0.1:52411`.

---

## 🚀 Quick Start

### Build

```powershell
cargo build --release
```

### Run Daemon

```powershell
# Run with tray icon (normal mode)
.\target\release\playpnp.exe

# Or run headless in terminal (no tray icon)
.\target\release\playpnp.exe serve
```

### Inspect Diagnostics

```powershell
.\target\release\playpnp.exe diag
```

Outputs primary routing IP, candidate endpoints, broadcast addresses, gateways, detected Tailscale peers, and VLC installation status.

### Manage Running Daemon

```powershell
# Check status
.\target\release\playpnp.exe status

# Clean shutdown
.\target\release\playpnp.exe stop
```

---

## ⚙️ Environment Variables & Customization

| Variable | Description |
| :--- | :--- |
| `PLAYPNP_BIND_IP` | Force a specific local IPv4 address (e.g. `192.168.43.100`). |
| `PLAYPNP_PEERS` | Comma-separated list of Tailscale or remote peer IPs to notify via unicast (e.g. `100.82.14.5,192.168.43.1`). |
| `PLAYPNP_NO_TRAY` | Set to `1` to run in headless console mode without a system tray icon. |
| `RUST_LOG` | Set logging level (`debug`, `info`, `warn`). |

Peers can also be added permanently by creating `%APPDATA%\playpnp\peers.txt` (one IP per line).

---

## 🛡️ Windows Firewall Notice

Windows Firewall must allow incoming UDP (port 1900) and TCP (ephemeral port) on `playpnp.exe`.

To verify or add firewall rules:
```powershell
# Allow playpnp inbound TCP & UDP
New-NetFirewallRule -DisplayName "playpnp release" -Direction Inbound -Program "$PWD\target\release\playpnp.exe" -Action Allow
```

When connected to a phone mobile hotspot, ensure Windows classifies the network as **Private**:
```powershell
Set-NetConnectionProfile -InterfaceAlias "Wi-Fi" -NetworkCategory Private
```

---

## 📄 License

MIT
