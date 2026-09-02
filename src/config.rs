use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct Config {
    pub uuid: Uuid,
    pub friendly_name: String,
    pub http_port: u16, // 0 = ephemeral
    pub control_port: u16,
}

impl Config {
    pub fn load() -> Self {
        let uuid = load_or_create_uuid();
        let hostname = hostname::get().unwrap_or_else(|_| "playpnp".into());
        let friendly_name = format!("playpnp ({})", hostname);
        Self {
            uuid,
            friendly_name,
            http_port: 0,
            control_port: 52411,
        }
    }

    pub fn uuid_string(&self) -> String {
        format!("uuid:{}", self.uuid)
    }

    pub fn udn(&self) -> String {
        self.uuid_string()
    }
}

fn config_dir() -> PathBuf {
    let base = dirs::config_dir()
        .or_else(dirs::data_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("playpnp")
}

fn uuid_file_path() -> PathBuf {
    config_dir().join("uuid")
}

fn load_or_create_uuid() -> Uuid {
    let path = uuid_file_path();
    if let Ok(content) = std::fs::read_to_string(&path) {
        if let Ok(u) = Uuid::parse_str(content.trim().trim_start_matches("uuid:")) {
            return u;
        }
    }
    let new_uuid = Uuid::new_v4();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, new_uuid.to_string());
    new_uuid
}

mod hostname {
    pub fn get() -> Result<String, ()> {
        if let Ok(h) = std::env::var("COMPUTERNAME") {
            if !h.is_empty() {
                return Ok(h);
            }
        }
        if let Ok(h) = std::env::var("HOSTNAME") {
            if !h.is_empty() {
                return Ok(h);
            }
        }
        Ok("PC".to_string())
    }
}

mod dirs {
    use std::path::PathBuf;
    pub fn config_dir() -> Option<PathBuf> {
        std::env::var("APPDATA").ok().map(PathBuf::from)
    }
    pub fn data_dir() -> Option<PathBuf> {
        std::env::var("APPDATA").ok().map(PathBuf::from)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkEndpoint {
    pub name: String,
    pub ip: Ipv4Addr,
    pub netmask: Option<Ipv4Addr>,
    pub broadcast: Option<Ipv4Addr>,
    pub gateway: Option<Ipv4Addr>,
    pub is_tailscale: bool,
    pub is_primary: bool,
}

static DYNAMIC_PEERS: std::sync::LazyLock<Arc<Mutex<HashSet<Ipv4Addr>>>> =
    std::sync::LazyLock::new(|| Arc::new(Mutex::new(HashSet::new())));

/// Register a remote IP that contacted us (e.g. via HTTP or M-SEARCH) for proactive SSDP NOTIFY
pub fn register_peer(ip: Ipv4Addr) {
    if ip.is_loopback() || ip.is_unspecified() {
        return;
    }
    if let Ok(mut peers) = DYNAMIC_PEERS.lock() {
        if peers.insert(ip) {
            tracing::info!("Registered dynamic peer for SSDP NOTIFY: {}", ip);
        }
    }
}

pub fn get_registered_peers() -> Vec<Ipv4Addr> {
    if let Ok(peers) = DYNAMIC_PEERS.lock() {
        peers.iter().copied().collect()
    } else {
        Vec::new()
    }
}

/// UDP route probe: find which local IP is used to reach an external target
fn probe_route_ip(target: &str) -> Option<Ipv4Addr> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect(target).ok()?;
    match sock.local_addr().ok()?.ip() {
        IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_unspecified() => Some(v4),
        _ => None,
    }
}

/// Detect default gateway from routing table
fn detect_default_gateway() -> Option<Ipv4Addr> {
    // Run `route print 0.0.0.0` on Windows
    let output = std::process::Command::new("route")
        .args(["print", "0.0.0.0"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        // Look for: 0.0.0.0   0.0.0.0   <gateway>   <interface>   <metric>
        if parts.len() >= 4 && parts[0] == "0.0.0.0" && parts[1] == "0.0.0.0" {
            if let Ok(gw) = parts[2].parse::<Ipv4Addr>() {
                if !gw.is_unspecified() && !gw.is_loopback() {
                    return Some(gw);
                }
            }
        }
    }
    None
}

/// Check if an IP belongs to VirtualBox host-only network (192.168.56.x default)
fn is_virtualbox_ip(ip: &Ipv4Addr) -> bool {
    let oct = ip.octets();
    oct[0] == 192 && oct[1] == 168 && oct[2] == 56
}

/// Check if an IP is link-local APIPA (169.254.x.x)
fn is_link_local(ip: &Ipv4Addr) -> bool {
    let oct = ip.octets();
    oct[0] == 169 && oct[1] == 254
}

/// Check if an IP is a Tailscale CGNAT address (100.64.0.0/10)
pub fn is_tailscale_ip(ip: &Ipv4Addr) -> bool {
    let oct = ip.octets();
    oct[0] == 100 && (64..=127).contains(&oct[1])
}

/// Returns the primary local IPv4 address for advertising
pub fn local_ip() -> IpAddr {
    // 1. Env override
    if let Ok(forced) = std::env::var("PLAYPNP_BIND_IP") {
        if let Ok(v4) = forced.parse::<Ipv4Addr>() {
            tracing::info!("Using forced PLAYPNP_BIND_IP={}", v4);
            return IpAddr::V4(v4);
        }
    }

    // 2. UDP probe to public DNS (finds actual active Wi-Fi or Hotspot interface with internet)
    if let Some(ip) = probe_route_ip("8.8.8.8:80") {
        if !is_virtualbox_ip(&ip) && !is_link_local(&ip) {
            return IpAddr::V4(ip);
        }
    }

    // 3. local-ip-address crate
    if let Ok(IpAddr::V4(v4)) = local_ip_address::local_ip() {
        if !is_virtualbox_ip(&v4) && !is_link_local(&v4) {
            return IpAddr::V4(v4);
        }
    }

    // 4. Fallback to first valid candidate endpoint
    if let Some(ep) = get_candidate_endpoints().into_iter().next() {
        return IpAddr::V4(ep.ip);
    }

    IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))
}

