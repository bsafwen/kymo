//! NVIDIA GPUs through NVML (nvml.dll, installed with the driver), called in-process: about 0.2 ms
//! per sample, where starting nvidia-smi costs about 26 ms of CPU. Without the driver there is
//! simply no GPU series.

use crate::system::{push, Sample};
use crate::units::Unit;
use crate::win;
use std::ffi::c_void;

type Dev = *mut c_void;
type FInit = unsafe extern "C" fn() -> i32;
type FCount = unsafe extern "C" fn(*mut u32) -> i32;
type FHandle = unsafe extern "C" fn(u32, *mut Dev) -> i32;
type FU32 = unsafe extern "C" fn(Dev, *mut u32) -> i32;
type FU64 = unsafe extern "C" fn(Dev, *mut u64) -> i32;
type FArg = unsafe extern "C" fn(Dev, u32, *mut u32) -> i32;
type FPair = unsafe extern "C" fn(Dev, *mut u32, *mut u32) -> i32;
type FUtil = unsafe extern "C" fn(Dev, *mut [u32; 2]) -> i32;
type FMem1 = unsafe extern "C" fn(Dev, *mut [u64; 3]) -> i32;
type FMem2 = unsafe extern "C" fn(Dev, *mut MemV2) -> i32;

/// nvmlMemory_v2_t
#[repr(C)]
struct MemV2 {
    version: u32,
    total: u64,
    reserved: u64,
    free: u64,
    used: u64,
}

/// Clock event reasons (nvmlClocksEventReason*), stored as % of the time they were active.
const REASONS: [(&str, u64); 6] = [
    ("slow_idle", 0x1),
    ("slow_sw_power", 0x4),
    ("slow_hw", 0x8),
    ("slow_sw_thermal", 0x20),
    ("slow_hw_thermal", 0x40),
    ("slow_hw_power_brake", 0x80),
];

const CLOCKS: [(&str, u32); 4] = [
    ("clock_graphics", 0),
    ("clock_sm", 1),
    ("clock_mem", 2),
    ("clock_video", 3),
];

pub struct Gpu {
    lib: win::Handle,
    shutdown: Option<FInit>,
    devices: Vec<Dev>,
    util: FUtil,
    enc: Option<FPair>,
    dec: Option<FPair>,
    mem1: Option<FMem1>,
    mem2: Option<FMem2>,
    temp: Option<FArg>,
    power: Option<FU32>,
    limit: Option<FU32>,
    clock: Option<FArg>,
    fan: Option<FU32>,
    pstate: Option<FU32>,
    reasons: Option<FU64>,
}

/// # Safety
/// `T` must be the function-pointer type of the export `name`.
unsafe fn sym<T: Copy>(lib: win::Handle, name: &str) -> Option<T> {
    let mut z = name.as_bytes().to_vec();
    z.push(0);
    let p = win::GetProcAddress(lib, z.as_ptr());
    if p.is_null() {
        None
    } else {
        Some(std::mem::transmute_copy::<*const c_void, T>(&p))
    }
}

/// Why there are no GPU readings.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NoGpu {
    /// No NVIDIA driver, or a driver without an NVIDIA GPU.
    Absent,
    /// The driver is installed but NVML does not answer (driver update, crash or reset).
    Failing,
}

impl Gpu {
    pub fn open() -> Result<Gpu, NoGpu> {
        unsafe {
            let lib = win::LoadLibraryW(win::wide("nvml.dll").as_ptr());
            if lib.is_null() {
                return Err(NoGpu::Absent);
            }
            let shutdown: Option<FInit> = sym(lib, "nvmlShutdown");
            let close = || {
                if let Some(f) = shutdown {
                    f();
                }
                win::FreeLibrary(lib);
            };
            let (Some(init), Some(count), Some(handle)) = (
                sym::<FInit>(lib, "nvmlInit_v2"),
                sym::<FCount>(lib, "nvmlDeviceGetCount_v2"),
                sym::<FHandle>(lib, "nvmlDeviceGetHandleByIndex_v2"),
            ) else {
                win::FreeLibrary(lib);
                return Err(NoGpu::Failing);
            };
            match init() {
                0 => {}
                // NVML_ERROR_DRIVER_NOT_LOADED: the driver files without a working NVIDIA GPU (GPU
                // disabled, external GPU unplugged, card swapped out). A driver that was giving
                // readings and stops is still reported, from the readings seen in the last hour.
                9 => {
                    win::FreeLibrary(lib);
                    return Err(NoGpu::Absent);
                }
                _ => {
                    win::FreeLibrary(lib);
                    return Err(NoGpu::Failing);
                }
            }
            let mut n = 0u32;
            let mut devices = Vec::new();
            let counted = count(&mut n) == 0;
            if counted {
                for i in 0..n {
                    let mut d: Dev = std::ptr::null_mut();
                    if handle(i, &mut d) == 0 {
                        devices.push(d);
                    }
                }
            }
            let Some(util) = sym::<FUtil>(lib, "nvmlDeviceGetUtilizationRates") else {
                close();
                return Err(NoGpu::Failing);
            };
            if devices.is_empty() {
                close();
                return Err(if counted && n == 0 {
                    NoGpu::Absent
                } else {
                    NoGpu::Failing
                });
            }
            Ok(Gpu {
                lib,
                shutdown,
                devices,
                util,
                enc: sym(lib, "nvmlDeviceGetEncoderUtilization"),
                dec: sym(lib, "nvmlDeviceGetDecoderUtilization"),
                mem1: sym(lib, "nvmlDeviceGetMemoryInfo"),
                mem2: sym(lib, "nvmlDeviceGetMemoryInfo_v2"),
                temp: sym(lib, "nvmlDeviceGetTemperature"),
                power: sym(lib, "nvmlDeviceGetPowerUsage"),
                limit: sym(lib, "nvmlDeviceGetEnforcedPowerLimit"),
                clock: sym(lib, "nvmlDeviceGetClockInfo"),
                fan: sym(lib, "nvmlDeviceGetFanSpeed"),
                pstate: sym(lib, "nvmlDeviceGetPerformanceState"),
                reasons: sym(lib, "nvmlDeviceGetCurrentClocksEventReasons")
                    .or_else(|| sym(lib, "nvmlDeviceGetCurrentClocksThrottleReasons")),
            })
        }
    }

