//! Linux-only power readings taken straight from sysfs.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::temps::EnergyCounter;

fn read_number(path: &Path) -> std::io::Result<u64> {
    let text = fs::read_to_string(path)?;
    text.trim()
        .parse()
        .map_err(|_| std::io::Error::from(ErrorKind::InvalidData))
}

/// One CPU package's energy counter under the kernel's powercap interface.
/// Covers Intel and AMD (both appear as `intel-rapl`).
struct RaplZone {
    energy_path: PathBuf,
    /// Value at which the microjoule counter wraps back to zero.
    wrap: u64,
    counter: EnergyCounter,
}

pub struct CpuPower {
    zones: Vec<RaplZone>,
    /// The counters exist but this user may not read them.
    pub denied: bool,
}

impl CpuPower {
    pub fn open() -> Self {
        let mut zones = Vec::new();
        let mut denied = false;
        let entries = fs::read_dir("/sys/class/powercap")
            .into_iter()
            .flatten()
            .flatten();
        for entry in entries {
            // Packages are "intel-rapl:N"; "intel-rapl:N:M" are parts of one
            // package and would be double counted.
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_package = name
                .strip_prefix("intel-rapl:")
                .is_some_and(|rest| !rest.contains(':'));
            if !is_package {
                continue;
            }
            let dir = entry.path();
            // Skip non-CPU zones such as "psys" (whole platform).
            let zone_name = fs::read_to_string(dir.join("name")).unwrap_or_default();
            if !zone_name.trim().starts_with("package") {
                continue;
            }
            let energy_path = dir.join("energy_uj");
            match read_number(&energy_path) {
                Ok(_) => zones.push(RaplZone {
                    wrap: read_number(&dir.join("max_energy_range_uj")).unwrap_or(u64::MAX),
                    energy_path,
                    counter: EnergyCounter::default(),
                }),
                Err(error) if error.kind() == ErrorKind::PermissionDenied => denied = true,
                Err(_) => {}
            }
        }
        Self { zones, denied }
    }

    /// Total draw of all CPU packages in watts.
    pub fn watts(&mut self) -> Option<f32> {
        let mut total = None;
        for zone in &mut self.zones {
            let raw = read_number(&zone.energy_path).ok()?;
            if let Some(watts) = zone.counter.watts(raw, zone.wrap, 1e-6) {
                total = Some(total.unwrap_or(0.0) + watts);
            }
        }
        total
    }
}

/// Power draw of a non-NVIDIA graphics card in watts, from the kernel's
/// hardware sensors (reported by amdgpu).
pub fn gpu_watts() -> Option<f32> {
    let entries = fs::read_dir("/sys/class/hwmon").ok()?;
    for entry in entries.flatten() {
        let dir = entry.path();
        let name = fs::read_to_string(dir.join("name")).unwrap_or_default();
        if name.trim() != "amdgpu" {
            continue;
        }
        for file in ["power1_average", "power1_input"] {
            if let Ok(microwatts) = read_number(&dir.join(file)) {
                return Some(microwatts as f32 / 1e6);
            }
        }
    }
    None
}
