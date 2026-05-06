use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::window::Window;

pub struct EditorState {
    window: Arc<Window>,
    egui_winit: egui_winit::State,
    egui_ctx: egui::Context,
    renderer: egui_wgpu::Renderer,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface_config: wgpu::SurfaceConfiguration,
    // Form state
    server_addr: String,
    token: String,
    service_name: String,
    service_local_addr: String,
    dirty: bool,
    config_path: PathBuf,
}

impl EditorState {
    pub fn new(event_loop: &ActiveEventLoop, config_path: &Path) -> Result<Self> {
        let win_attr = Window::default_attributes()
            .with_title("KDCT Config Editor")
            .with_inner_size(winit::dpi::LogicalSize::new(480.0, 400.0))
            .with_resizable(true);
        let window = Arc::new(
            event_loop
                .create_window(win_attr)
                .context("Failed to create editor window")?,
        );
        // Bring window to front
        window.focus_window();

        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });

        let surface = instance
            .create_surface(window.clone())
            .context("Failed to create wgpu surface")?;

        let adapter =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            }))
            .context("Failed to find wgpu adapter")?;

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("kdct-editor"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::default(),
            },
            None,
        ))
        .context("Failed to create wgpu device")?;

        let size = window.inner_size();
        let surface_caps = surface.get_capabilities(&adapter);
        let surface_format = surface_caps
            .formats
            .iter()
            .find(|f| f.is_srgb())
            .copied()
            .unwrap_or(surface_caps.formats[0]);

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width: size.width,
            height: size.height,
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode: surface_caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &surface_config);

        let egui_ctx = egui::Context::default();
        let egui_winit = egui_winit::State::new(
            egui_ctx.clone(),
            egui::ViewportId::default(),
            &window,
            None,
            None,
            None,
        );
        let renderer = egui_wgpu::Renderer::new(&device, surface_format, None, 1, false);

        let (server_addr, token, service_name, service_local_addr) = load_config(config_path);

        Ok(EditorState {
            window,
            egui_winit,
            egui_ctx,
            renderer,
            surface,
            device,
            queue,
            surface_config,
            server_addr,
            token,
            service_name,
            service_local_addr,
            dirty: false,
            config_path: config_path.to_path_buf(),
        })
    }

    pub fn window(&self) -> &Window {
        &self.window
    }

    pub fn dirty(&self) -> bool {
        self.dirty
    }

    pub fn on_window_event(&mut self, event: &WindowEvent) -> bool {
        let resp = self.egui_winit.on_window_event(&self.window, event);
        resp.consumed
    }

    pub fn render(&mut self) -> Result<()> {
        let raw_input = self.egui_winit.take_egui_input(&self.window);

        // Capture form state before borrowing self
        let mut server_addr = self.server_addr.clone();
        let mut token = self.token.clone();
        let mut service_name = self.service_name.clone();
        let mut service_local_addr = self.service_local_addr.clone();
        let mut dirty = self.dirty;
        let mut saved = false;

        let full_output = self.egui_ctx.run(raw_input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.heading("KDCT Client Configuration");
                ui.add_space(10.0);

                ui.horizontal(|ui| {
                    ui.label("Server Address:");
                    if ui.text_edit_singleline(&mut server_addr).changed() {
                        dirty = true;
                    }
                });

                ui.horizontal(|ui| {
                    ui.label("Default Token:");
                    if ui
                        .add(egui::TextEdit::singleline(&mut token).password(true))
                        .changed()
                    {
                        dirty = true;
                    }
                });

                ui.add_space(10.0);
                ui.separator();
                ui.heading("Service");

                ui.horizontal(|ui| {
                    ui.label("Name:");
                    if ui.text_edit_singleline(&mut service_name).changed() {
                        dirty = true;
                    }
                });

                ui.horizontal(|ui| {
                    ui.label("Local Address:");
                    if ui.text_edit_singleline(&mut service_local_addr).changed() {
                        dirty = true;
                    }
                });

                ui.add_space(20.0);

                ui.horizontal(|ui| {
                    if ui
                        .add_sized([120.0, 32.0], egui::Button::new("Save"))
                        .clicked()
                    {
                        dirty = false;
                        saved = true;
                    }

                    if dirty {
                        ui.colored_label(egui::Color32::YELLOW, "Unsaved changes");
                    } else if saved {
                        ui.colored_label(
                            egui::Color32::GREEN,
                            "Saved. File watcher will hot-reload the client.",
                        );
                    }
                });
            });
        });

        // Write back form state
        self.server_addr = server_addr;
        self.token = token;
        self.service_name = service_name;
        self.service_local_addr = service_local_addr;

        let was_dirty = self.dirty;
        self.dirty = dirty;

        // If Save was clicked, persist to disk immediately
        if was_dirty && !self.dirty && saved {
            let path = self.config_path.clone();
            if let Err(e) = self.save(&path) {
                tracing::error!("Failed to save config: {:#}", e);
            }
            // Send a dummy event to trigger config watcher reload
            // (rathole's notify watcher should pick up the file change automatically)
            tracing::info!("Config saved — file watcher will trigger hot-reload");
        }

        self.egui_winit
            .handle_platform_output(&self.window, full_output.platform_output);

        let paint_jobs =
            self.egui_ctx
                .tessellate(full_output.shapes, full_output.pixels_per_point);
        let screen_descriptor = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [
                self.surface_config.width,
                self.surface_config.height,
            ],
            pixels_per_point: self.window.scale_factor() as f32,
        };

        for (id, delta) in &full_output.textures_delta.set {
            self.renderer
                .update_texture(&self.device, &self.queue, *id, delta);
        }

        let output = self.surface.get_current_texture()?;
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("kdct-editor-encoder"),
            });

        self.renderer.update_buffers(
            &self.device,
            &self.queue,
            &mut encoder,
            &paint_jobs,
            &screen_descriptor,
        );

        // SAFETY: encoder is alive for the duration of the render pass.
        // The render pass is dropped before encoder.finish().
        {
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("kdct-editor-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.12,
                            g: 0.12,
                            b: 0.15,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            // Extend lifetime to 'static — rpass is dropped before encoder.finish()
            let rpass: &mut wgpu::RenderPass<'static> =
                unsafe { std::mem::transmute(&mut rpass) };
            self.renderer
                .render(rpass, &paint_jobs, &screen_descriptor);
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        output.present();

        for id in &full_output.textures_delta.free {
            self.renderer.free_texture(id);
        }

        Ok(())
    }

    pub fn save(&self, config_path: &Path) -> Result<()> {
        let config = format!(
            r#"[client]
remote_addr = "{}"
default_token = "{}"

[client.transport]
type = "tcp"

[client.services.{}]
type = "tcp"
local_addr = "{}"
token = "{}"
"#,
            self.server_addr,
            self.token,
            self.service_name,
            self.service_local_addr,
            self.token,
        );

        std::fs::write(config_path, config)?;
        tracing::info!("Config saved to {}", config_path.display());
        Ok(())
    }
}

