mod config;
mod http;
mod ipc;
mod platform;
mod player;
mod single_instance;
mod ssdp;
mod state;
mod tray;
mod xml;

use config::Config;
use state::new_shared_state;
use std::sync::Arc;
use tokio::sync::watch;

fn exit_already_running() -> ! {
    eprintln!("PlayPnP Already Running");
    std::process::exit(1);
}

fn is_singleton_conflict(e: &anyhow::Error) -> bool {
    ipc::is_addr_in_use(e) || e.to_string().contains("already running")
}

/// Join the daemon thread, mapping a lost single-instance race to silent success.
fn join_daemon(handle: std::thread::JoinHandle<anyhow::Result<()>>) -> anyhow::Result<()> {
    match handle.join() {
        Ok(Ok(())) | Err(_) => Ok(()),
        Ok(Err(e)) if is_singleton_conflict(&e) => Ok(()),
        Ok(Err(e)) => Err(e),
    }
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("");

    match cmd {
        "serve" => {
            // Check if already running
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let already = rt.block_on(ipc::is_already_running());
            if already {
                exit_already_running();
            }

            // Foreground: initialize terminal logs
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
                )
                .with_target(false)
                .init();

            // File lock first: closes the check-then-bind race where two
            // processes both see "not running" before either binds :52411.
            // Held for the whole daemon lifetime (same path on Windows +
            // Linux, different root via platform::config_dir).
            let _instance_guard = match single_instance::SingleInstanceGuard::acquire()? {
                Some(g) => g,
                None => exit_already_running(),
            };

            let config = Config::load();
            let local_ip = config::local_ip();
            let endpoints = config::get_candidate_endpoints();
            println!(
                "playpnp {} serving in foreground...",
                env!("CARGO_PKG_VERSION")
            );
            println!("  FriendlyName: {}", config.friendly_name);
            println!("  UDN: {}", config.udn());
            println!("  Primary IP: {}", local_ip);
            println!(
                "  Endpoints: {}",
                endpoints
                    .iter()
                    .map(|e| {
                        let tag = if e.is_primary {
                            " [primary]"
                        } else if e.is_tailscale {
                            " [tailscale]"
                        } else {
                            ""
                        };
                        format!("{}{}", e.ip, tag)
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            println!("  SSDP: multicast + broadcast + gateway unicast + Tailscale peer unicast");
            println!(
                "  HTTP: 0.0.0.0:0 (ephemeral), description.xml uses dynamic URLBase from Host header"
            );
            println!("  Control: 127.0.0.1:52411 for playpnp stop/status");
            println!("  Dashboard: http://{}:<port>/ after startup", local_ip);
            println!("  Press Ctrl+C or run 'playpnp stop' to stop.");

            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            match rt.block_on(run_daemon(config, local_ip)) {
                Ok(()) => {}
                Err(e) if is_singleton_conflict(&e) => exit_already_running(),
                Err(e) => return Err(e),
            }
            println!("PlayPnP Stopped");
            return Ok(());
        }
        "__daemon" => {
            // Background worker process: silent, runs with tray
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let already = rt.block_on(ipc::is_already_running());
            if already {
                return Ok(());
            }

            // Atomic guard: a second `playpnp` racing us fails here even
            // though the control-port probe above said "not running".
            // Silent exit — the winner owns the daemon; `start`'s readiness
            // poll treats either outcome as success.
            let _instance_guard = match single_instance::SingleInstanceGuard::acquire()? {
                Some(g) => g,
                None => return Ok(()),
            };

            if std::env::var("RUST_LOG").is_ok() {
                let _ = tracing_subscriber::fmt()
                    .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
                    .with_target(false)
                    .try_init();
            }

            let config = Config::load();
            let local_ip = config::local_ip();

            #[cfg(feature = "tray")]
            {
                match run_with_tray(config, local_ip) {
                    Ok(()) => {}
                    Err(e) if is_singleton_conflict(&e) => return Ok(()),
                    Err(e) => return Err(e),
                }
            }
            #[cfg(not(feature = "tray"))]
            {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()?;
                match rt.block_on(run_daemon(config, local_ip)) {
                    Ok(()) => {}
                    Err(e) if is_singleton_conflict(&e) => return Ok(()),
                    Err(e) => return Err(e),
                }
            }
            return Ok(());
        }
        "stop" => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            rt.block_on(async {
                match ipc::send_stop().await {
                    Ok(_) => {
                        println!("PlayPnP Stopped");
                    }
                    Err(e) => {
                        if ipc::is_already_running().await {
                            eprintln!("failed to stop playpnp: {}", e);
                        } else {
                            println!("PlayPnP Not Running");
                        }
                    }
                }
            });
            return Ok(());
        }
        "status" => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            rt.block_on(async {
                match ipc::send_status().await {
                    Ok(msg) => println!("{}", msg.trim()),
                    Err(_) => {
                        println!("playpnp not running");
                    }
                }
            });
            return Ok(());
        }
        "diag" | "diagnose" | "--diag" => {
            run_diag();
            return Ok(());
        }
        "help" | "--help" | "-h" => {
            print_help();
            return Ok(());
        }
        "version" | "--version" | "-V" => {
            println!("playpnp {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        "" | "start" => {
            // Default: start background daemon silently with 📺 tray icon.
            if start_background_daemon()? {
                println!("PlayPnP Started");
            } else {
                println!("PlayPnP Already Running");
            }
            return Ok(());
        }
        other => {
            eprintln!("Unknown command: {}", other);
            print_help();
            std::process::exit(1);
        }
    }
}

/// Returns true when a daemon is (now) running because of this call,
/// false when one was already running.
fn start_background_daemon() -> anyhow::Result<bool> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    // Make repeated `playpnp` invocations harmless. The readiness wait below
    // also prevents an immediate `playpnp status` from losing a startup race.
    if rt.block_on(ipc::is_already_running()) {
        return Ok(false);
    }

    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("__daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Keep the daemon out of the caller's console while allowing the
        // tray's GUI/message loop to run normally.
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Detach into a new session so closing the terminal (SIGHUP)
        // doesn't kill the daemon. `serve` + systemd is still the
        // recommended Linux mode; this just makes bare `playpnp` sane.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    let _child = cmd.spawn()?;
    let ready = rt.block_on(async {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if ipc::is_already_running().await {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    });

    if ready {
        Ok(true)
    } else {
        anyhow::bail!("playpnp daemon did not become ready within 5 seconds")
    }
}

fn run_diag() {
    println!("playpnp diag — network diagnostics");
    println!("Version: {}", env!("CARGO_PKG_VERSION"));
    let local_ip = config::local_ip();
    println!("Primary IP: {}", local_ip);

    let endpoints = config::get_candidate_endpoints();
    println!("\nCandidate Endpoints ({}):", endpoints.len());
    for ep in &endpoints {
        let mut tags = Vec::new();
        if ep.is_primary {
            tags.push("PRIMARY");
        }
        if ep.is_tailscale {
            tags.push("TAILSCALE");
        }
        let bcast = ep.broadcast.map_or("-".into(), |b| b.to_string());
        let gw = ep.gateway.map_or("-".into(), |g| g.to_string());
        let mask = ep.netmask.map_or("-".into(), |m| m.to_string());
        println!(
            "  {} {:<16} mask={:<16} bcast={:<16} gw={:<16} [{}]",
            ep.name,
            ep.ip,
            mask,
            bcast,
            gw,
            tags.join(", ")
        );
    }

    println!("\nAll Interfaces (if-addrs):");
    if let Ok(addrs) = if_addrs::get_if_addrs() {
        let candidate_ips: Vec<_> = endpoints.iter().map(|e| e.ip).collect();
        for iface in addrs {
            let ip = iface.ip();
            let flags = match ip {
                std::net::IpAddr::V4(v4) if candidate_ips.contains(&v4) => "[candidate]",
                _ if iface.is_loopback() => "[loopback]",
                _ => "[filtered]",
            };
            println!("  {}: {} {}", iface.name, ip, flags);
        }
    }

    let peers = config::get_tailscale_peers();
    println!("\nTailscale/Unicast Peers ({}):", peers.len());
    if peers.is_empty() {
        println!("  (none detected — set PLAYPNP_PEERS=100.x.y.z or add to peers.txt)");
    } else {
        for p in &peers {
            println!("  {}", p);
        }
    }

    // VLC check
    if let Some(vlc_path) = player::find_vlc() {
        println!("\nVLC: Found at {}", vlc_path.display());
    } else {
        println!("\nVLC: NOT FOUND — playback is unavailable until VLC is installed");
    }

    println!("\nSDP Strategy:");
    println!("  Common Wi-Fi:  multicast 239.255.255.250:1900 + subnet broadcast per interface");
    println!("  Mobile Hotspot: unicast to gateway (e.g. 192.168.43.1:1900) + subnet broadcast");
    println!("  Tailscale:      unicast to each peer IP on port 1900");
    println!("  All M-SEARCH responses use pick_ip_for_target() to select correct LOCATION IP");
    println!("\nHTTP: binds 0.0.0.0:0 (ephemeral), description.xml uses Host header for URLBase");
    println!(
        "Dashboard: open http://{}:<port>/ in browser after starting",
        local_ip
    );

    #[cfg(windows)]
    {
        println!("\nPeers file: %APPDATA%\\playpnp\\peers.txt (one IP per line)");
        println!("\nFirewall: run 'Get-NetFirewallRule -DisplayName *playpnp* | Format-List'");
        println!(
            "Network profile: run 'Get-NetConnectionProfile | Format-List Name,NetworkCategory'"
        );
    }
    #[cfg(not(windows))]
    {
        println!(
            "\nPeers file: {} (one IP per line)",
            crate::platform::peers_hint()
        );
        println!("\nFirewall: allow UDP 1900 in + the ephemeral TCP port (ufw/nftables).");
        println!("Check listening sockets with: ss -tulpn | grep playpnp");
    }
}

fn print_help() {
    println!(
        r#"playpnp - DLNA MediaRenderer

Usage:
  playpnp          Start daemon in background with tray icon 📺 (silent)
  playpnp serve    Run in foreground without icon, all logs in terminal
  playpnp stop     Stop running daemon
  playpnp status   Show daemon status
  playpnp diag     Show network diagnostics (IPs, firewall help)
  playpnp help     Show this help
  playpnp version  Show version

Features:
  - 📺 System tray icon with status link and quit menu (falls back to headless)
  - Real VLC media player control via RC interface
  - Multi-network support: Common Wi-Fi, Mobile Hotspot, and Tailscale
  - Dynamic UPnP device description matching client IP
"#
    );
}

#[cfg(feature = "tray")]
fn run_with_tray(config: Config, local_ip: std::net::IpAddr) -> anyhow::Result<()> {
    use std::sync::mpsc;

    let friendly = config.friendly_name.clone();
    let (port_tx, port_rx) = mpsc::channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let shutdown_tx_daemon = shutdown_tx.clone();
    let shutdown_rx_daemon = shutdown_rx.clone();

    let daemon_handle = std::thread::spawn(move || -> anyhow::Result<()> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(run_daemon_with_channels(
            config,
            local_ip,
            port_tx,
            shutdown_tx_daemon,
            shutdown_rx_daemon,
        ))
    });

    let http_port = match port_rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(p) if p != 0 => p,
        // No HTTP announcement: daemon lost the singleton race before
        // serving. No second tray — exit silently.
        _ if daemon_handle.is_finished() => return join_daemon(daemon_handle),
        _ => 0,
    };

    let tray_result = tray::run_tray(
        shutdown_tx.clone(),
        shutdown_rx,
        friendly,
        http_port,
        local_ip,
    );

    match tray_result {
        Ok(()) => {
            // After tray exits, ensure daemon stops
            let _ = shutdown_tx.send(true);
            join_daemon(daemon_handle)
        }
        Err(e) => {
            // No tray host (e.g. headless server, minimal Wayland bar):
            // stay up headless instead of exiting. `serve` + systemd
            // is the recommended Linux mode; this is the safety net.
            tracing::warn!("Tray unavailable ({}), running headless", e);
            join_daemon(daemon_handle)
        }
    }
}

