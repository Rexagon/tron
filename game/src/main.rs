use std::sync::Arc;

use anyhow::Result;
use argh::FromArgs;
use winit::event::*;
use winit::event_loop::EventLoopBuilder;
#[cfg(wayland_platform)]
use winit::platform::wayland::EventLoopBuilderExtWayland;
#[cfg(x11_platform)]
use winit::platform::x11::{EventLoopBuilderExtX11, WindowBuilderExtX11, XWindowType};
use winit::window::WindowBuilder;

use renderer::Renderer;

#[cfg(not(any(target_env = "msvc", miri)))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn main() -> Result<()> {
    let app: App = argh::from_env();
    app.run()
}

/// Vulkan rendering experiments
#[derive(FromArgs)]
struct App {
    /// enable profiling server
    #[argh(switch)]
    profiling: bool,

    /// use Vulkan validation layer
    #[argh(switch)]
    vk_validation_layer: bool,

    /// enable X11-specific popup mode
    #[cfg(x11_platform)]
    #[argh(switch)]
    x11_as_popup: bool,

    /// force use X11 window backend
    #[cfg(x11_platform)]
    #[argh(switch)]
    x11_backend: bool,

    /// force use Wayland window backend
    #[cfg(wayland_platform)]
    #[argh(switch)]
    wayland_backend: bool,
}

impl App {
    pub fn run(self) -> Result<()> {
        #[cfg(all(x11_platform, wayland_platform))]
        if self.x11_backend && self.wayland_backend {
            panic!("can't use both X11 and Wayland backends");
        }

        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::builder()
                    .with_default_directive(tracing::Level::INFO.into())
                    .from_env_lossy(),
            )
            .init();

        let _puffin_server = if self.profiling {
            let server_addr = format!("127.0.0.1:{}", puffin_http::DEFAULT_PORT);
            let puffin_server = puffin_http::Server::new(&server_addr).unwrap();
            tracing::info!(server_addr, "started profiling server");
            Some(puffin_server)
        } else {
            None
        };
        profiling::puffin::set_scopes_on(self.profiling);

        let app_name = env!("CARGO_BIN_NAME").to_owned();

        let event_loop = {
            let mut builder = EventLoopBuilder::new();

            #[cfg(x11_platform)]
            if self.x11_backend {
                builder.with_x11();
            }
            #[cfg(wayland_platform)]
            if self.wayland_backend {
                builder.with_wayland();
            }

            builder.build()?
        };

        let window = {
            let mut builder = WindowBuilder::new();
            builder = builder.with_title(app_name);

            #[cfg(x11_platform)]
            if self.x11_as_popup {
                builder =
                    builder.with_x11_window_type(vec![XWindowType::Dialog, XWindowType::Normal]);
            }

            builder.build(&event_loop).map(Arc::new)?
        };

        let mut renderer = Renderer::builder(window.clone())
            .app_version((0, 0, 1))
            .validation_layer(self.vk_validation_layer)
            .build()?;

        // TEMP
        let mesh_handle = {
            let mesh = renderer::Mesh::new(renderer::CubeMeshGenerator::from_size(1.0))
                .with_computed_normals()
                .with_computed_tangents()
                .build()?;

            renderer.state().add_mesh(&mesh)?
        };

        let mut minimized = false;
        let handle_event = {
            let renderer_state = renderer.state().clone();
            move |event, elwt: &winit::event_loop::EventLoopWindowTarget<_>| {
                // elwt.set_control_flow(winit::event_loop::ControlFlow::Poll);

                match event {
                    Event::AboutToWait => window.request_redraw(),
                    Event::WindowEvent { event, .. } => match event {
                        WindowEvent::RedrawRequested if !elwt.exiting() && !minimized => {
                            renderer_state.notify_draw();
                        }
                        WindowEvent::Resized(size) => {
                            minimized = size.width == 0 || size.height == 0;
                        }
                        WindowEvent::CloseRequested => {
                            renderer_state.set_running(false);
                            elwt.exit();
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
        };

        tracing::debug!("event loop started");
        event_loop.run(handle_event)?;
        tracing::debug!("event loop stopped");

        drop(mesh_handle);

        renderer.cleanup()?;

        Ok(())
    }
}
