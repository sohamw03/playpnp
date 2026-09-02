use socket2::{Domain, Protocol, Socket, Type};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;

use crate::config::{
    get_candidate_endpoints, get_tailscale_peers, pick_ip_for_target, register_peer,
};

const MULTICAST_ADDR: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const MULTICAST_PORT: u16 = 1900;
const MAX_AGE: u32 = 1800;

pub struct SsdpServer {
    config_uuid: String,
    #[allow(dead_code)]
    friendly_name: String,
    #[allow(dead_code)]
    local_ip: IpAddr,
    http_port: u16,
}

impl SsdpServer {
    pub fn new(uuid: String, friendly_name: String, local_ip: IpAddr, http_port: u16) -> Self {
        Self {
            config_uuid: uuid,
            friendly_name,
            local_ip,
            http_port,
        }
    }

    fn location_for_ip(&self, ip: Ipv4Addr) -> String {
        format!("http://{}:{}/description.xml", ip, self.http_port)
    }

    fn usn_root(&self) -> String {
        format!("{}::upnp:rootdevice", self.config_uuid)
    }
    fn usn_renderer(&self) -> String {
        format!(
            "{}::urn:schemas-upnp-org:device:MediaRenderer:1",
            self.config_uuid
        )
    }
    fn usn_avtransport(&self) -> String {
        format!(
            "{}::urn:schemas-upnp-org:service:AVTransport:1",
            self.config_uuid
        )
    }
    fn usn_rendering(&self) -> String {
        format!(
            "{}::urn:schemas-upnp-org:service:RenderingControl:1",
            self.config_uuid
        )
    }
    fn usn_connection(&self) -> String {
        format!(
            "{}::urn:schemas-upnp-org:service:ConnectionManager:1",
            self.config_uuid
        )
    }

    fn notify_message_for_ip(&self, ip: Ipv4Addr, nt: &str, usn: &str, nts: &str) -> String {
        format!(
            "NOTIFY * HTTP/1.1\r\n\
             HOST: {mcast}:{port}\r\n\
             CACHE-CONTROL: max-age={max_age}\r\n\
             LOCATION: {loc}\r\n\
             NT: {nt}\r\n\
             NTS: {nts}\r\n\
             SERVER: Windows/10.0 UPnP/1.0 playpnp/1.0\r\n\
             USN: {usn}\r\n\
             BOOTID.UPNP.ORG: 1\r\n\
             CONFIGID.UPNP.ORG: 1\r\n\r\n",
            mcast = MULTICAST_ADDR,
            port = MULTICAST_PORT,
            max_age = MAX_AGE,
            loc = self.location_for_ip(ip),
            nt = nt,
            nts = nts,
            usn = usn
        )
    }

    fn alive_messages_for_ip(&self, ip: Ipv4Addr) -> Vec<String> {
        vec![
            self.notify_message_for_ip(ip, "upnp:rootdevice", &self.usn_root(), "ssdp:alive"),
            self.notify_message_for_ip(ip, &self.config_uuid, &self.config_uuid, "ssdp:alive"),
            self.notify_message_for_ip(
                ip,
                "urn:schemas-upnp-org:device:MediaRenderer:1",
                &self.usn_renderer(),
                "ssdp:alive",
            ),
            self.notify_message_for_ip(
                ip,
                "urn:schemas-upnp-org:service:AVTransport:1",
                &self.usn_avtransport(),
                "ssdp:alive",
            ),
            self.notify_message_for_ip(
                ip,
                "urn:schemas-upnp-org:service:RenderingControl:1",
                &self.usn_rendering(),
                "ssdp:alive",
            ),
            self.notify_message_for_ip(
                ip,
                "urn:schemas-upnp-org:service:ConnectionManager:1",
                &self.usn_connection(),
                "ssdp:alive",
            ),
        ]
    }

