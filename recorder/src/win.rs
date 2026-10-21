//! The Windows functions this program uses, declared by hand (no bindings crate), and small
//! helpers around them. Layouts are those of 64-bit Windows.
#![allow(non_snake_case)]

use std::ffi::c_void;

pub type Handle = *mut c_void;

#[link(name = "ntdll")]
extern "system" {
    pub fn NtQuerySystemInformation(
        class: u32,
        buffer: *mut c_void,
        length: u32,
        returned: *mut u32,
    ) -> i32;
}

#[link(name = "pdh")]
extern "system" {
    pub fn PdhOpenQueryW(source: *const u16, user: usize, query: *mut isize) -> u32;
    pub fn PdhAddEnglishCounterW(
        query: isize,
        path: *const u16,
        user: usize,
        counter: *mut isize,
    ) -> u32;
    pub fn PdhCollectQueryData(query: isize) -> u32;
    pub fn PdhGetFormattedCounterArrayW(
        counter: isize,
        format: u32,
        size: *mut u32,
        count: *mut u32,
        items: *mut c_void,
    ) -> u32;
    pub fn PdhGetFormattedCounterValue(
        counter: isize,
        format: u32,
        kind: *mut u32,
        value: *mut c_void,
    ) -> u32;
    pub fn PdhGetRawCounterArrayW(
        counter: isize,
        size: *mut u32,
        count: *mut u32,
        items: *mut c_void,
    ) -> u32;
    pub fn PdhCloseQuery(query: isize) -> u32;
}

#[repr(C)]
struct MemoryStatusEx {
    length: u32,
    memory_load: u32,
    total_phys: u64,
    avail_phys: u64,
    total_page_file: u64,
    avail_page_file: u64,
    total_virtual: u64,
    avail_virtual: u64,
    avail_extended_virtual: u64,
}

#[link(name = "kernel32")]
extern "system" {
    pub fn LoadLibraryW(name: *const u16) -> Handle;
    pub fn GetProcAddress(module: Handle, name: *const u8) -> *const c_void;
    pub fn FreeLibrary(module: Handle) -> i32;
    fn GetDiskFreeSpaceExW(
        root: *const u16,
        available: *mut u64,
        total: *mut u64,
        free: *mut u64,
    ) -> i32;
    fn GlobalMemoryStatusEx(status: *mut MemoryStatusEx) -> i32;
    fn GetCurrentProcess() -> Handle;
    pub fn GetCurrentProcessId() -> u32;
    fn GetProcessTimes(
        process: Handle,
        creation: *mut u64,
        exit: *mut u64,
        kernel: *mut u64,
        user: *mut u64,
    ) -> i32;
    fn SetProcessInformation(process: Handle, class: i32, info: *const c_void, size: u32) -> i32;
    fn CreateMutexW(attributes: *const c_void, initial_owner: i32, name: *const u16) -> Handle;
    fn GetLastError() -> u32;
    fn GetTickCount64() -> u64;
    fn QueryUnbiasedInterruptTime(time: *mut u64) -> i32;
    fn GetActiveProcessorCount(group: u16) -> u32;
    fn TerminateProcess(process: Handle, code: u32) -> i32;
}

/// Ends this process at once with `code`: no DLL detach, no exit handlers (the history is
/// written unbuffered, so nothing is lost). For the hang watchdog.
pub fn terminate(code: u32) -> ! {
    unsafe {
        TerminateProcess(GetCurrentProcess(), code);
    }
    std::process::exit(code as i32)
}

/// SERVICE_TABLE_ENTRYW
#[repr(C)]
pub struct ServiceTableEntry {
    pub name: *const u16,
    pub main: Option<unsafe extern "system" fn(u32, *mut *mut u16)>,
}

/// SERVICE_STATUS
#[repr(C)]
pub struct ServiceStatus {
    pub service_type: u32,
    pub current_state: u32,
    pub controls_accepted: u32,
    pub win32_exit_code: u32,
    pub service_specific_exit_code: u32,
    pub check_point: u32,
    pub wait_hint: u32,
}

pub type ServiceHandler = unsafe extern "system" fn(u32, u32, *mut c_void, *mut c_void) -> u32;

#[link(name = "advapi32")]
extern "system" {
    pub fn StartServiceCtrlDispatcherW(table: *const ServiceTableEntry) -> i32;
    pub fn RegisterServiceCtrlHandlerExW(
        name: *const u16,
        handler: ServiceHandler,
        context: *mut c_void,
    ) -> isize;
    pub fn SetServiceStatus(handle: isize, status: *const ServiceStatus) -> i32;
}

/// Milliseconds since Windows started, sleep and hibernation included, unaffected by clock changes.
pub fn tick_ms() -> i64 {
    unsafe { GetTickCount64() as i64 }
}

/// Milliseconds the PC has been awake since Windows started: like `tick_ms`, but sleep and
/// hibernation are left out.
pub fn awake_ms() -> i64 {
    let mut t = 0u64;
    unsafe { QueryUnbiasedInterruptTime(&mut t) };
    (t / 10_000) as i64
}

