use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use eframe::wgpu;

use crate::shared::{GpuState, Mode, Shared, lower_thread_priority};

/// Number of f32 elements the shader works over (one invocation each).
const ELEMENTS: u32 = 1 << 20;
const WORKGROUP_SIZE: u32 = 64;
/// Target duration of a single dispatch. Short enough that the desktop
/// compositor still gets regular turns on the GPU.
const TARGET_DISPATCH: Duration = Duration::from_millis(6);
/// Submissions kept queued so the GPU never idles between dispatches.
const QUEUE_DEPTH: usize = 2;
/// Length of the busy part of one Half-mode cycle.
const HALF_BURST: Duration = Duration::from_millis(50);
const IDLE_POLL: Duration = Duration::from_millis(50);
const MAX_ITERS: u32 = 1 << 22;

const SHADER: &str = r#"
struct Params {
    iters: u32,
    pad0: u32,
    pad1: u32,
    pad2: u32,
}

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read_write> data: array<f32>;

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = id.x;
    if (i >= arrayLength(&data)) {
        return;
    }
    var v = data[i] + f32(i) * 0.000001;
    for (var k = 0u; k < params.iters; k = k + 1u) {
        v = fract(sin(v * 12.9898 + f32(k)) * 43758.5453) + cos(v);
    }
    data[i] = v;
}
"#;

pub fn spawn(shared: Arc<Shared>) {
    thread::Builder::new()
        .name("gpu-heater".into())
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| run(&shared)));
            let message = match result {
                Ok(Ok(())) => return,
                Ok(Err(e)) => e,
                Err(_) => "GPU worker crashed".to_owned(),
            };
            shared.gpu.set(Mode::Off);
            *shared.gpu_state.lock().unwrap() = GpuState::Failed(message);
        })
        .expect("failed to spawn GPU worker");
}

fn run(shared: &Shared) -> Result<(), String> {
    lower_thread_priority();
    let mut burner = Burner::new()?;
    *shared.gpu_state.lock().unwrap() = GpuState::Ready(burner.name.clone());

    loop {
        match shared.gpu.get() {
            Mode::Off => thread::sleep(IDLE_POLL),
            Mode::Full => {
                burner.calibrate()?;
                burner.burn_while(|| shared.gpu.get() == Mode::Full)?;
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
    name: String,
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::ComputePipeline,
    bind_group: wgpu::BindGroup,
    params: wgpu::Buffer,
    calibrated: bool,
}

impl Burner {
    fn new() -> Result<Self, String> {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
            ..Default::default()
        }))
        .map_err(|e| format!("No GPU found: {e}"))?;

        let info = adapter.get_info();
        if info.device_type == wgpu::DeviceType::Cpu {
            return Err(format!("No hardware GPU found (only {})", info.name));
        }

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("heater"),
            ..Default::default()
        }))
        .map_err(|e| format!("Could not open GPU: {e}"))?;

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("heater"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("heater"),
            layout: None,
            module: &shader,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });

        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("params"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let data = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("data"),
            size: u64::from(ELEMENTS) * 4,
            usage: wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("heater"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: params.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: data.as_entire_binding(),
                },
            ],
        });

        Ok(Self {
            name: info.name,
            device,
            queue,
            pipeline,
            bind_group,
            params,
            calibrated: false,
        })
    }

    fn set_iters(&self, iters: u32) {
        let mut bytes = [0u8; 16];
        bytes[..4].copy_from_slice(&iters.to_le_bytes());
        self.queue.write_buffer(&self.params, 0, &bytes);
    }

    fn submit(&self) -> wgpu::SubmissionIndex {
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.dispatch_workgroups(ELEMENTS / WORKGROUP_SIZE, 1, 1);
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

    /// Scale the per-dispatch workload so one dispatch takes roughly
    /// `TARGET_DISPATCH` on this particular GPU.
    fn calibrate(&mut self) -> Result<(), String> {
        if self.calibrated {
            return Ok(());
        }
        let mut iters: u32 = 16;
        // The first dispatch includes one-off driver warm-up, so discard it.
        self.set_iters(iters);
        self.wait(self.submit())?;
        loop {
            self.set_iters(iters);
            let start = Instant::now();
            self.wait(self.submit())?;
            let took = start.elapsed();
            if took >= TARGET_DISPATCH.mul_f32(0.7) || iters >= MAX_ITERS {
                break;
            }
            let scale = (TARGET_DISPATCH.as_secs_f64() / took.as_secs_f64().max(1e-6))
                .clamp(1.5, 8.0);
            iters = ((f64::from(iters) * scale) as u32).min(MAX_ITERS);
        }
        self.calibrated = true;
        Ok(())
    }

    /// Run dispatches back to back while `keep_going` returns true, then
    /// block until the GPU has drained everything that was queued.
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
