#[cfg(feature = "tray")]
pub fn run_tray(
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
    friendly_name: String,
    http_port: u16,
    local_ip: std::net::IpAddr,
) -> anyhow::Result<()> {
    use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};
    use tray_icon::{TrayIconBuilder, TrayIconEvent, menu::MenuEvent};
    use winit::event_loop::{ControlFlow, EventLoop};

    // muda/tray-icon menus call into GTK on Linux, which panics
    // ("GTK has not been initialized") instead of returning Err —
    // and panic=abort would take the whole daemon down. Init first;
    // a failed init (e.g. no display) falls back to headless via Err.
    #[cfg(target_os = "linux")]
    gtk::init().map_err(|e| anyhow::anyhow!("GTK init failed: {}", e))?;

    let event_loop = EventLoop::new()?;

    let status_item = MenuItem::new(
        format!("{} - {}:{}", friendly_name, local_ip, http_port),
        false,
        None,
    );
    let show_item = MenuItem::new("Show Status", true, None);
    let stop_item = MenuItem::new("Stop", true, None);
    let quit_item = MenuItem::new("Quit", true, None);
    let show_id = show_item.id().clone();
    let stop_id = stop_item.id().clone();
    let quit_id = quit_item.id().clone();

    let menu = Menu::with_items(&[
        &status_item,
        &PredefinedMenuItem::separator(),
        &show_item,
        &stop_item,
        &quit_item,
    ])?;

    // Load 📺 icon
    let icon = load_icon();

    // Hover-only text: tooltip covers Windows/macOS, Linux StatusNotifier
    // ignores tooltip so Title is set directly below (with_title would
    // leave a persistent label next to the icon).
    const TRAY_HOVER_TEXT: &str = "PlayPnP - DLNA MediaRenderer";

    let _tray = TrayIconBuilder::new()
        .with_id("playpnp")
        .with_menu(Box::new(menu))
        .with_tooltip(TRAY_HOVER_TEXT)
        .with_icon(icon)
        .build()?;

    // tray-icon leaves Title unset, so hosts fall back to its
    // "tray-icon tray app {id}" AppIndicator id on hover.
    #[cfg(target_os = "linux")]
    unsafe {
        (*(_tray.app_indicator() as *mut libappindicator::AppIndicator))
            .set_title(TRAY_HOVER_TEXT)
    }

    // Channels
    let menu_channel = MenuEvent::receiver();
    let tray_channel = TrayIconEvent::receiver();

    event_loop.set_control_flow(ControlFlow::WaitUntil(
        std::time::Instant::now() + std::time::Duration::from_millis(300),
    ));

    #[allow(deprecated)]
    event_loop.run(move |event, elwt| {
        elwt.set_control_flow(ControlFlow::WaitUntil(
            std::time::Instant::now() + std::time::Duration::from_millis(300),
        ));

        // libayatana-appindicator registers its StatusNotifierItem over
        // async GDBus calls, which only complete when the GLib default
        // context is iterated. winit never runs one, so pump it here —
        // otherwise the tray item never appears on StatusNotifier hosts
        // (e.g. the Noctalia bar) even though the daemon is healthy.
        #[cfg(target_os = "linux")]
        {
            let ctx = gtk::glib::MainContext::default();
            while ctx.iteration(false) {}
        }

        // Check if IPC stop or external shutdown fired
        if *shutdown_rx.borrow() {
            elwt.exit();
            return;
        }

        if let winit::event::Event::NewEvents(winit::event::StartCause::Init) = event {
            tracing::info!("Tray icon running");
        }

        if let Ok(menu_event) = menu_channel.try_recv() {
            if menu_event.id == show_id {
                tracing::info!("Tray: Show Status clicked");
                let url = format!("http://{}:{}/", local_ip, http_port);
                crate::platform::open_url(&url);
            } else if menu_event.id == stop_id || menu_event.id == quit_id {
                tracing::info!("Tray: Stop/Quit clicked");
                let _ = shutdown_tx.send(true);
                elwt.exit();
            }
        }
        if let Ok(tray_event) = tray_channel.try_recv() {
            if let TrayIconEvent::Click { button, .. } = tray_event {
                if button == tray_icon::MouseButton::Left {
                    tracing::debug!("Tray left click");
                }
            }
        }
    })?;

    Ok(())
}