/// Returns all candidate network endpoints (Wi-Fi, Hotspot, Tailscale, Ethernet)
pub fn get_candidate_endpoints() -> Vec<NetworkEndpoint> {
    // Env override
    if let Ok(forced) = std::env::var("PLAYPNP_BIND_IP") {
        if let Ok(v4) = forced.parse::<Ipv4Addr>() {
            return vec![NetworkEndpoint {
                name: "Forced".to_string(),
                ip: v4,
                netmask: Some(Ipv4Addr::new(255, 255, 255, 0)),
                broadcast: Some(Ipv4Addr::new(
                    v4.octets()[0],
                    v4.octets()[1],
                    v4.octets()[2],
                    255,
                )),
                gateway: None,
                is_tailscale: is_tailscale_ip(&v4),
                is_primary: true,
            }];
        }
    }

    let default_gw = detect_default_gateway();
    let primary_ip = probe_route_ip("8.8.8.8:80");
    let tailscale_probe = probe_route_ip("100.100.100.100:80");

    let mut endpoints = Vec::new();

    if let Ok(addrs) = if_addrs::get_if_addrs() {
        for iface in addrs {
            if iface.is_loopback() {
                continue;
            }

            if let if_addrs::IfAddr::V4(ref v4_iface) = iface.addr {
                let ip = v4_iface.ip;
                if is_link_local(&ip) || is_virtualbox_ip(&ip) {
                    continue;
                }

                let name = iface.name.to_lowercase();
                if name.contains("wsl")
                    || name.contains("hyper-v")
                    || name.contains("hyperv")
                    || name.contains("vmware")
                    || name.contains("virtualbox")
                    || name.contains("vbox")
                    || name.contains("docker")
                    || name.contains("veth")
                {
                    continue;
                }

                let is_ts = is_tailscale_ip(&ip) || name.contains("tailscale");
                let is_primary = primary_ip.map_or(false, |p| p == ip);

                // Derive gateway: if this interface is the primary route, assign detected default gateway
                // Otherwise heuristic .1 if /24
                let gw = if is_primary {
                    default_gw
                } else if !is_ts {
                    let oct = ip.octets();
                    Some(Ipv4Addr::new(oct[0], oct[1], oct[2], 1))
                } else {
                    None
                };

                endpoints.push(NetworkEndpoint {
                    name: iface.name.clone(),
                    ip,
                    netmask: Some(v4_iface.netmask),
                    broadcast: v4_iface.broadcast,
                    gateway: gw,
                    is_tailscale: is_ts,
                    is_primary,
                });
            }
        }
    }

    // If Tailscale probe found an IP not listed in if-addrs, add it
    if let Some(ts_ip) = tailscale_probe {
        if is_tailscale_ip(&ts_ip) && !endpoints.iter().any(|e| e.ip == ts_ip) {
            endpoints.push(NetworkEndpoint {
                name: "Tailscale (Probed)".to_string(),
                ip: ts_ip,
                netmask: Some(Ipv4Addr::new(255, 192, 0, 0)),
                broadcast: None,
                gateway: None,
                is_tailscale: true,
                is_primary: false,
            });
        }
    }

    // Sort: Primary first, then non-Tailscale LAN (Wi-Fi/Hotspot/Ethernet), then Tailscale
    endpoints.sort_by_key(|e| {
        if e.is_primary {
            0
        } else if !e.is_tailscale {
            let oct = e.ip.octets();
            if oct[0] == 192 && oct[1] == 168 {
                1
            } else if oct[0] == 10 {
                2
            } else {
                3
            }
        } else {
            10 // Tailscale
        }
    });

    endpoints
}

