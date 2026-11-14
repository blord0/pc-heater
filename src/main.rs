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

use shared::{Mode, ModeCell, Shared};
use temps::Stats;

const WINDOW_WIDTH: f32 = 440.0;
/// Internal marker for the copy started when asking for administrator rights.
#[cfg(windows)]
use winsensor::RELAUNCHED_FLAG;
#[cfg(not(windows))]
const RELAUNCHED_FLAG: &str = "--relaunched";
const USAGE_REFRESH: Duration = Duration::from_millis(1000);

fn main() -> eframe::Result {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            let text = format!(
                "{message}\nusage: pc-heater [--cpu off|half|full] [--gpu off|half|full]"
            );
            eprintln!("{text}");
            #[cfg(windows)]
            winsensor::error_box(&text);
            std::process::exit(2);
        }
    };

    // Reading the CPU temperature on Windows needs administrator rights, so
    // always offer to restart elevated. If that is declined, carry on
    // without it.
    #[cfg(windows)]
    if !args.relaunched
        && std::env::var_os("PC_HEATER_NO_ELEVATE").is_none()
        && !winsensor::is_elevated()
        && winsensor::relaunch_elevated()
    {
        return Ok(());
    }

    let (adapters, slots): (Vec<_>, Vec<_>) = gpu::discover().into_iter().unzip();
    let shared = Arc::new(Shared::new(slots));
    shared.cpu.set(args.cpu);
    for slot in &shared.gpus {
        slot.mode.set(args.gpu);
    }

    let cpu_threads = cpu::spawn(shared.clone());
    for (index, adapter) in adapters.into_iter().enumerate() {
        gpu::spawn(shared.clone(), index, adapter);
    }
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

struct Args {
    /// Starting mode for the CPU.
    cpu: Mode,
    /// Starting mode for every GPU.
    gpu: Mode,
    /// This copy was started by the request for administrator rights, so
    /// it must not ask again.
    relaunched: bool,
}

/// Read the command line. Both modes default to Off.
fn parse_args() -> Result<Args, String> {
    let mut parsed = Args {
        cpu: Mode::Off,
        gpu: Mode::Off,
        relaunched: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let target = match flag.as_str() {
            "--cpu" => &mut parsed.cpu,
            "--gpu" => &mut parsed.gpu,
            RELAUNCHED_FLAG => {
                parsed.relaunched = true;
                continue;
            }
            other => return Err(format!("unknown option '{other}'")),
        };
        let value = args.next().unwrap_or_default();
        *target = Mode::parse(&value)
            .ok_or_else(|| format!("'{flag}' needs one of: off, half, full"))?;
    }
    Ok(parsed)
}

struct HeaterApp {
    shared: Arc<Shared>,
    cpu_threads: usize,
    system: Option<System>,
    cpu_usage: f32,
    last_refresh: Instant,
    /// Window height last requested to fit the content.
    fitted_height: f32,
    /// Window title last set, so it is only changed when the total does.
    title: String,
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
            title: String::new(),
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

            // Show the combined draw of everything with a power reading in
            // the title bar.
            let readings = std::iter::once(&temps.cpu_power)
                .chain(temps.gpus.iter().map(|gpu| &gpu.power))
                .filter_map(|stats| stats.current);
            let title = match readings.reduce(|total, watts| total + watts) {
                Some(total) => format!("PC Heater – {total:.0} W"),
                None => "PC Heater".to_owned(),
            };
            if title != self.title {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
                self.title = title;
            }

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
                Some((&temps.cpu, temps.cpu_hint)),
            );

            if self.shared.gpus.is_empty() {
                section_break(ui);
                ui.heading("GPU");
                ui.colored_label(ui.visuals().error_fg_color, "No hardware GPU found");
            }
            for (index, (slot, stats)) in self.shared.gpus.iter().zip(&temps.gpus).enumerate() {
                section_break(ui);
                if self.shared.gpus.len() == 1 {
                    ui.heading("GPU");
                } else {
                    ui.heading(format!("GPU {}", index + 1));
                }
                let error = slot.error.lock().unwrap();
                mode_selector(ui, &slot.mode, error.is_none());
                ui.weak(&slot.name);
                if let Some(error) = &*error {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                readout(
                    ui,
                    &format!("gpu_readout_{index}"),
                    // Built-in graphics draw from the processor's own supply.
                    (
                        &stats.power,
                        slot.integrated.then_some("counted in the CPU figure"),
                    ),
                    // Graphics built into the processor have no sensor of
                    // their own; the CPU temperature already covers them.
                    (!slot.integrated).then_some((&stats.temp, None)),
                );
            }

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

fn section_break(ui: &mut egui::Ui) {
    ui.add_space(4.0);
    ui.separator();
    ui.add_space(4.0);
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

/// Power and (where the device has one) temperature statistics for one
/// device, as an aligned table.
fn readout(
    ui: &mut egui::Ui,
    id: &str,
    power: (&Stats, Option<&str>),
    temp: Option<(&Stats, Option<&str>)>,
) {
    let mut rows = vec![("Power", "W", power)];
    if let Some(temp) = temp {
        rows.push(("Temp", "°C", temp));
    }
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