#[cfg(not(feature = "tray"))]
fn run_with_tray(_config: Config, _local_ip: std::net::IpAddr) -> anyhow::Result<()> {
    anyhow::bail!("tray feature disabled")
}

async fn run_daemon(config: Config, local_ip: std::net::IpAddr) -> anyhow::Result<()> {
    let (port_tx, _port_rx) = std::sync::mpsc::channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    run_daemon_with_channels(config, local_ip, port_tx, shutdown_tx, shutdown_rx).await
}

async fn run_daemon_with_channels(
    config: Config,
    local_ip: std::net::IpAddr,
    port_notify: std::sync::mpsc::Sender<u16>,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    // Singleton bind FIRST, before ephemeral HTTP (never conflicts) and
    // reuse-port SSDP (would silently duplicate). Loser exits here.
    let control_listener = ipc::bind_control_server().await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            anyhow::Error::new(e).context(format!(
                "playpnp is already running (control port {} busy)",
                ipc::CONTROL_ADDR
            ))
        } else {
            anyhow::Error::new(e).context("failed to bind control port")
        }
    })?;

    let av_state = new_shared_state();
    let player = player::create_player();

    let shutdown_rx_http = shutdown_rx.clone();
    let shutdown_rx_ssdp = shutdown_rx.clone();

    // Control server (listener already bound above — no second bind, no
    // swallowed AddrInUse).
    let friendly_clone = config.friendly_name.clone();
    let shutdown_tx_clone = shutdown_tx.clone();
    let control_handle = tokio::spawn(async move {
        if let Err(e) =
            ipc::serve_control_server(control_listener, shutdown_tx_clone, friendly_clone).await
        {
            tracing::error!("Control server error: {}", e);
        }
    });

    // HTTP server
    let (http_addr, http_shutdown_tx) = http::run_http_server(
        config.clone(),
        local_ip,
        av_state.clone(),
        player.clone(),
        shutdown_rx_http,
    )
    .await?;
    let http_port = http_addr.port();
    let _ = port_notify.send(http_port);
    tracing::info!("HTTP server ready on {}", http_addr);

    // SSDP
    let ssdp_server = Arc::new(ssdp::SsdpServer::new(
        config.udn(),
        config.friendly_name.clone(),
        local_ip,
        http_port,
    ));
    let ssdp_handle = tokio::spawn({
        let ssdp = ssdp_server.clone();
        async move {
            if let Err(e) = ssdp.run(shutdown_rx_ssdp).await {
                tracing::error!("SSDP error: {}", e);
            }
        }
    });

    // Wait for shutdown via Ctrl+C or control port
    let mut shutdown_rx_signal = shutdown_rx.clone();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Received Ctrl+C, shutting down");
            let _ = shutdown_tx.send(true);
            let _ = http_shutdown_tx.send(true);
        }
        _ = async {
            loop {
                if *shutdown_rx_signal.borrow() {
                    break;
                }
                if shutdown_rx_signal.changed().await.is_err() { break; }
                if *shutdown_rx_signal.borrow() { break; }
            }
        } => {
            tracing::info!("Shutdown via control port");
            let _ = http_shutdown_tx.send(true);
        }
    }

    tracing::info!("Shutting down SSDP and HTTP");
    let _ = shutdown_tx.send(true);
    let _ = http_shutdown_tx.send(true);
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    ssdp_handle.abort();
    control_handle.abort();
    Ok(())
}
