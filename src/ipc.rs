use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub const CONTROL_ADDR: &str = "127.0.0.1:52411";

pub async fn is_already_running() -> bool {
    let Ok(Ok(mut stream)) = tokio::time::timeout(
        std::time::Duration::from_millis(300),
        TcpStream::connect(CONTROL_ADDR),
    )
    .await
    else {
        return false;
    };

    if !matches!(
        tokio::time::timeout(
            std::time::Duration::from_millis(300),
            stream.write_all(b"STATUS\n"),
        )
        .await,
        Ok(Ok(()))
    ) {
        return false;
    }

    let mut buf = [0u8; 128];
    let Ok(Ok(n)) =
        tokio::time::timeout(std::time::Duration::from_millis(300), stream.read(&mut buf)).await
    else {
        return false;
    };

    String::from_utf8_lossy(&buf[..n])
        .trim_start()
        .starts_with("OK running ")
}

pub async fn send_stop() -> anyhow::Result<String> {
    let mut stream = TcpStream::connect(CONTROL_ADDR).await?;
    stream.write_all(b"STOP\n").await?;
    stream.flush().await?;
    let mut buf = [0u8; 512];
    let n =
        tokio::time::timeout(std::time::Duration::from_secs(2), stream.read(&mut buf)).await??;
    let response = String::from_utf8_lossy(&buf[..n]).to_string();

    // The control server acknowledges STOP before the tray/message loop has
    // necessarily finished tearing down HTTP and SSDP. Wait briefly so the
    // CLI does not claim success while the daemon is still reachable.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if !is_already_running().await {
            return Ok(response);
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    anyhow::bail!("playpnp did not stop within 5 seconds")
}

pub async fn send_status() -> anyhow::Result<String> {
    let mut stream = TcpStream::connect(CONTROL_ADDR).await?;
    stream.write_all(b"STATUS\n").await?;
    stream.flush().await?;
    let mut buf = [0u8; 1024];
    let n =
        tokio::time::timeout(std::time::Duration::from_secs(2), stream.read(&mut buf)).await??;
    Ok(String::from_utf8_lossy(&buf[..n]).to_string())
}

pub async fn run_control_server(
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    friendly_name: String,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(CONTROL_ADDR).await?;
    tracing::info!("Control server listening on {}", CONTROL_ADDR);
    loop {
        let (mut socket, addr) = listener.accept().await?;
        let shutdown_tx_clone = shutdown_tx.clone();
        let friendly = friendly_name.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 256];
            let n = match socket.read(&mut buf).await {
                Ok(n) => n,
                Err(_) => return,
            };
            let cmd = String::from_utf8_lossy(&buf[..n]).trim().to_string();
            tracing::debug!("Control command from {}: {}", addr, cmd);
            let response =
                if cmd.eq_ignore_ascii_case("STOP") || cmd.to_uppercase().starts_with("STOP") {
                    let _ = shutdown_tx_clone.send(true);
                    format!("OK stopping {}\n", friendly)
                } else if cmd.eq_ignore_ascii_case("STATUS") {
                    format!("OK running {} pid={}\n", friendly, std::process::id())
                } else {
                    format!("OK unknown command {}\n", cmd)
                };
            let _ = socket.write_all(response.as_bytes()).await;
        });
    }
}