#[cfg(feature = "tray")]
fn load_icon() -> tray_icon::Icon {
    let width = 32;
    let height = 32;
    let mut rgba = vec![0u8; (width * height * 4) as usize];

    let mut set_px = |x: i32, y: i32, r: u8, g: u8, b: u8, a: u8| {
        if (0..32).contains(&x) && (0..32).contains(&y) {
            let idx = ((y * 32 + x) * 4) as usize;
            rgba[idx] = r;
            rgba[idx + 1] = g;
            rgba[idx + 2] = b;
            rgba[idx + 3] = a;
        }
    };

    // 1. Antennas (rabbit ears)
    // Left antenna: from (13, 7) to (6, 0)
    for i in 0..=7 {
        set_px(13 - i, 7 - i, 190, 195, 205, 255);
    }
    set_px(6, 0, 220, 225, 235, 255);
    set_px(5, 0, 220, 225, 235, 255);

    // Right antenna: from (18, 7) to (25, 0)
    for i in 0..=7 {
        set_px(18 + i, 7 - i, 190, 195, 205, 255);
    }
    set_px(25, 0, 220, 225, 235, 255);
    set_px(26, 0, 220, 225, 235, 255);

    // 2. Stand feet (bottom)
    // Left leg
    set_px(6, 27, 80, 85, 95, 255);
    set_px(7, 27, 80, 85, 95, 255);
    set_px(5, 28, 80, 85, 95, 255);
    set_px(6, 28, 80, 85, 95, 255);
    set_px(4, 29, 80, 85, 95, 255);
    set_px(5, 29, 80, 85, 95, 255);

    // Right leg
    set_px(24, 27, 80, 85, 95, 255);
    set_px(25, 27, 80, 85, 95, 255);
    set_px(25, 28, 80, 85, 95, 255);
    set_px(26, 28, 80, 85, 95, 255);
    set_px(26, 29, 80, 85, 95, 255);
    set_px(27, 29, 80, 85, 95, 255);

    // 3. TV Body (Outer Cabinet) - from y=8 to y=26, x=2 to x=29
    for y in 8..=26 {
        for x in 2..=29 {
            // Rounded corners cutouts
            if (x <= 3 && y == 8)
                || (x >= 28 && y == 8)
                || (x <= 3 && y == 26)
                || (x >= 28 && y == 26)
            {
                continue;
            }

            // Outer border
            let is_border = x == 2
                || x == 29
                || y == 8
                || y == 26
                || (x == 3 && (y == 9 || y == 25))
                || (x == 28 && (y == 9 || y == 25));

            if is_border {
                set_px(x, y, 70, 75, 85, 255);
            } else {
                set_px(x, y, 42, 45, 52, 255); // Cabinet dark casing
            }
        }
    }

    // 4. TV Screen bezel & display
    // Bezel border: x=5..=22, y=10..=24
    for y in 10..=24 {
        for x in 5..=22 {
            if (x == 5 && (y == 10 || y == 24)) || (x == 22 && (y == 10 || y == 24)) {
                set_px(x, y, 42, 45, 52, 255);
                continue;
            }

            let is_bezel = x == 5 || x == 22 || y == 10 || y == 24;
            if is_bezel {
                set_px(x, y, 22, 24, 28, 255); // Dark inner bezel
            } else {
                // TV Screen - glowing vibrant blue / cyan CRT
                if y == 11 && (7..=15).contains(&x) {
                    set_px(x, y, 160, 225, 255, 255); // Top glare reflection
                } else if y == 12 && (7..=12).contains(&x) {
                    set_px(x, y, 110, 195, 255, 255); // Glare soft edge
                } else if y >= 21 {
                    set_px(x, y, 25, 120, 210, 255); // Deep lower blue
                } else {
                    set_px(x, y, 40, 155, 240, 255); // Vibrant CRT blue
                }
            }
        }
    }

    // 5. Right Control Panel (Knobs & Speaker)
    // Upper Knob at (26, 12)
    set_px(25, 12, 185, 190, 200, 255);
    set_px(26, 12, 240, 245, 255, 255);
    set_px(27, 12, 185, 190, 200, 255);
    set_px(26, 11, 185, 190, 200, 255);
    set_px(26, 13, 185, 190, 200, 255);

    // Lower Knob at (26, 16)
    set_px(25, 16, 185, 190, 200, 255);
    set_px(26, 16, 240, 245, 255, 255);
    set_px(27, 16, 185, 190, 200, 255);
    set_px(26, 15, 185, 190, 200, 255);
    set_px(26, 17, 185, 190, 200, 255);

    // Speaker grille lines at y=20, y=22
    for x in 24..=27 {
        set_px(x, 20, 20, 22, 26, 255);
        set_px(x, 22, 20, 22, 26, 255);
    }

    tray_icon::Icon::from_rgba(rgba, width, height)
        .unwrap_or_else(|_| tray_icon::Icon::from_rgba(vec![0, 0, 0, 255], 1, 1).unwrap())
}

#[cfg(not(feature = "tray"))]
pub fn run_tray(
    _shutdown_tx: tokio::sync::watch::Sender<bool>,
    _shutdown_rx: tokio::sync::watch::Receiver<bool>,
    _friendly_name: String,
    _http_port: u16,
    _local_ip: std::net::IpAddr,
) -> anyhow::Result<()> {
    anyhow::bail!("tray feature not enabled")
}
