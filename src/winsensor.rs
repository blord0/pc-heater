//! Windows-only temperature sources that sysinfo and NVML don't cover.
//!
//! CPU: Windows has no unprivileged way to read CPU temperatures, so this
//! talks to PawnIO (a signed kernel driver the user installs once) through
//! its Intel and AMD modules. Needs administrator rights; without them, or
//! without the driver, there is simply no reading.
//!
//! GPU: AMD cards are read through the ADL library that ships with the
//! Radeon driver. No special rights needed.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, HMODULE, INVALID_HANDLE_VALUE,
    WAIT_ABANDONED, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY, RegCloseKey, RegOpenKeyExW,
};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, OpenMutexW, OpenProcessToken, ReleaseMutex,
    WaitForSingleObject,
};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

fn wide(text: &str) -> Vec<u16> {
    std::ffi::OsStr::new(text)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

// ---------------------------------------------------------------------------
// Elevation
// ---------------------------------------------------------------------------

const UNINSTALL_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\PawnIO";

pub fn driver_installed() -> bool {
    let mut key: HKEY = null_mut();
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            wide(UNINSTALL_KEY).as_ptr(),
            0,
            KEY_READ | KEY_WOW64_64KEY,
            &mut key,
        )
    };
    if status != 0 {
        return false;
    }
    unsafe { RegCloseKey(key) };
    true
}

