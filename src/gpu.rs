use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use eframe::wgpu;

use crate::shared::{GpuSlot, Mode, ModeCell, PciAddress, Shared};

/// Side length of the off-screen image the load draws into.
const TARGET_SIZE: u32 = 2048;
/// Side length of the noise texture the load reads from.
const NOISE_SIZE: u32 = 2048;
/// Target duration of a single submission. Long enough that Windows counts
/// the GPU as fully busy (shorter ones get under-reported in Task Manager),
/// short enough that the desktop compositor still gets regular turns.
const TARGET_SUBMIT: Duration = Duration::from_millis(14);
/// Submissions kept queued so the GPU never idles between them.
const QUEUE_DEPTH: usize = 2;
/// Length of the busy part of one Half-mode cycle.
const HALF_BURST: Duration = Duration::from_millis(50);
const IDLE_POLL: Duration = Duration::from_millis(50);
const MAX_LAYERS: u32 = 1 << 20;

/// Many blended full-screen layers, each doing a run of texture reads
/// followed by arithmetic. Measured on an RTX 3070 Ti this mix draws the
/// most power: it keeps the shader cores, texture units, blending and video
/// memory all busy. Arithmetic alone leaves ~35 W unused, and scattered
/// texture reads stall the cores and draw less still.
const SHADER: &str = r#"
struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) layer: f32,
}

@group(0) @binding(0) var noise: texture_2d<f32>;
@group(0) @binding(1) var noise_sampler: sampler;

@vertex
fn vertex(
    @builtin(vertex_index) vertex: u32,
    @builtin(instance_index) instance: u32,
) -> VertexOutput {
    // One triangle that covers the whole target.
    let corner = vec2<f32>(f32((vertex << 1u) & 2u), f32(vertex & 2u));
    var out: VertexOutput;
    out.position = vec4<f32>(corner * 2.0 - 1.0, 0.0, 1.0);
    out.uv = corner;
    out.layer = f32(instance);
    return out;
}

@fragment
fn fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    var uv = in.uv * (1.0 + in.layer * 0.37) + vec2<f32>(in.layer * 0.113, in.layer * 0.071);
    var sum = vec4<f32>(0.0);
    for (var k = 0; k < 8; k = k + 1) {
        sum = sum + textureSampleLevel(noise, noise_sampler, uv, 0.0);
        uv = uv + vec2<f32>(0.0007, 0.0003);
    }
    var v = sum.x + in.uv.x;
    for (var k = 0; k < 32; k = k + 1) {
        v = fract(sin(v * 12.9898 + f32(k)) * 43758.5453) + cos(v);
    }
    return vec4<f32>(sum.rgb * 0.125 + v * 0.001, 0.25);
}
"#;

/// Find every hardware graphics card, most powerful kind first.
pub fn discover() -> Vec<(wgpu::Adapter, GpuSlot)> {
    catch_unwind(|| {
        let instance = wgpu::Instance::default();
        let mut adapters =
            pollster::block_on(instance.enumerate_adapters(wgpu::Backends::PRIMARY));
        // Software renderers would just be a second CPU load.
        adapters.retain(|adapter| adapter.get_info().device_type != wgpu::DeviceType::Cpu);

        // Each graphics API lists the same cards again, and they don't
        // describe them identically, so take the cards from one API only:
        // the native one (DirectX 12 on Windows, Vulkan elsewhere) if it
        // found any, otherwise whichever did.
        let native = if cfg!(windows) {
            wgpu::Backend::Dx12
        } else {
            wgpu::Backend::Vulkan
        };
        let has_native = adapters.iter().any(|a| a.get_info().backend == native);
        let backend = if has_native {
            Some(native)
        } else {
            adapters.first().map(|a| a.get_info().backend)
        };
        adapters.retain(|adapter| Some(adapter.get_info().backend) == backend);

        adapters.sort_by_key(|adapter| match adapter.get_info().device_type {
            wgpu::DeviceType::DiscreteGpu => 0,
            wgpu::DeviceType::IntegratedGpu => 1,
            _ => 2,
        });

        // Even one API can list a card more than once.
        let mut seen = Vec::new();
        let mut found = Vec::new();
        for adapter in adapters {
            let info = adapter.get_info();
            let key = if info.device_pci_bus_id.is_empty() {
                format!("{:x}:{:x}:{}", info.vendor, info.device, info.name)
            } else {
                info.device_pci_bus_id.clone()
            };
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            let slot = GpuSlot {
                name: info.name,
                vendor: info.vendor,
                address: PciAddress::parse(&info.device_pci_bus_id),
                integrated: info.device_type == wgpu::DeviceType::IntegratedGpu,
                mode: ModeCell::new(),
                error: std::sync::Mutex::new(None),
            };
            found.push((adapter, slot));
        }
        found
    })
    .unwrap_or_default()
}

