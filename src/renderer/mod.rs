use std::sync::atomic::AtomicUsize;

use anyhow::Result;
use winit::window::Window;

use self::managers::MeshManager;
use self::pipelines::{BasePipelineBuffer, GeometryPipeline, ScreenPipeline, SkyPipeline};
use self::types::{Camera, Texture};

pub mod managers;
pub mod pipelines;
pub mod types;

pub struct Renderer {
    surface: wgpu::Surface,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    size: winit::dpi::PhysicalSize<u32>,

    camera: Camera,
    base_pipeline_buffer: BasePipelineBuffer,
    depth_texture: Texture,
    geometry_pipeline: GeometryPipeline,
    sky_pipeline: SkyPipeline,
    screen_pipeline: ScreenPipeline,
}

impl Renderer {
    pub async fn new(window: &Window) -> Result<Self> {
        let size = window.inner_size();

        let instance = wgpu::Instance::default();
        let surface = unsafe { instance.create_surface(window)? };
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::default(),
                force_fallback_adapter: false,
                compatible_surface: Some(&surface),
            })
            .await
            .ok_or(WindowStateError::AdapterNotFound)?;

        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: None,
                    features: wgpu::Features::default(),
                    limits: wgpu::Limits::default(),
                },
                None,
            )
            .await?;

        let format = wgpu::TextureFormat::Bgra8UnormSrgb;
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width,
            height: size.height,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            view_formats: vec![format],
        };
        surface.configure(&device, &config);

        let mut camera = Camera::new();
        camera.update_projection(config.width as f32 / config.height as f32);

        let base_pipeline_buffer = BasePipelineBuffer::new(&device);

        let depth_texture = Texture::new_depth(&device, &config, "depth_texture");

        let geometry_pipeline = GeometryPipeline::new(&device, &base_pipeline_buffer);
        let sky_pipeline = SkyPipeline::new(&device, &base_pipeline_buffer);
        let screen_pipeline = ScreenPipeline::new(&device, &config);

        // let doge_mesh = {
        //     let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        //         label: Some("mesh_loader"),
        //     });

        //     let meshes = load_scene(
        //         &mut mesh_manager,
        //         &device,
        //         &queue,
        //         &mut encoder,
        //         &resource_id,
        //         include_bytes!("../res/bike.glb"),
        //     )?;

        //     meshes.into_iter().nth(5).unwrap()
        // };

        // // TEMP:
        // let texture = Texture::from_bytes(
        //     &device,
        //     &queue,
        //     include_bytes!("../res/texture.png"),
        //     "texture",
        // )?;
        // let descr =
        //     geometry_pipeline.create_instance_description(&device, &glam::Mat4::IDENTITY, &texture);

        // let doge = Doge {
        //     mesh: doge_mesh,
        //     texture,
        //     descr,
        // };

        Ok(Self {
            surface,
            device,
            queue,
            config,
            size,

            camera,
            base_pipeline_buffer,
            depth_texture,
            geometry_pipeline,
            sky_pipeline,
            screen_pipeline,
        })
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    pub fn size(&self) -> winit::dpi::PhysicalSize<u32> {
        self.size
    }

    pub fn encode_commands(&self, name: &'static str) -> CommandEncoder<'_> {
        let encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some(name) });

        CommandEncoder {
            device: &self.device,
            queue: &self.queue,
            encoder,
        }
    }

    pub fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
        self.size = new_size;
        self.config.width = new_size.width;
        self.config.height = new_size.height;
        self.surface.configure(&self.device, &self.config);
        self.camera
            .update_projection(new_size.width as f32 / new_size.height as f32);
        self.depth_texture = Texture::new_depth(&self.device, &self.config, "depth_texture");
        self.screen_pipeline
            .update_screen_texture(&self.device, &self.config);
    }

    pub fn render(&mut self, time: f32) {
        self.base_pipeline_buffer.update(
            &self.queue,
            &self.camera,
            self.config.width,
            self.config.height,
            time,
        );

        let output = match self.surface.get_current_texture() {
            Ok(frame) => frame,
            Err(_) => {
                self.surface.configure(&self.device, &self.config);
                self.surface
                    .get_current_texture()
                    .expect("failed to get next frame texture")
            }
        };
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("render_command_encoder"),
            });

        let depth = wgpu::RenderPassDepthStencilAttachment {
            view: &self.depth_texture.view,
            depth_ops: Some(wgpu::Operations {
                load: wgpu::LoadOp::Clear(1.0),
                store: true,
            }),
            stencil_ops: None,
        };

        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("geometry_render_pass"),
                color_attachments: &[Some(self.screen_pipeline.render_target())],
                depth_stencil_attachment: Some(depth),
            });
            render_pass.set_bind_group(0, self.base_pipeline_buffer.bind_group(), &[]);

            //self.mesh_manager.buffers().bind(&mut render_pass);
            // self.geometry_pipeline.render(
            //     &mut render_pass,
            //     std::iter::once(&self.doge)
            //         .map(|doge| (self.mesh_manager.get_mesh(doge.mesh.raw()), &doge.descr)),
            // );

            self.sky_pipeline.render(&mut render_pass);
        }

        self.screen_pipeline.render(&mut encoder, &view);

        self.queue.submit(std::iter::once(encoder.finish()));
        output.present();
    }
}

pub struct CommandEncoder<'a> {
    pub device: &'a wgpu::Device,
    pub queue: &'a wgpu::Queue,
    pub encoder: wgpu::CommandEncoder,
}

impl CommandEncoder<'_> {
    pub fn submit(self) {
        self.queue.submit(std::iter::once(self.encoder.finish()));
    }
}

// struct Doge {
//     mesh: MeshHandle,
//     texture: Texture,
//     descr: InstanceDescription,
// }

#[derive(thiserror::Error, Debug)]
enum WindowStateError {
    #[error("No suitable adapter found")]
    AdapterNotFound,
    #[error("Incompatible surface")]
    IncompatibleSurface,
}