/// Install the PawnIO driver with the Windows package manager. Needs
/// administrator rights. Returns true once the driver is installed.
pub fn install_driver() -> bool {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let finished = std::process::Command::new("winget")
        .args([
            "install",
            "--id",
            "namazso.PawnIO",
            "--exact",
            "--silent",
            "--disable-interactivity",
            "--accept-source-agreements",
            "--accept-package-agreements",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .is_ok();
    finished && driver_installed()
}

pub fn is_elevated() -> bool {
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut size = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            (&raw mut elevation).cast::<c_void>(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        );
        CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

/// Ask Windows to start another copy of this program as administrator,
/// passing the same arguments. Returns true if that copy was started (the
/// caller should then exit); false if the user declined or it failed.
pub fn relaunch_elevated() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let exe: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
    let args = std::env::args()
        .skip(1)
        .map(|a| format!("\"{}\"", a.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" ");
    let result = unsafe {
        ShellExecuteW(
            null_mut(),
            wide("runas").as_ptr(),
            exe.as_ptr(),
            wide(&args).as_ptr(),
            null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecute reports success with any value above 32.
    result as usize > 32
}

// ---------------------------------------------------------------------------
// PawnIO
// ---------------------------------------------------------------------------

const DEVICE_PATH: &str = r"\\?\GLOBALROOT\Device\PawnIO";
const IOCTL_LOAD_BINARY: u32 = (41394 << 16) | (0x821 << 2);
const IOCTL_EXECUTE_FN: u32 = (41394 << 16) | (0x841 << 2);
const FN_NAME_LENGTH: usize = 32;

/// Signed PawnIO modules. Each refuses to load on the other vendor's CPUs.
static INTEL_MSR_MODULE: &[u8] = include_bytes!("../assets/pawnio/IntelMSR.bin");
static AMD_FAMILY17_MODULE: &[u8] = include_bytes!("../assets/pawnio/AMDFamily17.bin");

/// An open PawnIO session with one module loaded.
pub struct PawnIo(HANDLE);

impl PawnIo {
    fn open(module: &[u8]) -> Option<Self> {
        let handle = unsafe {
            CreateFileW(
                wide(DEVICE_PATH).as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return None;
        }
        let session = Self(handle);
        let mut returned = 0u32;
        let loaded = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_LOAD_BINARY,
                module.as_ptr().cast(),
                module.len() as u32,
                null_mut(),
                0,
                &mut returned,
                null_mut(),
            )
        };
        (loaded != 0).then_some(session)
    }

    /// Call a module function taking one 64-bit value and returning one.
    fn call(&self, function: &str, argument: u64) -> Option<u64> {
        let mut input = [0u8; FN_NAME_LENGTH + 8];
        let name = function.as_bytes();
        input[..name.len().min(FN_NAME_LENGTH - 1)]
            .copy_from_slice(&name[..name.len().min(FN_NAME_LENGTH - 1)]);
        input[FN_NAME_LENGTH..].copy_from_slice(&argument.to_le_bytes());
        let mut output = [0u8; 8];
        let mut returned = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                self.0,
                IOCTL_EXECUTE_FN,
                input.as_ptr().cast(),
                input.len() as u32,
                output.as_mut_ptr().cast(),
                output.len() as u32,
                &mut returned,
                null_mut(),
            )
        };
        (ok != 0 && returned as usize == output.len()).then(|| u64::from_le_bytes(output))
    }
}

impl Drop for PawnIo {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

const MSR_TEMPERATURE_TARGET: u64 = 0x1A2;
const MSR_PACKAGE_THERM_STATUS: u64 = 0x1B1;
const MSR_RAPL_POWER_UNIT: u64 = 0x606;
const MSR_PKG_ENERGY_STATUS: u64 = 0x611;
const MSR_AMD_RAPL_POWER_UNIT: u64 = 0xC001_0299;
const MSR_AMD_PKG_ENERGY_STATUS: u64 = 0xC001_029B;

/// Zen reported-temperature register (same one Linux's k10temp uses).
const SMN_THM_TCON_CUR_TMP: u64 = 0x0005_9800;
const ZEN_RANGE_SEL: u64 = 0x8_0000;
const ZEN_TJ_SEL: u64 = 0x3_0000;
/// Other hardware tools take this mutex around PCI config access.
const PCI_MUTEX: &str = r"Global\Access_PCI";
/// Access right needed to wait on an existing mutex.
const SYNCHRONIZE: u32 = 0x0010_0000;

/// CPU package temperature and energy use through PawnIO.
pub enum CpuSensor {
    Intel {
        io: PawnIo,
        tj_max: f32,
        joules_per_unit: Option<f64>,
    },
    Amd {
        io: PawnIo,
        pci_mutex: HANDLE,
        joules_per_unit: Option<f64>,
    },
}

/// Size of one step of the package energy counter, from the RAPL unit MSR.
fn energy_unit(io: &PawnIo, unit_msr: u64) -> Option<f64> {
    let bits = (io.call("ioctl_read_msr", unit_msr)? >> 8) & 0x1F;
    Some(1.0 / (1u64 << bits) as f64)
}

// The handles are only ever used from the temperature thread.
unsafe impl Send for CpuSensor {}

impl CpuSensor {
    pub fn open() -> Option<Self> {
        if let Some(io) = PawnIo::open(INTEL_MSR_MODULE) {
            let target = (io.call("ioctl_read_msr", MSR_TEMPERATURE_TARGET)? >> 16) & 0xFF;
            let tj_max = if target > 0 { target as f32 } else { 100.0 };
            let joules_per_unit = energy_unit(&io, MSR_RAPL_POWER_UNIT);
            return Some(Self::Intel {
                io,
                tj_max,
                joules_per_unit,
            });
        }
        // Covers Zen and later (families 17h to 1Ah).
        let io = PawnIo::open(AMD_FAMILY17_MODULE)?;
        let name = wide(PCI_MUTEX);
        let mut pci_mutex = unsafe { CreateMutexW(null(), 0, name.as_ptr()) };
        if pci_mutex.is_null() {
            pci_mutex = unsafe { OpenMutexW(SYNCHRONIZE, 0, name.as_ptr()) };
        }
        let joules_per_unit = energy_unit(&io, MSR_AMD_RAPL_POWER_UNIT);
        Some(Self::Amd {
            io,
            pci_mutex,
            joules_per_unit,
        })
    }

    /// The package energy counter: its raw 32-bit value and the size of one
    /// step in joules. It wraps around, so only differences are meaningful.
    pub fn energy(&self) -> Option<(u64, f64)> {
        let (io, unit, msr) = match self {
            Self::Intel {
                io,
                joules_per_unit,
                ..
            } => (io, joules_per_unit, MSR_PKG_ENERGY_STATUS),
            Self::Amd {
                io,
                joules_per_unit,
                ..
            } => (io, joules_per_unit, MSR_AMD_PKG_ENERGY_STATUS),
        };
        let raw = io.call("ioctl_read_msr", msr)? & 0xFFFF_FFFF;
        Some((raw, (*unit)?))
    }

    /// Temperature in °C: package for Intel, Tctl for AMD.
    pub fn read(&self) -> Option<f32> {
        match self {
            Self::Intel { io, tj_max, .. } => {
                let status = io.call("ioctl_read_msr", MSR_PACKAGE_THERM_STATUS)?;
                // Bits 22:16 hold how far below TjMax the package currently is.
                Some(tj_max - ((status >> 16) & 0x7F) as f32)
            }
            Self::Amd { io, pci_mutex, .. } => {
                if !pci_mutex.is_null() {
                    let wait = unsafe { WaitForSingleObject(*pci_mutex, 10) };
                    if wait != WAIT_OBJECT_0 && wait != WAIT_ABANDONED {
                        return None;
                    }
                }
                let raw = io.call("ioctl_read_smn", SMN_THM_TCON_CUR_TMP);
                if !pci_mutex.is_null() {
                    unsafe { ReleaseMutex(*pci_mutex) };
                }
                let raw = raw? & 0xFFFF_FFFF;
                let mut celsius = (raw >> 21) as f32 * 0.125;
                if raw & ZEN_RANGE_SEL != 0 || raw & ZEN_TJ_SEL == ZEN_TJ_SEL {
                    celsius -= 49.0;
                }
                (celsius > 0.0).then_some(celsius)
            }
        }
    }
}

impl Drop for CpuSensor {
    fn drop(&mut self) {
        if let Self::Amd { pci_mutex, .. } = self
            && !pci_mutex.is_null()
        {
            unsafe { CloseHandle(*pci_mutex) };
        }
    }
}

// ---------------------------------------------------------------------------
// AMD GPU (ADL)
// ---------------------------------------------------------------------------

const ADL_MAX_PATH: usize = 256;
const ADL_PMLOG_MAX_SENSORS: usize = 256;
const ADL_PMLOG_TEMPERATURE_EDGE: usize = 8;
const ADL_PMLOG_TEMPERATURE_GFX: usize = 28;
const ADL_PMLOG_ASIC_POWER: usize = 23;
const ADL_PMLOG_BOARD_POWER: usize = 73;
const ADL_ODN_TEMPERATURE_EDGE: i32 = 1;
const AMD_VENDOR_ID: i32 = 0x1002;

#[repr(C)]
struct AdlAdapterInfo {
    size: i32,
    adapter_index: i32,
    udid: [u8; ADL_MAX_PATH],
    bus_number: i32,
    device_number: i32,
    function_number: i32,
    vendor_id: i32,
    adapter_name: [u8; ADL_MAX_PATH],
    display_name: [u8; ADL_MAX_PATH],
    present: i32,
    exist: i32,
    driver_path: [u8; ADL_MAX_PATH],
    driver_path_ext: [u8; ADL_MAX_PATH],
    pnp_string: [u8; ADL_MAX_PATH],
    os_display_index: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AdlSensor {
    supported: i32,
    value: i32,
}

#[repr(C)]
struct AdlPmLogData {
    size: i32,
    sensors: [AdlSensor; ADL_PMLOG_MAX_SENSORS],
}

#[repr(C)]
struct AdlTemperature {
    size: i32,
    temperature: i32,
}

type AdlContext = *mut c_void;
type AdlAlloc = unsafe extern "C" fn(i32) -> *mut c_void;
type AdlCreate = unsafe extern "C" fn(AdlAlloc, i32, *mut AdlContext) -> i32;
type AdlDestroy = unsafe extern "C" fn(AdlContext) -> i32;
type AdlAdapterCount = unsafe extern "C" fn(AdlContext, *mut i32) -> i32;
type AdlAdapterInfoGet = unsafe extern "C" fn(AdlContext, *mut AdlAdapterInfo, i32) -> i32;
type AdlPmLogGet = unsafe extern "C" fn(AdlContext, i32, *mut AdlPmLogData) -> i32;
type AdlOdnTempGet = unsafe extern "C" fn(AdlContext, i32, i32, *mut i32) -> i32;
type AdlOd5TempGet = unsafe extern "C" fn(AdlContext, i32, i32, *mut AdlTemperature) -> i32;

/// ADL asks the caller to allocate its buffers and never hands them back,
/// so these small allocations live for the rest of the process.
unsafe extern "C" fn adl_alloc(size: i32) -> *mut c_void {
    let words = (size.max(0) as usize).div_ceil(8).max(1);
    vec![0u64; words].leak().as_mut_ptr().cast()
}

/// Look up an exported function, or None if this driver version lacks it.
unsafe fn symbol<T: Copy>(library: HMODULE, name: &[u8]) -> Option<T> {
    debug_assert!(name.ends_with(b"\0"));
    let address = unsafe { GetProcAddress(library, name.as_ptr()) }?;
    Some(unsafe { std::mem::transmute_copy(&address) })
}

/// Temperature and power of one AMD graphics card.
pub struct AmdGpu {
    context: AdlContext,
    adapter: i32,
    destroy: AdlDestroy,
    pm_log: Option<AdlPmLogGet>,
    odn_temp: Option<AdlOdnTempGet>,
    od5_temp: Option<AdlOd5TempGet>,
}

unsafe impl Send for AmdGpu {}

impl AmdGpu {
    /// Open the AMD card at the given PCI bus and device number, or, when
    /// that is unknown, the `ordinal`-th AMD card.
    pub fn open(address: Option<(u32, u32)>, ordinal: usize) -> Option<Self> {
        unsafe {
            let library = LoadLibraryExW(
                wide("atiadlxx.dll").as_ptr(),
                null_mut(),
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            );
            if library.is_null() {
                return None;
            }
            let create: AdlCreate = symbol(library, b"ADL2_Main_Control_Create\0")?;
            let destroy: AdlDestroy = symbol(library, b"ADL2_Main_Control_Destroy\0")?;
            let count: AdlAdapterCount = symbol(library, b"ADL2_Adapter_NumberOfAdapters_Get\0")?;
            let info: AdlAdapterInfoGet = symbol(library, b"ADL2_Adapter_AdapterInfo_Get\0")?;

            let mut context: AdlContext = null_mut();
            if create(adl_alloc, 1, &mut context) != 0 {
                return None;
            }
            // From here on, dropping `gpu` releases the context.
            let mut gpu = Self {
                context,
                adapter: -1,
                destroy,
                pm_log: symbol(library, b"ADL2_New_QueryPMLogData_Get\0"),
                odn_temp: symbol(library, b"ADL2_OverdriveN_Temperature_Get\0"),
                od5_temp: symbol(library, b"ADL2_Overdrive5_Temperature_Get\0"),
            };

            let mut adapters = 0i32;
            if count(context, &mut adapters) != 0 || adapters <= 0 {
                return None;
            }
            let mut infos: Vec<AdlAdapterInfo> = (0..adapters)
                .map(|_| std::mem::zeroed::<AdlAdapterInfo>())
                .collect();
            let bytes = (infos.len() * size_of::<AdlAdapterInfo>()) as i32;
            if info(context, infos.as_mut_ptr(), bytes) != 0 {
                return None;
            }
            // Each card is listed once per display output: group the
            // entries by card.
            let mut cards: Vec<((u32, u32), Vec<i32>)> = Vec::new();
            for entry in &infos {
                let udid = String::from_utf8_lossy(&entry.udid).to_ascii_uppercase();
                // Some driver versions report the vendor as decimal 1002.
                let is_amd = entry.vendor_id == AMD_VENDOR_ID
                    || entry.vendor_id == 1002
                    || udid.contains("VEN_1002");
                if !is_amd {
                    continue;
                }
                let location = (entry.bus_number as u32, entry.device_number as u32);
                match cards.iter_mut().find(|(known, _)| *known == location) {
                    Some((_, indices)) => indices.push(entry.adapter_index),
                    None => cards.push((location, vec![entry.adapter_index])),
                }
            }
            let by_address =
                address.and_then(|wanted| cards.iter().find(|(location, _)| *location == wanted));
            let (_, indices) = by_address.or_else(|| cards.get(ordinal))?;
            // Use the first of the card's entries that gives a reading.
            for &index in indices {
                gpu.adapter = index;
                if gpu.read().is_some() {
                    return Some(gpu);
                }
            }
            None
        }
    }

    /// First supported, plausible value among the given driver log sensors.
    fn pm_log_value(&self, sensors: &[usize], max: f32) -> Option<f32> {
        let pm_log = self.pm_log?;
        unsafe {
            let mut data: AdlPmLogData = std::mem::zeroed();
            data.size = size_of::<AdlPmLogData>() as i32;
            if pm_log(self.context, self.adapter, &mut data) != 0 {
                return None;
            }
            sensors
                .iter()
                .map(|&index| data.sensors[index])
                .filter(|sensor| sensor.supported != 0)
                .map(|sensor| sensor.value as f32)
                .find(|value| *value > 0.0 && *value < max)
        }
    }

    /// Power draw in watts. Only current cards report this.
    pub fn power(&self) -> Option<f32> {
        self.pm_log_value(&[ADL_PMLOG_BOARD_POWER, ADL_PMLOG_ASIC_POWER], 2000.0)
    }

    pub fn read(&self) -> Option<f32> {
        let plausible = |celsius: f32| (celsius > 0.0 && celsius < 150.0).then_some(celsius);
        // Current cards: the driver's sensor log, already in °C.
        if let Some(celsius) = self.pm_log_value(
            &[ADL_PMLOG_TEMPERATURE_EDGE, ADL_PMLOG_TEMPERATURE_GFX],
            150.0,
        ) {
            return Some(celsius);
        }
        unsafe {
            // Older cards: Overdrive N, then Overdrive 5, both in millidegrees.
            if let Some(odn_temp) = self.odn_temp {
                let mut value = 0i32;
                if odn_temp(self.context, self.adapter, ADL_ODN_TEMPERATURE_EDGE, &mut value) == 0
                    && let Some(celsius) = plausible(value as f32 * 0.001)
                {
                    return Some(celsius);
                }
            }
            if let Some(od5_temp) = self.od5_temp {
                let mut value = AdlTemperature {
                    size: size_of::<AdlTemperature>() as i32,
                    temperature: 0,
                };
                if od5_temp(self.context, self.adapter, 0, &mut value) == 0
                    && let Some(celsius) = plausible(value.temperature as f32 * 0.001)
                {
                    return Some(celsius);
                }
            }
        }
        None
    }
}

impl Drop for AmdGpu {
    fn drop(&mut self) {
        unsafe { (self.destroy)(self.context) };
    }
}
