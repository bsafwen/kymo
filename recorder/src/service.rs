//! Running as a Windows service. The service control manager starts `kymo service --data
//! DIR`, which records exactly as `run` does and stops when asked (stop, shutdown). A recorder that
//! fails, or that a hang watchdog ends (main.rs), stops without saying so: the service control
//! manager then starts it again (the installer sets its recovery actions).

use crate::win::{self, ServiceStatus, ServiceTableEntry};
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, Ordering};
use std::sync::OnceLock;

pub const NAME: &str = "Kymo";

const OWN_PROCESS: u32 = 0x10;
const STOPPED: u32 = 1;
const STOP_PENDING: u32 = 3;
const RUNNING: u32 = 4;
const ACCEPT_STOP: u32 = 0x1;
const ACCEPT_SHUTDOWN: u32 = 0x4;
const CONTROL_STOP: u32 = 1;
const CONTROL_INTERROGATE: u32 = 4;
const CONTROL_SHUTDOWN: u32 = 5;
const ERROR_SERVICE_SPECIFIC: u32 = 1066;
const ERROR_NOT_IMPLEMENTED: u32 = 120;

/// Set when the service is asked to stop; the recording loop checks it.
pub static STOP: AtomicBool = AtomicBool::new(false);
static ARGS: OnceLock<(PathBuf, u16)> = OnceLock::new();
static HANDLE: AtomicIsize = AtomicIsize::new(0);
static EXIT: AtomicI32 = AtomicI32::new(0);

fn status(state: u32, exit: i32, wait_hint: u32) {
    let h = HANDLE.load(Ordering::SeqCst);
    if h == 0 {
        return;
    }
    let s = ServiceStatus {
        service_type: OWN_PROCESS,
        current_state: state,
        controls_accepted: if state == RUNNING {
            ACCEPT_STOP | ACCEPT_SHUTDOWN
        } else {
            0
        },
        win32_exit_code: if exit != 0 { ERROR_SERVICE_SPECIFIC } else { 0 },
        service_specific_exit_code: exit as u32,
        check_point: 0,
        wait_hint,
    };
    unsafe { win::SetServiceStatus(h, &s) };
}

unsafe extern "system" fn handler(
    control: u32,
    _kind: u32,
    _data: *mut c_void,
    _context: *mut c_void,
) -> u32 {
    match control {
        CONTROL_STOP | CONTROL_SHUTDOWN => {
            STOP.store(true, Ordering::SeqCst);
            status(STOP_PENDING, 0, 15_000);
            0
        }
        CONTROL_INTERROGATE => 0,
        _ => ERROR_NOT_IMPLEMENTED,
    }
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut *mut u16) {
    let name = win::wide(NAME);
    let h = win::RegisterServiceCtrlHandlerExW(name.as_ptr(), handler, std::ptr::null_mut());
    if h == 0 {
        return;
    }
    HANDLE.store(h, Ordering::SeqCst);
    status(RUNNING, 0, 0);
    let code = match ARGS.get() {
        Some((data, port)) => crate::run(data.clone(), *port, None, &STOP),
        None => 2,
    };
    EXIT.store(code, Ordering::SeqCst);
    status(STOPPED, code, 0);
}

/// Hands this process to the service control manager; returns when the service has stopped.
/// Started any other way (from a console or a task), it does nothing and returns 2.
pub fn run(data: PathBuf, port: u16) -> i32 {
    let _ = ARGS.set((data, port));
    let name = win::wide(NAME);
    let table = [
        ServiceTableEntry {
            name: name.as_ptr(),
            main: Some(service_main),
        },
        ServiceTableEntry {
            name: std::ptr::null(),
            main: None,
        },
    ];
    if unsafe { win::StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
        return 2;
    }
    EXIT.load(Ordering::SeqCst)
}