    /// Reads every GPU; false when the first one gives no reading at all (the caller reopens NVML
    /// after repeated failures).
    pub fn sample(&self, out: &mut Vec<Sample>) -> bool {
        let mut ok = true;
        for (i, &d) in self.devices.iter().enumerate() {
            let p = if i == 0 {
                "gpu".to_string()
            } else {
                format!("gpu{i}")
            };
            let key = |s: &str| format!("{p}.{s}");
            let before = out.len();
            unsafe {
                let mut u = [0u32; 2];
                if (self.util)(d, &mut u) == 0 {
                    push(out, key("util"), Unit::Percent, u[0] as f64);
                    push(out, key("mem_util"), Unit::Percent, u[1] as f64);
                }
                for (name, f) in [("enc_util", self.enc), ("dec_util", self.dec)] {
                    let (mut v, mut period) = (0u32, 0u32);
                    if let Some(f) = f {
                        if f(d, &mut v, &mut period) == 0 {
                            push(out, key(name), Unit::Percent, v as f64);
                        }
                    }
                }
                // Version 2 reports the driver's reserved memory apart, as nvidia-smi does.
                let mut have_mem = false;
                if let Some(f) = self.mem2 {
                    let mut m = MemV2 {
                        version: std::mem::size_of::<MemV2>() as u32 | (2 << 24),
                        total: 0,
                        reserved: 0,
                        free: 0,
                        used: 0,
                    };
                    if f(d, &mut m) == 0 {
                        push(out, key("mem_used"), Unit::Bytes, m.used as f64);
                        push(out, key("mem_total"), Unit::Bytes, m.total as f64);
                        push(out, key("mem_reserved"), Unit::Bytes, m.reserved as f64);
                        have_mem = true;
                    }
                }
                if !have_mem {
                    if let Some(f) = self.mem1 {
                        let mut m = [0u64; 3]; // total, free, used
                        if f(d, &mut m) == 0 {
                            push(out, key("mem_used"), Unit::Bytes, m[2] as f64);
                            push(out, key("mem_total"), Unit::Bytes, m[0] as f64);
                        }
                    }
                }
                let mut v = 0u32;
                if let Some(f) = self.temp {
                    if f(d, 0, &mut v) == 0 {
                        push(out, key("temp"), Unit::Celsius, v as f64);
                    }
                }
                if let Some(f) = self.power {
                    if f(d, &mut v) == 0 {
                        push(out, key("power"), Unit::Watts, v as f64 / 1000.0);
                    }
                }
                if let Some(f) = self.limit {
                    if f(d, &mut v) == 0 {
                        push(out, key("power_limit"), Unit::Watts, v as f64 / 1000.0);
                    }
                }
                if let Some(f) = self.clock {
                    for (name, kind) in CLOCKS {
                        if f(d, kind, &mut v) == 0 {
                            push(out, key(name), Unit::Mhz, v as f64);
                        }
                    }
                }
                if let Some(f) = self.fan {
                    if f(d, &mut v) == 0 {
                        push(out, key("fan"), Unit::Percent, v as f64);
                    }
                }
                if let Some(f) = self.pstate {
                    if f(d, &mut v) == 0 {
                        push(out, key("pstate"), Unit::Count, v as f64);
                    }
                }
                if let Some(f) = self.reasons {
                    let mut r = 0u64;
                    if f(d, &mut r) == 0 {
                        for (name, bit) in REASONS {
                            push(
                                out,
                                key(name),
                                Unit::Percent,
                                if r & bit != 0 { 100.0 } else { 0.0 },
                            );
                        }
                    }
                }
            }
            if i == 0 && out.len() == before {
                ok = false;
            }
        }
        ok
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        unsafe {
            if let Some(f) = self.shutdown {
                f();
            }
            win::FreeLibrary(self.lib);
        }
    }
}
