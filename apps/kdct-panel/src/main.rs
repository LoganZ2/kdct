use anyhow::Result;
use image::RgbaImage;
use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;
use tray_icon::{Icon, TrayIconBuilder};

enum StatusUpdate {
    Connected,
    Disconnected,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config_path = config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("kdct-client.toml");

    let connected = Arc::new(AtomicBool::new(false));

    let icon_red = make_tray_icon(220, 60, 60);
    let icon_green = make_tray_icon(60, 200, 60);

    // Build menu
    let menu = Menu::new();
    let start_item = MenuItem::new("Start Client", true, None);
    let stop_item = MenuItem::new("Stop Client", true, None);
    let edit_item = MenuItem::new("Edit Config...", true, None);
    let quit_item = MenuItem::new("Quit", true, None);

    menu.append(&start_item)?;
    menu.append(&stop_item)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&edit_item)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&quit_item)?;

    // Build tray icon
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("kdct — disconnected")
        .with_icon(icon_red.clone())
        .build()?;

    let tray_icon_red = icon_red;
    let tray_icon_green = icon_green;

    type ShutdownTx = Option<oneshot::Sender<()>>;
    let shutdown_tx: Arc<Mutex<ShutdownTx>> = Arc::new(Mutex::new(None));

    // Channel for status updates from background thread
    let (status_tx, status_rx) = std_mpsc::channel::<StatusUpdate>();

    // Menu event loop (blocking, on main thread)
    loop {
        // Process menu events
        if let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == start_item.id() {
                if !connected.load(Ordering::SeqCst) {
                    let (tx, rx) = oneshot::channel();
                    *shutdown_tx.lock().unwrap() = Some(tx);

                    let path = config_path.clone();
                    let tx = status_tx.clone();

                    std::thread::spawn(move || {
                        let rt = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .unwrap();

                        rt.block_on(async {
                            let _ = tx.send(StatusUpdate::Connected);
                            let result = run_client(path, rx).await;
                            let _ = tx.send(StatusUpdate::Disconnected);

                            if let Err(e) = result {
                                tracing::error!("Client stopped: {:#}", e);
                            }
                        });
                    });
                }
            }

            if event.id == stop_item.id() {
                if let Some(tx) = shutdown_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
            }

            if event.id == edit_item.id() {
                if let Err(e) = open::that(config_path.as_os_str()) {
                    tracing::error!("Failed to open config: {:#}", e);
                }
            }

            if event.id == quit_item.id() {
                if let Some(tx) = shutdown_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                std::process::exit(0);
            }
        }

        // Process status updates from background threads
        while let Ok(update) = status_rx.try_recv() {
            match update {
                StatusUpdate::Connected => {
                    connected.store(true, Ordering::SeqCst);
                    tray.set_icon(Some(tray_icon_green.clone())).ok();
                    tray.set_tooltip(Some("kdct — connected")).ok();
                }
                StatusUpdate::Disconnected => {
                    connected.store(false, Ordering::SeqCst);
                    tray.set_icon(Some(tray_icon_red.clone())).ok();
                    tray.set_tooltip(Some("kdct — disconnected")).ok();
                }
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

async fn run_client(config_path: PathBuf, shutdown: oneshot::Receiver<()>) -> Result<()> {
    let _ = rathole::Config::from_file(&config_path).await?;

    let (tx, shutdown_rx) = tokio::sync::broadcast::channel::<bool>(1);

    let handle = tokio::spawn(async move {
        rathole::run(
            rathole::cli::Cli {
                config_path: Some(config_path),
                server: false,
                client: true,
                genkey: None,
            },
            shutdown_rx,
        )
        .await
    });

    let shutdown_join = tokio::spawn(async move { shutdown.await });

    tokio::select! {
        _ = shutdown_join => {
            let _ = tx.send(true);
            drop(tx); // drop broadcast sender so client task can exit
        }
        _ = handle => {
            // Client exited on its own — broadcast sender dropped, all good
        }
    }

    Ok(())
}

/// Generate a 32x32 tray icon — a filled circle with the given RGB color.
fn make_tray_icon(r: u8, g: u8, b: u8) -> Icon {
    let size = 32u32;
    let mut img = RgbaImage::new(size, size);
    let cx = (size / 2) as f32;
    let cy = (size / 2) as f32;
    let radius = (size / 2 - 2) as f32;

    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            if (dx * dx + dy * dy).sqrt() <= radius {
                img.put_pixel(x, y, image::Rgba([r, g, b, 255]));
            } else {
                img.put_pixel(x, y, image::Rgba([0, 0, 0, 0]));
            }
        }
    }

    let (width, height) = (img.width() as _, img.height() as _);
    Icon::from_rgba(img.into_raw(), width, height).expect("Failed to create icon")
}

fn config_dir() -> Option<PathBuf> {
    let dir = dirs::config_dir()?;
    let kdct_dir = dir.join("kdct");
    std::fs::create_dir_all(&kdct_dir).ok();
    Some(kdct_dir)
}