    fn byebye_messages_for_ip(&self, ip: Ipv4Addr) -> Vec<String> {
        vec![
            self.notify_message_for_ip(ip, "upnp:rootdevice", &self.usn_root(), "ssdp:byebye"),
            self.notify_message_for_ip(ip, &self.config_uuid, &self.config_uuid, "ssdp:byebye"),
            self.notify_message_for_ip(
                ip,
                "urn:schemas-upnp-org:device:MediaRenderer:1",
                &self.usn_renderer(),
                "ssdp:byebye",
            ),
            self.notify_message_for_ip(
                ip,
                "urn:schemas-upnp-org:service:AVTransport:1",
                &self.usn_avtransport(),
                "ssdp:byebye",
            ),
            self.notify_message_for_ip(
                ip,
                "urn:schemas-upnp-org:service:RenderingControl:1",
                &self.usn_rendering(),
                "ssdp:byebye",
            ),
            self.notify_message_for_ip(
                ip,
                "urn:schemas-upnp-org:service:ConnectionManager:1",
                &self.usn_connection(),
                "ssdp:byebye",
            ),
        ]
    }

    fn msearch_response_for_ip(&self, st: &str, ip: Ipv4Addr) -> Option<String> {
        let loc = self.location_for_ip(ip);
        let st_clean = st.trim().trim_matches('"').trim_matches('\'');

        let (usn, st_val) = if st_clean.eq_ignore_ascii_case("ssdp:all")
            || st_clean.eq_ignore_ascii_case("upnp:rootdevice")
        {
            (self.usn_root(), "upnp:rootdevice".to_string())
        } else if st_clean.eq_ignore_ascii_case(&self.config_uuid) {
            (self.config_uuid.clone(), self.config_uuid.clone())
        } else if st_clean.to_lowercase().contains("mediarenderer") {
            (
                self.usn_renderer(),
                "urn:schemas-upnp-org:device:MediaRenderer:1".to_string(),
            )
        } else if st_clean.to_lowercase().contains("avtransport") {
            (
                self.usn_avtransport(),
                "urn:schemas-upnp-org:service:AVTransport:1".to_string(),
            )
        } else if st_clean.to_lowercase().contains("renderingcontrol") {
            (
                self.usn_rendering(),
                "urn:schemas-upnp-org:service:RenderingControl:1".to_string(),
            )
        } else if st_clean.to_lowercase().contains("connectionmanager") {
            (
                self.usn_connection(),
                "urn:schemas-upnp-org:service:ConnectionManager:1".to_string(),
            )
        } else {
            return None;
        };

        Some(format!(
            "HTTP/1.1 200 OK\r\n\
             CACHE-CONTROL: max-age={max_age}\r\n\
             EXT:\r\n\
             LOCATION: {loc}\r\n\
             SERVER: Windows/10.0 UPnP/1.0 playpnp/1.0\r\n\
             ST: {st}\r\n\
             USN: {usn}\r\n\
             BOOTID.UPNP.ORG: 1\r\n\
             CONFIGID.UPNP.ORG: 1\r\n\r\n",
            max_age = MAX_AGE,
            loc = loc,
            st = st_val,
            usn = usn
        ))
    }

    fn msearch_responses_for_all_for_ip(&self, ip: Ipv4Addr) -> Vec<String> {
        vec![
            self.msearch_response_for_ip("upnp:rootdevice", ip).unwrap(),
            self.msearch_response_for_ip(&self.config_uuid, ip).unwrap(),
            self.msearch_response_for_ip("urn:schemas-upnp-org:device:MediaRenderer:1", ip)
                .unwrap(),
            self.msearch_response_for_ip("urn:schemas-upnp-org:service:AVTransport:1", ip)
                .unwrap(),
            self.msearch_response_for_ip("urn:schemas-upnp-org:service:RenderingControl:1", ip)
                .unwrap(),
            self.msearch_response_for_ip("urn:schemas-upnp-org:service:ConnectionManager:1", ip)
                .unwrap(),
        ]
    }

