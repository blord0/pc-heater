use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::temps::Temps;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Mode {
    Off = 0,
    Half = 1,
    Full = 2,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::Off, Mode::Half, Mode::Full];

    pub fn parse(text: &str) -> Option<Mode> {
        Mode::ALL
            .into_iter()
            .find(|m| m.label().eq_ignore_ascii_case(text))
    }

    pub fn label(self) -> &'static str {
        match self {
            Mode::Off => "Off",
            Mode::Half => "Half",
            Mode::Full => "Full",
        }
    }
}

/// A mode that can be changed from the GUI thread and read by workers.
pub struct ModeCell(AtomicU8);

impl ModeCell {
    pub const fn new() -> Self {
        Self(AtomicU8::new(Mode::Off as u8))
    }

    pub fn get(&self) -> Mode {
        match self.0.load(Ordering::Relaxed) {
            1 => Mode::Half,
            2 => Mode::Full,
            _ => Mode::Off,
        }
    }

    pub fn set(&self, mode: Mode) {
        self.0.store(mode as u8, Ordering::Relaxed);
    }
}

pub enum GpuState {
    Starting,
    Ready(String),
    Failed(String),
}

pub struct Shared {
    pub cpu: ModeCell,
    pub gpu: ModeCell,
    pub gpu_state: Mutex<GpuState>,
    pub temps: Mutex<Temps>,
}

impl Shared {
    pub fn new() -> Self {
        Self {
            cpu: ModeCell::new(),
            gpu: ModeCell::new(),
            gpu_state: Mutex::new(GpuState::Starting),
            temps: Mutex::new(Temps::default()),
        }
    }
}

/// Drop the calling thread to the lowest scheduling priority so the heater
/// never starves the desktop or other programs.
pub fn lower_thread_priority() {
    #[cfg(unix)]
    unsafe {
        // On Linux, `who == 0` with PRIO_PROCESS applies to the calling thread.
        libc::setpriority(libc::PRIO_PROCESS, 0, 19);
    }
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_LOWEST,
        };
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_LOWEST);
    }
}
