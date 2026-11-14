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

pub const VENDOR_NVIDIA: u32 = 0x10DE;
#[cfg(windows)]
pub const VENDOR_AMD: u32 = 0x1002;

/// Where a card sits on the PCI bus. Used to tie sensor readings to the
/// card they belong to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PciAddress {
    pub domain: u32,
    pub bus: u32,
    pub device: u32,
    pub function: u32,
}

impl PciAddress {
    /// Parse the usual `0000:01:00.0` form.
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.trim().split(':');
        let (domain, bus, rest) = (parts.next()?, parts.next()?, parts.next()?);
        let (device, function) = rest.split_once('.')?;
        let hex = |digits: &str| u32::from_str_radix(digits, 16).ok();
        Some(Self {
            domain: hex(domain)?,
            bus: hex(bus)?,
            device: hex(device)?,
            function: hex(function)?,
        })
    }
}

/// One graphics card being heated.
pub struct GpuSlot {
    pub name: String,
    /// PCI vendor ID.
    pub vendor: u32,
    pub address: Option<PciAddress>,
    pub mode: ModeCell,
    /// Set if the card's load could not be started or stopped working.
    pub error: Mutex<Option<String>>,
}

pub struct Shared {
    pub cpu: ModeCell,
    pub gpus: Vec<GpuSlot>,
    pub temps: Mutex<Temps>,
}

impl Shared {
    pub fn new(gpus: Vec<GpuSlot>) -> Self {
        let temps = Temps::new(gpus.len());
        Self {
            cpu: ModeCell::new(),
            gpus,
            temps: Mutex::new(temps),
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