fn load_config(path: &Path) -> (String, String, String, String) {
    let default = || {
        (
            "127.0.0.1:2334".to_string(),
            "test-token".to_string(),
            "web".to_string(),
            "127.0.0.1:8888".to_string(),
        )
    };

    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return default(),
    };

    let table: toml::Table = match toml::from_str(&content) {
        Ok(t) => t,
        Err(_) => return default(),
    };

    let client = table.get("client").and_then(|v| v.as_table());
    let server_addr = client
        .and_then(|c| c.get("remote_addr"))
        .and_then(|v| v.as_str())
        .unwrap_or("127.0.0.1:2334")
        .to_string();

    let token = client
        .and_then(|c| c.get("default_token"))
        .and_then(|v| v.as_str())
        .unwrap_or("test-token")
        .to_string();

    let services = client
        .and_then(|c| c.get("services"))
        .and_then(|v| v.as_table());

    let (name, local_addr) = services
        .and_then(|svc| {
            svc.iter().next().map(|(n, v)| {
                let addr = v
                    .get("local_addr")
                    .and_then(|a| a.as_str())
                    .unwrap_or("127.0.0.1:8888");
                (n.clone(), addr.to_string())
            })
        })
        .unwrap_or_else(|| ("web".to_string(), "127.0.0.1:8888".to_string()));

    (server_addr, token, name, local_addr)
}
