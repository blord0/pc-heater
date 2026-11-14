#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod cpu;
mod gpu;
mod shared;
mod temps;
#[cfg(target_os = "linux")]
mod linsensor;
#[cfg(windows)]
mod winsensor;

use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use sysinfo::System;

use shared::{GpuState, Mode, ModeCell, Shared};
use temps::Stats;

const WINDOW_WIDTH: f32 = 440.0;
const USAGE_REFRESH: Duration = Duration::from_millis(1000);

fn main() -> eframe::Result {
    let shared = Arc::new(Shared::new());
    if let Err(message) = apply_args(&shared) {
        eprintln!("{message}\nusage: pc-heater [--cpu off|half|full] [--gpu off|half|full]");
        std::process::exit(2);
    }

    // Reading the CPU temperature on Windows needs administrator rights, so
    // always offer to restart elevated. If that is declined, carry on
    // without it.
    #[cfg(windows)]
    if std::env::var_os("PC_HEATER_NO_ELEVATE").is_none()
        && !winsensor::is_elevated()
        && winsensor::relaunch_elevated()
    {
        return Ok(());
    }

    let cpu_threads = cpu::spawn(shared.clone());
    gpu::spawn(shared.clone());
    temps::spawn(shared.clone());

    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png"))
        .expect("bundled icon is a valid PNG");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("PC Heater")
            .with_app_id("pc-heater")
            .with_icon(icon)
            .with_inner_size([WINDOW_WIDTH, 380.0])
            .with_resizable(false),
        ..Default::default()
    };
    eframe::run_native(
        "pc-heater",
        options,
        Box::new(move |_cc| Ok(Box::new(HeaterApp::new(shared, cpu_threads)))),
    )
}

/// Optional starting modes from the command line; both default to Off.
fn apply_args(shared: &Shared) -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let cell = match flag.as_str() {
            "--cpu" => &shared.cpu,
            "--gpu" => &shared.gpu,
            other => return Err(format!("unknown option '{other}'")),
        };
        let value = args.next().unwrap_or_default();
        let mode = Mode::parse(&value)
            .ok_or_else(|| format!("'{flag}' needs one of: off, half, full"))?;
        cell.set(mode);
    }
    Ok(())
}

struct HeaterApp {
    shared: Arc<Shared>,
    cpu_threads: usize,
    system: Option<System>,
    cpu_usage: f32,
    last_refresh: Instant,
    /// Window height last requested to fit the content.
    fitted_height: f32,
}

impl HeaterApp {
    fn new(shared: Arc<Shared>, cpu_threads: usize) -> Self {
        let mut system = System::new().ok();
        if let Some(system) = &mut system {
            system.refresh_cpu_usage();
        }
        Self {
            shared,
            cpu_threads,
            system,
            cpu_usage: 0.0,
            last_refresh: Instant::now(),
            fitted_height: 0.0,
        }
    }
}

impl eframe::App for HeaterApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.last_refresh.elapsed() >= USAGE_REFRESH {
            if let Some(system) = &mut self.system {
                system.refresh_cpu_usage();
                self.cpu_usage = system.global_cpu_usage();
            }
            self.last_refresh = Instant::now();
        }
        ui.ctx().request_repaint_after(USAGE_REFRESH);

        egui::CentralPanel::default().show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            let temps = self.shared.temps.lock().unwrap();

            ui.heading("CPU");
            mode_selector(ui, &self.shared.cpu, true);
            if self.system.is_some() {
                ui.weak(format!(
                    "{} threads · {:.0}% total usage",
                    self.cpu_threads, self.cpu_usage
                ));
            } else {
                ui.weak(format!("{} threads", self.cpu_threads));
            }
            readout(
                ui,
                "cpu_readout",
                (&temps.cpu_power, temps.cpu_power_hint),
                (&temps.cpu, temps.cpu_hint),
            );

            ui.add_space(4.0);
            ui.separator();
            ui.add_space(4.0);

            ui.heading("GPU");
            let state = self.shared.gpu_state.lock().unwrap();
            match &*state {
                GpuState::Starting => {
                    mode_selector(ui, &self.shared.gpu, false);
                    ui.weak("Starting…");
                    ui.ctx().request_repaint_after(Duration::from_millis(100));
                }
                GpuState::Ready(name) => {
                    mode_selector(ui, &self.shared.gpu, true);
                    ui.weak(name);
                }
                GpuState::Failed(error) => {
                    mode_selector(ui, &self.shared.gpu, false);
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
            }
            readout(ui, "gpu_readout", (&temps.gpu_power, None), (&temps.gpu, None));

            // Keep the window exactly tall enough for everything above.
            // The cursor sits just below the last widget; the panel itself
            // always fills the window, so its own rect can't be used here.
            let needed = (ui.cursor().top() + 4.0).ceil();
            if (needed - self.fitted_height).abs() > 0.5 {
                self.fitted_height = needed;
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                    WINDOW_WIDTH,
                    needed,
                )));
            }
        });
    }
}

fn mode_selector(ui: &mut egui::Ui, cell: &ModeCell, enabled: bool) {
    let current = cell.get();
    ui.add_enabled_ui(enabled, |ui| {
        ui.horizontal(|ui| {
            for mode in Mode::ALL {
                let text = egui::RichText::new(mode.label()).size(18.0);
                let button = egui::Button::selectable(current == mode, text)
                    .min_size(egui::vec2(100.0, 34.0));
                if ui.add(button).clicked() {
                    cell.set(mode);
                }
            }
        });
    });
}

/// Power and temperature statistics for one device, as an aligned table.
fn readout(
    ui: &mut egui::Ui,
    id: &str,
    power: (&Stats, Option<&str>),
    temp: (&Stats, Option<&str>),
) {
    let rows = [("Power", "W", power), ("Temp", "°C", temp)];
    egui::Grid::new(id).spacing([18.0, 4.0]).show(ui, |ui| {
        for title in ["", "Now", "Min", "Max", "Avg (total)", "Avg (5 min)"] {
            ui.weak(title);
        }
        ui.end_row();
        for (name, unit, (stats, _)) in &rows {
            ui.weak(*name);
            if stats.has_data() {
                match stats.current {
                    Some(value) => ui.strong(format!("{value:.0} {unit}")),
                    None => ui.strong("–"),
                };
                ui.strong(format!("{:.0} {unit}", stats.min));
                ui.strong(format!("{:.0} {unit}", stats.max));
                ui.strong(format!("{:.1} {unit}", stats.avg_total()));
                ui.strong(format!("{:.1} {unit}", stats.avg_recent()));
            } else {
                for _ in 0..5 {
                    ui.weak("–");
                }
            }
            ui.end_row();
        }
    });
    for (name, _, (stats, hint)) in &rows {
        if !stats.has_data() {
            match hint {
                Some(hint) => ui.weak(format!("{name}: no sensor found ({hint})")),
                None => ui.weak(format!("{name}: no sensor found")),
            };
        }
    }
}
