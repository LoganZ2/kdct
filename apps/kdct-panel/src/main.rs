use anyhow::Result;
use image::RgbaImage;
use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};

mod editor;

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

    let event_loop = EventLoop::new()?;

    let config_path = config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("kdct-client.toml");

    let connected = Arc::new(AtomicBool::new(false));

    let icon_red = make_tray_icon(220, 60, 60);
    let icon_green = make_tray_icon(60, 200, 60);

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

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("kdct — disconnected")
        .with_icon(icon_red.clone())
        .build()?;

    type ShutdownTx = Option<oneshot::Sender<()>>;
    let shutdown_tx: Arc<Mutex<ShutdownTx>> = Arc::new(Mutex::new(None));
    let (status_tx, status_rx) = std_mpsc::channel::<StatusUpdate>();

    let mut app = PanelApp {
        tray,
        icon_red,
        icon_green,
        connected,
        shutdown_tx,
        status_rx,
        config_path,
        start_item_id: start_item.id().clone(),
        stop_item_id: stop_item.id().clone(),
        edit_item_id: edit_item.id().clone(),
        quit_item_id: quit_item.id().clone(),
        status_tx,
        editor: None,
    };

    event_loop.run_app(&mut app)?;

    Ok(())
}

struct PanelApp {
    tray: TrayIcon,
    icon_red: Icon,
    icon_green: Icon,
    connected: Arc<AtomicBool>,
    shutdown_tx: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    status_rx: std_mpsc::Receiver<StatusUpdate>,
    config_path: PathBuf,
    start_item_id: muda::MenuId,
    stop_item_id: muda::MenuId,
    edit_item_id: muda::MenuId,
    quit_item_id: muda::MenuId,
    status_tx: std_mpsc::Sender<StatusUpdate>,
    editor: Option<editor::EditorState>,
}

impl ApplicationHandler for PanelApp {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        window_id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        // Forward to editor if active
        if let Some(ref mut editor) = &mut self.editor {
            if editor.window().id() == window_id {
                let _ = editor.on_window_event(&event);
                match event {
                    WindowEvent::CloseRequested => {
                        if editor.dirty() {
                            // Save before closing
                            if let Err(e) = editor.save(&self.config_path) {
                                tracing::error!("Failed to save config: {:#}", e);
                            }
                        }
                        self.editor = None;
                    }
                    WindowEvent::RedrawRequested => {
                        editor.render().ok();
                    }
                    _ => {}
                }
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Process tray menu events
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == self.start_item_id {
                if !self.connected.load(Ordering::SeqCst) {
                    let (tx, rx) = oneshot::channel();
                    *self.shutdown_tx.lock().unwrap() = Some(tx);

                    let path = self.config_path.clone();
                    let tx = self.status_tx.clone();

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

            if event.id == self.stop_item_id {
                if let Some(tx) = self.shutdown_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
            }

            if event.id == self.edit_item_id {
                // Open editor window (or refocus if already open)
                if self.editor.is_none() {
                    match editor::EditorState::new(
                        event_loop,
                        &self.config_path,
                    ) {
                        Ok(ed) => self.editor = Some(ed),
                        Err(e) => tracing::error!("Failed to open editor: {:#}", e),
                    }
                }
            }

            if event.id == self.quit_item_id {
                if let Some(tx) = self.shutdown_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                event_loop.exit();
                return;
            }
        }

        // Process status updates
        while let Ok(update) = self.status_rx.try_recv() {
            match update {
                StatusUpdate::Connected => {
                    self.connected.store(true, Ordering::SeqCst);
                    self.tray.set_icon(Some(self.icon_green.clone())).ok();
                    self.tray.set_tooltip(Some("kdct — connected")).ok();
                }
                StatusUpdate::Disconnected => {
                    self.connected.store(false, Ordering::SeqCst);
                    self.tray.set_icon(Some(self.icon_red.clone())).ok();
                    self.tray.set_tooltip(Some("kdct — disconnected")).ok();
                }
            }
        }

        // Request redraw if editor is open
        if let Some(ref editor) = &self.editor {
            editor.window().request_redraw();
        }

        event_loop.set_control_flow(if self.editor.is_some() {
            ControlFlow::Poll
        } else {
            ControlFlow::Wait
        });
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
            },
            shutdown_rx,
        )
        .await
    });

    let shutdown_join = tokio::spawn(async move { shutdown.await });

    tokio::select! {
        _ = shutdown_join => {
            let _ = tx.send(true);
        }
        _ = handle => {}
    }

    Ok(())
}

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