    pub async fn run(
        self: Arc<Self>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        let socket = create_multicast_socket()?;
        let std_socket: std::net::UdpSocket = socket.into();
        std_socket.set_nonblocking(true)?;
        let udp = Arc::new(UdpSocket::from_std(std_socket)?);

        tracing::info!(
            "SSDP listening on 0.0.0.0:1900, multicast {}",
            MULTICAST_ADDR
        );

        // Spawn notifier loop
        let notifier_self = self.clone();
        let mut shutdown_n = shutdown.clone();
        tokio::spawn(async move {
            // initial alive bursts
            for i in 0..3 {
                if i > 0 {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                notifier_self.send_alive_all().await;
            }

            // Periodic heartbeat every 30 seconds for maximum reliability across hotspots and Tailscale
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            interval.tick().await; // skip immediate
            loop {
                tokio::select! {
                    _ = shutdown_n.changed() => {
                        tracing::info!("SSDP notifier shutting down, sending byebye");
                        notifier_self.send_byebye_all().await;
                        break;
                    }
                    _ = interval.tick() => {
                        notifier_self.send_alive_all().await;
                    }
                }
            }
        });

        // Responder loop
        let mut buf = [0u8; 2048];
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    tracing::info!("SSDP responder shutdown");
                    break;
                }
                res = udp.recv_from(&mut buf) => {
                    match res {
                        Ok((len, addr)) => {
                            let data = &buf[..len];
                            if let Ok(text) = std::str::from_utf8(data) {
                                if text.to_uppercase().contains("M-SEARCH") {
                                    self.handle_msearch(text, &*udp, addr).await;
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!("SSDP recv error: {}", e);
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Broadcast / Multicast / Unicast SSDP NOTIFY alive across all interfaces
    async fn send_alive_all(&self) {
        let endpoints = get_candidate_endpoints();
        let mcast_dest = SocketAddr::new(IpAddr::V4(MULTICAST_ADDR), MULTICAST_PORT);

        for ep in &endpoints {
            let msgs = self.alive_messages_for_ip(ep.ip);

            // Create a socket bound specifically to this interface to guarantee proper outgoing path
            let sender = match create_sender_socket_for_ip(ep.ip) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("Failed to create sender socket for {}: {}", ep.ip, e);
                    continue;
                }
            };

            // 1. Send Multicast to 239.255.255.250:1900
            for msg in &msgs {
                let _ = sender.send_to(msg.as_bytes(), mcast_dest).await;
            }

            // 2. Send Subnet Broadcast (e.g. 192.168.43.255:1900) to defeat AP isolation & hotspot multicast blocks
            if let Some(bcast) = ep.broadcast {
                let bcast_dest = SocketAddr::new(IpAddr::V4(bcast), MULTICAST_PORT);
                for msg in &msgs {
                    let _ = sender.send_to(msg.as_bytes(), bcast_dest).await;
                }
            }

            // 3. Send Unicast to Default Gateway (e.g. 192.168.43.1:1900 - phone mobile hotspot!)
            if let Some(gw) = ep.gateway {
                let gw_dest = SocketAddr::new(IpAddr::V4(gw), MULTICAST_PORT);
                for msg in &msgs {
                    let _ = sender.send_to(msg.as_bytes(), gw_dest).await;
                }
            }

            tracing::debug!("SSDP NOTIFY alive dispatched on {} ({})", ep.name, ep.ip);
        }

        // 4. Send Unicast to all detected Tailscale peers and dynamic peers
        let tailscale_peers = get_tailscale_peers();
        if !tailscale_peers.is_empty() {
            // Pick local Tailscale IP if available, or primary IP
            let source_ep = endpoints
                .iter()
                .find(|e| e.is_tailscale)
                .or_else(|| endpoints.first());
            if let Some(ep) = source_ep {
                if let Ok(sender) = create_sender_socket_for_ip(ep.ip) {
                    let msgs = self.alive_messages_for_ip(ep.ip);
                    for peer_ip in tailscale_peers {
                        let peer_dest = SocketAddr::new(IpAddr::V4(peer_ip), MULTICAST_PORT);
                        for msg in &msgs {
                            let _ = sender.send_to(msg.as_bytes(), peer_dest).await;
                        }
                        tracing::info!(
                            "SSDP NOTIFY unicast sent to peer {} from {}",
                            peer_dest,
                            ep.ip
                        );
                    }
                }
            }
        }
    }

    /// Send byebye on all interfaces
    async fn send_byebye_all(&self) {
        let endpoints = get_candidate_endpoints();
        let mcast_dest = SocketAddr::new(IpAddr::V4(MULTICAST_ADDR), MULTICAST_PORT);

        for ep in &endpoints {
            let msgs = self.byebye_messages_for_ip(ep.ip);
            let sender = match create_sender_socket_for_ip(ep.ip) {
                Ok(s) => s,
                Err(_) => continue,
            };

            for msg in &msgs {
                let _ = sender.send_to(msg.as_bytes(), mcast_dest).await;
            }
            if let Some(bcast) = ep.broadcast {
                let bcast_dest = SocketAddr::new(IpAddr::V4(bcast), MULTICAST_PORT);
                for msg in &msgs {
                    let _ = sender.send_to(msg.as_bytes(), bcast_dest).await;
                }
            }
            if let Some(gw) = ep.gateway {
                let gw_dest = SocketAddr::new(IpAddr::V4(gw), MULTICAST_PORT);
                for msg in &msgs {
                    let _ = sender.send_to(msg.as_bytes(), gw_dest).await;
                }
            }
        }
    }

    async fn handle_msearch(&self, text: &str, udp: &UdpSocket, addr: SocketAddr) {
        let st = extract_header(text, "ST").unwrap_or_default();
        let st_trim = st.trim().trim_matches('"').trim_matches('\'');

        // Record sender IP as an active peer for continuous updates
        if let IpAddr::V4(v4) = addr.ip() {
            register_peer(v4);
        }

        let best_ip = pick_ip_for_target(addr.ip());
        tracing::info!(
            "M-SEARCH from {} ST='{}' -> responding with LOCATION http://{}:{}/description.xml",
            addr,
            st_trim,
            best_ip,
            self.http_port
        );

        if st_trim.eq_ignore_ascii_case("ssdp:all") {
            for resp in self.msearch_responses_for_all_for_ip(best_ip) {
                let _ = udp.send_to(resp.as_bytes(), addr).await;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            return;
        }

        if let Some(resp) = self.msearch_response_for_ip(st_trim, best_ip) {
            let _ = udp.send_to(resp.as_bytes(), addr).await;
            tracing::debug!("Sent M-SEARCH response to {} for ST {}", addr, st_trim);
        }
    }
}

fn extract_header(text: &str, name: &str) -> Option<String> {
    for line in text.lines() {
        if let Some(colon) = line.find(':') {
            let (k, v) = line.split_at(colon);
            if k.trim().eq_ignore_ascii_case(name) {
                return Some(v[1..].trim().to_string());
            }
        }
    }
    None
}

/// Create UDP socket bound specifically to an interface for reliable transmission
fn create_sender_socket_for_ip(ip: Ipv4Addr) -> anyhow::Result<UdpSocket> {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    s.set_broadcast(true)?;
    s.set_multicast_ttl_v4(4)?;
    let _ = s.set_multicast_if_v4(&ip);
    s.bind(&SocketAddr::new(IpAddr::V4(ip), 0).into())?;
    s.set_nonblocking(true)?;
    let std_s: std::net::UdpSocket = s.into();
    Ok(UdpSocket::from_std(std_s)?)
}

fn create_multicast_socket() -> anyhow::Result<Socket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    {
        let _ = socket.set_reuse_port(true);
    }
    socket.set_nonblocking(true)?;
    let bind_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), MULTICAST_PORT);
    socket.bind(&bind_addr.into())?;

    let mcast = MULTICAST_ADDR;
    let _ = socket.join_multicast_v4(&mcast, &Ipv4Addr::UNSPECIFIED);

    // Join on all candidate interface IPs
    for ep in get_candidate_endpoints() {
        let _ = socket.join_multicast_v4(&mcast, &ep.ip);
    }

    socket.set_multicast_ttl_v4(4)?;
    socket.set_multicast_loop_v4(true)?;
    Ok(socket)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mcast_receive() {
        let sock = match create_multicast_socket() {
            Ok(s) => s,
            Err(e) => {
                println!("create_multicast_socket failed: {}", e);
                return;
            }
        };
        let std_sock: std::net::UdpSocket = sock.into();
        std_sock.set_nonblocking(true).unwrap();
        let tokio_sock = UdpSocket::from_std(std_sock).unwrap();

        let sender = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        sender.set_multicast_loop_v4(true).unwrap();
        sender
            .send_to(b"TEST_MSEARCH", "239.255.255.250:1900")
            .unwrap();

        let mut buf = [0u8; 128];
        match tokio::time::timeout(Duration::from_millis(500), tokio_sock.recv_from(&mut buf)).await
        {
            Ok(Ok((n, from))) => {
                println!(
                    "Received {} bytes from {}: {}",
                    n,
                    from,
                    String::from_utf8_lossy(&buf[..n])
                );
            }
            Ok(Err(e)) => println!("Recv error: {}", e),
            Err(_) => println!("Recv timed out!"),
        }
    }
}
