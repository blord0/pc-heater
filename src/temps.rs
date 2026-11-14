use std::collections::VecDeque;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use nvml_wrapper::Nvml;
use nvml_wrapper::enum_wrappers::device::TemperatureSensor;
use sysinfo::Components;

use crate::shared::Shared;

const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
/// Samples kept for the rolling average (5 minutes at one per second).
const WINDOW_SAMPLES: usize = 300;

/// Running temperature statistics in °C since the program started.
#[derive(Default)]
pub struct TempStats {
    pub current: Option<f32>,
    pub min: f32,
    pub max: f32,
    sum: f64,
    count: u64,
    window: VecDeque<f32>,
    window_sum: f64,
}

impl TempStats {
    fn record(&mut self, value: Option<f32>) {
        self.current = value;
        let Some(value) = value else { return };
        if self.count == 0 {
            self.min = value;
            self.max = value;
        } else {
            self.min = self.min.min(value);
            self.max = self.max.max(value);
        }
        self.sum += f64::from(value);
        self.count += 1;

        self.window.push_back(value);
        self.window_sum += f64::from(value);
        if self.window.len() > WINDOW_SAMPLES {
            self.window_sum -= f64::from(self.window.pop_front().unwrap());
        }
    }

    pub fn has_data(&self) -> bool {
        self.count > 0
    }

    /// Average over the whole run.
    pub fn avg_total(&self) -> f32 {
        (self.sum / self.count.max(1) as f64) as f32
    }

    /// Average over the last five minutes (or the whole run if shorter).
    pub fn avg_recent(&self) -> f32 {
        (self.window_sum / self.window.len().max(1) as f64) as f32
    }
}

#[derive(Default)]
pub struct Temps {
    pub cpu: TempStats,
    pub gpu: TempStats,
    /// What the user can do to get a CPU reading, when there is none.
    pub cpu_hint: Option<&'static str>,
}

pub fn spawn(shared: Arc<Shared>) {
    thread::Builder::new()
        .name("temp-monitor".into())
        .spawn(move || {
            let mut sensors = Sensors::new();
            shared.temps.lock().unwrap().cpu_hint = sensors.cpu_hint();

            // Running as administrator without the driver: install it in the
            // background and pick it up as soon as that finishes.
            #[cfg(windows)]
            let mut installing = {
                use crate::winsensor::{driver_installed, install_driver, is_elevated};
                (is_elevated() && !driver_installed()).then(|| {
                    shared.temps.lock().unwrap().cpu_hint = Some("installing PawnIO driver…");
                    thread::spawn(install_driver)
                })
            };

            loop {
                #[cfg(windows)]
                if installing.as_ref().is_some_and(|job| job.is_finished()) {
                    installing = None;
                    sensors.driver_cpu = crate::winsensor::CpuSensor::open();
                    shared.temps.lock().unwrap().cpu_hint = sensors.cpu_hint();
                }

                let (cpu, gpu) = sensors.read();
                {
                    let mut temps = shared.temps.lock().unwrap();
                    temps.cpu.record(cpu);
                    temps.gpu.record(gpu);
                }
                thread::sleep(SAMPLE_INTERVAL);
            }
        })
        .expect("failed to spawn temperature monitor");
}

struct Sensors {
    components: Option<Components>,
    nvml: Option<Nvml>,
    #[cfg(windows)]
    driver_cpu: Option<crate::winsensor::CpuSensor>,
    #[cfg(windows)]
    amd_gpu: Option<crate::winsensor::AmdGpu>,
}

impl Sensors {
    fn new() -> Self {
        Self {
            components: Components::new_with_refreshed_list().ok(),
            nvml: Nvml::init().ok(),
            #[cfg(windows)]
            driver_cpu: crate::winsensor::CpuSensor::open(),
            #[cfg(windows)]
            amd_gpu: crate::winsensor::AmdGpu::open(),
        }
    }

    fn read(&mut self) -> (Option<f32>, Option<f32>) {
        if let Some(components) = &mut self.components {
            components.refresh(false);
        }
        let cpu = self
            .driver_cpu_temp()
            .or_else(|| self.component_temp(CPU_PACKAGE_LABELS, CPU_FALLBACK_LABELS));
        let gpu = self
            .nvidia_temp()
            .or_else(|| self.amd_gpu_temp())
            .or_else(|| self.component_temp(GPU_LABELS, &[]));
        (cpu, gpu)
    }

    /// What the user can do to get a CPU reading, when there is none.
    #[cfg(windows)]
    fn cpu_hint(&self) -> Option<&'static str> {
        use crate::winsensor::{driver_installed, is_elevated};
        if self.driver_cpu.is_some() {
            None
        } else if !is_elevated() {
            Some("needs administrator")
        } else if !driver_installed() {
            Some("PawnIO driver install failed")
        } else {
            Some("CPU not supported")
        }
    }

    #[cfg(not(windows))]
    fn cpu_hint(&self) -> Option<&'static str> {
        None
    }

    #[cfg(windows)]
    fn driver_cpu_temp(&self) -> Option<f32> {
        self.driver_cpu.as_ref()?.read()
    }

    #[cfg(not(windows))]
    fn driver_cpu_temp(&self) -> Option<f32> {
        None
    }

    #[cfg(windows)]
    fn amd_gpu_temp(&self) -> Option<f32> {
        self.amd_gpu.as_ref()?.read()
    }

    #[cfg(not(windows))]
    fn amd_gpu_temp(&self) -> Option<f32> {
        None
    }

    fn nvidia_temp(&self) -> Option<f32> {
        let device = self.nvml.as_ref()?.device_by_index(0).ok()?;
        let celsius = device.temperature(TemperatureSensor::Gpu).ok()?;
        Some(celsius as f32)
    }

    /// Hottest sensor whose label matches `preferred`, falling back to the
    /// hottest one matching `fallback`.
    fn component_temp(&self, preferred: &[&str], fallback: &[&str]) -> Option<f32> {
        let components = self.components.as_ref()?;
        let hottest = |labels: &[&str]| {
            components
                .iter()
                .filter(|c| {
                    let label = c.label().to_lowercase();
                    labels.iter().any(|l| label.contains(l))
                })
                .filter_map(|c| c.temperature())
                .filter(|t| t.is_finite() && *t > 0.0)
                .reduce(f32::max)
        };
        hottest(preferred).or_else(|| hottest(fallback))
    }
}

/// Whole-package CPU sensors: Intel coretemp, AMD k10temp/zenpower.
const CPU_PACKAGE_LABELS: &[&str] = &["package", "tctl", "tdie"];
/// Per-core sensors. Generic ACPI thermal zones are deliberately left out:
/// on many desktop boards they report a fixed value unrelated to the CPU.
const CPU_FALLBACK_LABELS: &[&str] = &["coretemp", "k10temp", "zenpower", "cpu", "core"];
/// Non-NVIDIA GPUs that report through the kernel's hardware sensors on
/// Linux: AMD (amdgpu/radeon), Intel Arc (i915/xe) and nouveau.
const GPU_LABELS: &[&str] = &["amdgpu", "radeon", "i915", "xe ", "nouveau", "gpu"];