/// Returns list of all candidate IPv4 addresses
#[allow(dead_code)]
pub fn get_candidate_ips() -> Vec<Ipv4Addr> {
    get_candidate_endpoints()
        .into_iter()
        .map(|e| e.ip)
        .collect()
}

/// Pick best local IP to reach a specific target (same /24, /16, or Tailscale)
pub fn pick_ip_for_target(target: IpAddr) -> Ipv4Addr {
    let endpoints = get_candidate_endpoints();
    if endpoints.is_empty() {
        return Ipv4Addr::new(127, 0, 0, 1);
    }

    if let IpAddr::V4(tgt4) = target {
        // If target is Tailscale (100.64.0.0/10), pick local Tailscale IP
        if is_tailscale_ip(&tgt4) {
            if let Some(ts_ep) = endpoints.iter().find(|e| e.is_tailscale) {
                return ts_ep.ip;
            }
        }

        let tgt_oct = tgt4.octets();
        // Check exact /24 match
        for ep in &endpoints {
            let oct = ep.ip.octets();
            if oct[0] == tgt_oct[0] && oct[1] == tgt_oct[1] && oct[2] == tgt_oct[2] {
                return ep.ip;
            }
        }

        // Check /16 match for 192.168 or 10.
        for ep in &endpoints {
            let oct = ep.ip.octets();
            if oct[0] == tgt_oct[0] && oct[1] == tgt_oct[1] {
                return ep.ip;
            }
        }
    }

    // Default to primary endpoint
    endpoints[0].ip
}

/// Query Tailscale CLI for active peers on the tailnet
pub fn get_tailscale_peers() -> Vec<Ipv4Addr> {
    let mut peers = HashSet::new();

    // 1. Check environment variable PLAYPNP_PEERS (comma-separated)
    if let Ok(env_peers) = std::env::var("PLAYPNP_PEERS") {
        for s in env_peers.split(',') {
            if let Ok(ip) = s.trim().parse::<Ipv4Addr>() {
                peers.insert(ip);
            }
        }
    }

    // 2. Check peers.txt file in config dir
    let peers_file = config_dir().join("peers.txt");
    if let Ok(content) = std::fs::read_to_string(peers_file) {
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') || trimmed.is_empty() {
                continue;
            }
            if let Ok(ip) = trimmed.parse::<Ipv4Addr>() {
                peers.insert(ip);
            }
        }
    }

    // 3. Try querying `tailscale status --json`
    if let Ok(output) = std::process::Command::new("tailscale")
        .args(["status", "--json"])
        .output()
    {
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout);
            // Simple parsing for "TailscaleIPs":["100.x.y.z",...]
            for chunk in text.split("\"TailscaleIPs\":[") {
                if let Some(end) = chunk.find(']') {
                    let ips_part = &chunk[..end];
                    for item in ips_part.split(',') {
                        let clean = item.trim().trim_matches('"');
                        if let Ok(ip) = clean.parse::<Ipv4Addr>() {
                            if is_tailscale_ip(&ip) {
                                peers.insert(ip);
                            }
                        }
                    }
                }
            }
        }
    }

    // 4. Include dynamic peers that contacted us
    for p in get_registered_peers() {
        peers.insert(p);
    }

    peers.into_iter().collect()
}