/// Logical processors in every processor group.
pub fn logical_processors() -> usize {
    unsafe { GetActiveProcessorCount(0xFFFF) as usize } // ALL_PROCESSOR_GROUPS
}

pub const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004_u32 as i32;
const FILETIME_UNIX_DIFF_MS: i64 = 11_644_473_600_000;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A NUL-terminated UTF-16 string owned by Windows.
///
/// # Safety
/// `p` is null or points to a NUL-terminated UTF-16 string.
pub unsafe fn from_wide_ptr(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut n = 0;
    while *p.add(n) != 0 {
        n += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
}

pub fn filetime_to_unix_ms(ft: i64) -> i64 {
    ft / 10_000 - FILETIME_UNIX_DIFF_MS
}

pub fn unix_ms_to_filetime(ms: i64) -> i64 {
    (ms + FILETIME_UNIX_DIFF_MS) * 10_000
}

/// CPU time used by this process so far (user + kernel), in 100 ns units.
pub fn own_cpu_100ns() -> u64 {
    let (mut c, mut e, mut k, mut u) = (0u64, 0u64, 0u64, 0u64);
    unsafe {
        if GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) == 0 {
            return 0;
        }
    }
    k + u
}

pub fn memory_total() -> Option<u64> {
    let mut m = MemoryStatusEx {
        length: std::mem::size_of::<MemoryStatusEx>() as u32,
        memory_load: 0,
        total_phys: 0,
        avail_phys: 0,
        total_page_file: 0,
        avail_page_file: 0,
        total_virtual: 0,
        avail_virtual: 0,
        avail_extended_virtual: 0,
    };
    (unsafe { GlobalMemoryStatusEx(&mut m) } != 0).then_some(m.total_phys)
}

/// Size and free space of a volume such as "C:".
pub fn disk_space(volume: &str) -> Option<(u64, u64)> {
    let root = wide(&format!("{volume}\\"));
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    (unsafe { GetDiskFreeSpaceExW(root.as_ptr(), &mut avail, &mut total, &mut free) } != 0)
        .then_some((total, free))
}

/// Total size and use of all page files, in bytes (SystemPageFileInformation).
pub fn pagefiles() -> Option<(u64, u64)> {
    let mut buf = vec![0u64; 1024];
    let mut ret = 0u32;
    let rc = unsafe {
        NtQuerySystemInformation(
            18,
            buf.as_mut_ptr() as *mut c_void,
            (buf.len() * 8) as u32,
            &mut ret,
        )
    };
    if rc < 0 {
        return None;
    }
    let base = buf.as_ptr() as *const u8;
    let (mut total, mut used, mut off) = (0u64, 0u64, 0usize);
    if ret == 0 {
        return Some((0, 0));
    }
    loop {
        // NextEntryOffset, TotalSize and TotalInUse (pages), PeakUsage, PageFileName.
        let next: u32 = read(base, off);
        let size: u32 = read(base, off + 4);
        let in_use: u32 = read(base, off + 8);
        total += size as u64 * 4096;
        used += in_use as u64 * 4096;
        if next == 0 || off + next as usize >= ret as usize {
            break;
        }
        off += next as usize;
    }
    Some((total, used))
}

/// When Windows started (SystemTimeOfDayInformation, exactly 48 bytes: a longer buffer is
/// refused), in Unix milliseconds.
pub fn boot_time_ms() -> Option<i64> {
    let mut buf = [0u64; 6];
    let mut ret = 0u32;
    let rc = unsafe { NtQuerySystemInformation(3, buf.as_mut_ptr() as *mut c_void, 48, &mut ret) };
    (rc >= 0 && buf[0] != 0).then(|| filetime_to_unix_ms(buf[0] as i64))
}

/// Owns a named mutex for the life of the process, or None if another process already does.
pub fn single_instance(name: &str) -> Option<Handle> {
    let w = wide(name);
    let h = unsafe { CreateMutexW(std::ptr::null(), 1, w.as_ptr()) };
    if h.is_null() || unsafe { GetLastError() } == 183 {
        return None; // ERROR_ALREADY_EXISTS
    }
    Some(h)
}

/// Memory priority 5 (normal). A task started with priority 5 gets normal CPU and I/O priority
/// but memory priority 4.
pub fn normal_memory_priority() {
    let normal: u32 = 5;
    unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            0,
            &normal as *const u32 as *const c_void,
            4,
        );
    }
}

/// Reads a `T` at a byte offset of a buffer Windows filled, whatever its alignment.
pub fn read<T: Copy>(base: *const u8, off: usize) -> T {
    unsafe { std::ptr::read_unaligned(base.add(off) as *const T) }
}

#[cfg(test)]
mod tests {
    #[test]
    fn boot_time() {
        let boot = super::boot_time_ms().expect("boot time");
        let now = crate::units::now_ms();
        assert!(boot < now && now - boot < 400 * crate::units::DAY_MS);
    }

    #[test]
    fn clocks_and_processors() {
        let (awake, tick) = (super::awake_ms(), super::tick_ms());
        assert!(awake > 0 && awake <= tick + 1000);
        assert!(super::logical_processors() >= 1);
    }
}