/// Start the load thread for the card at `index` in `shared.gpus`.
pub fn spawn(shared: Arc<Shared>, index: usize, adapter: wgpu::Adapter) {
    thread::Builder::new()
        .name(format!("gpu-heater-{index}"))
        .spawn(move || {
            let slot = &shared.gpus[index];
            let result = catch_unwind(AssertUnwindSafe(|| run(slot, &adapter)));
            let message = match result {
                Ok(Ok(())) => return,
                Ok(Err(e)) => e,
                Err(_) => "GPU worker crashed".to_owned(),
            };
            slot.mode.set(Mode::Off);
            *slot.error.lock().unwrap() = Some(message);
        })
        .expect("failed to spawn GPU worker");
}

fn run(slot: &GpuSlot, adapter: &wgpu::Adapter) -> Result<(), String> {
    // This thread stays at normal priority: it only hands work to the card
    // and waits, but if it ran at the CPU heaters' low priority they would
    // starve it and the card would sit idle.
    let mut burner = Burner::new(adapter)?;

    loop {
        match slot.mode.get() {
            Mode::Off => thread::sleep(IDLE_POLL),
            Mode::Full => {
                burner.calibrate()?;
                burner.burn_while(|| slot.mode.get() == Mode::Full)?;
            }
            Mode::Half => {
                burner.calibrate()?;
                let start = Instant::now();
                burner.burn_while(|| start.elapsed() < HALF_BURST)?;
                // Idle for exactly as long as the burst really took.
                thread::sleep(start.elapsed());
            }
        }
    }
}

struct Burner {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    target_view: wgpu::TextureView,
    /// Blended layers drawn per submission, sized by `calibrate`.
    layers: u32,
    calibrated: bool,
}

impl Burner {
    fn new(adapter: &wgpu::Adapter) -> Result<Self, String> {
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("heater"),
            ..Default::default()
        }))
        .map_err(|e| format!("Could not open GPU: {e}"))?;

        let target_format = wgpu::TextureFormat::Rgba16Float;
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("heater target"),
            size: wgpu::Extent3d {
                width: TARGET_SIZE,
                height: TARGET_SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: target_format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());

        let noise_extent = wgpu::Extent3d {
            width: NOISE_SIZE,
            height: NOISE_SIZE,
            depth_or_array_layers: 1,
        };
        let noise = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("heater noise"),
            size: noise_extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            noise.as_image_copy(),
            &noise_pixels(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(NOISE_SIZE * 4),
                rows_per_image: Some(NOISE_SIZE),
            },
            noise_extent,
        );
        let noise_view = noise.create_view(&wgpu::TextureViewDescriptor::default());
        let noise_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("heater noise"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("heater"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("heater"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vertex"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fragment"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            multiview_mask: None,
            cache: None,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("heater"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&noise_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&noise_sampler),
                },
            ],
        });

        Ok(Self {
            device,
            queue,
            pipeline,
            bind_group,
            target_view,
            layers: 1,
            calibrated: false,
        })
    }

    fn submit(&self) -> wgpu::SubmissionIndex {
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.target_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.draw(0..3, 0..self.layers);
        }
        self.queue.submit(Some(encoder.finish()))
    }

    fn wait(&self, index: wgpu::SubmissionIndex) -> Result<(), String> {
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(index),
                timeout: None,
            })
            .map(|_| ())
            .map_err(|e| format!("GPU stopped responding: {e}"))
    }

    /// Pick the number of layers so that one submission takes roughly
    /// `TARGET_SUBMIT` on this particular GPU.
    fn calibrate(&mut self) -> Result<(), String> {
        if self.calibrated {
            return Ok(());
        }
        self.layers = 4;
        // The first submission includes one-off driver warm-up, so discard it.
        self.wait(self.submit())?;
        loop {
            let start = Instant::now();
            self.wait(self.submit())?;
            let took = start.elapsed();
            let ratio = TARGET_SUBMIT.as_secs_f64() / took.as_secs_f64().max(1e-6);
            let close_enough = took >= TARGET_SUBMIT.mul_f32(0.5);
            // Far off: take big steps. Close: one proportional correction.
            let scale = if close_enough { ratio } else { ratio.clamp(1.5, 8.0) };
            self.layers = ((f64::from(self.layers) * scale).round() as u32).clamp(1, MAX_LAYERS);
            if close_enough || self.layers >= MAX_LAYERS {
                break;
            }
        }
        self.calibrated = true;
        Ok(())
    }

    /// Submit work back to back while `keep_going` returns true, then block
    /// until the GPU has drained everything that was queued.
    fn burn_while(&self, keep_going: impl Fn() -> bool) -> Result<(), String> {
        let mut in_flight = VecDeque::with_capacity(QUEUE_DEPTH + 1);
        while keep_going() {
            in_flight.push_back(self.submit());
            if in_flight.len() > QUEUE_DEPTH {
                self.wait(in_flight.pop_front().unwrap())?;
            }
        }
        if let Some(last) = in_flight.pop_back() {
            self.wait(last)?;
        }
        Ok(())
    }
}

/// Random RGBA pixels for the noise texture.
fn noise_pixels() -> Vec<u8> {
    let mut state: u32 = 0x9E37_79B9;
    let mut pixels = Vec::with_capacity((NOISE_SIZE * NOISE_SIZE * 4) as usize);
    for _ in 0..NOISE_SIZE * NOISE_SIZE {
        // xorshift32
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        pixels.extend_from_slice(&state.to_le_bytes());
    }
    pixels
}
