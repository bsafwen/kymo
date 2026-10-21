//! Windows performance counters (PDH). One query holds every counter; each collection updates all of
//! them at once, and PDH computes rates and percentages between the last two collections, that is
//! over exactly one sampling interval. Wildcard instances, such as `(*)`, follow devices that come and go.

use crate::win::{self, from_wide_ptr, wide};
use std::ffi::c_void;

const PDH_MORE_DATA: u32 = 0x8000_07D2;
const PDH_FMT_DOUBLE: u32 = 0x0000_0200;
const PDH_FMT_NOCAP100: u32 = 0x0000_8000;

/// PDH_FMT_COUNTERVALUE_ITEM_W
#[repr(C)]
struct FmtItem {
    name: *const u16,
    status: u32,
    value: f64,
}

/// PDH_RAW_COUNTER_ITEM_W (the nested PDH_RAW_COUNTER flattened; offsets are the same).
#[repr(C)]
struct RawItem {
    name: *const u16,
    status: u32,
    time_low: u32,
    time_high: u32,
    first: i64,
    second: i64,
    multi: u32,
}

/// PDH_FMT_COUNTERVALUE
#[repr(C)]
struct FmtValue {
    status: u32,
    value: f64,
}

fn valid(status: u32) -> bool {
    status <= 1 // PDH_CSTATUS_VALID_DATA or PDH_CSTATUS_NEW_DATA
}

pub struct Query {
    handle: isize,
    counters: Vec<isize>,
    buf: Vec<u64>, // 8-byte aligned, filled by PDH
}

impl Query {
    pub fn open() -> Option<Query> {
        let mut h = 0isize;
        if unsafe { win::PdhOpenQueryW(std::ptr::null(), 0, &mut h) } != 0 {
            return None;
        }
        Some(Query {
            handle: h,
            counters: Vec::new(),
            buf: vec![0; 16 * 1024],
        })
    }

    /// Adds a counter such as `\Processor Information(*)\% Idle Time` (English names, any display
    /// language); returns its index, or None if this PC does not have it.
    pub fn add(&mut self, path: &str) -> Option<usize> {
        let mut c = 0isize;
        let w = wide(path);
        if unsafe { win::PdhAddEnglishCounterW(self.handle, w.as_ptr(), 0, &mut c) } != 0 {
            return None;
        }
        self.counters.push(c);
        Some(self.counters.len() - 1)
    }

    pub fn collect(&mut self) -> bool {
        unsafe { win::PdhCollectQueryData(self.handle) == 0 }
    }

    /// Value of a counter without instances (rates over the last interval).
    pub fn value(&self, i: usize) -> Option<f64> {
        let mut v = FmtValue {
            status: u32::MAX,
            value: 0.0,
        };
        let rc = unsafe {
            win::PdhGetFormattedCounterValue(
                self.counters[i],
                PDH_FMT_DOUBLE | PDH_FMT_NOCAP100,
                std::ptr::null_mut(),
                &mut v as *mut FmtValue as *mut c_void,
            )
        };
        (rc == 0 && valid(v.status) && v.value.is_finite()).then_some(v.value)
    }

    /// Values of a wildcard counter, one per instance.
    pub fn values(&mut self, i: usize, out: &mut Vec<(String, f64)>) {
        out.clear();
        let c = self.counters[i];
        let Some(count) = self.fetch(|size, count, buf| unsafe {
            win::PdhGetFormattedCounterArrayW(
                c,
                PDH_FMT_DOUBLE | PDH_FMT_NOCAP100,
                size,
                count,
                buf,
            )
        }) else {
            return;
        };
        let items = self.buf.as_ptr() as *const FmtItem;
        for k in 0..count {
            let it = unsafe { &*items.add(k) };
            if valid(it.status) && it.value.is_finite() {
                out.push((unsafe { from_wide_ptr(it.name) }, it.value));
            }
        }
    }

    /// Raw values of a wildcard counter (for cumulative counters such as energy).
    pub fn raw_values(&mut self, i: usize, out: &mut Vec<(String, i64)>) {
        out.clear();
        let c = self.counters[i];
        let Some(count) = self
            .fetch(|size, count, buf| unsafe { win::PdhGetRawCounterArrayW(c, size, count, buf) })
        else {
            return;
        };
        let items = self.buf.as_ptr() as *const RawItem;
        for k in 0..count {
            let it = unsafe { &*items.add(k) };
            if valid(it.status) {
                out.push((unsafe { from_wide_ptr(it.name) }, it.first));
            }
        }
    }

    fn fetch(&mut self, f: impl Fn(*mut u32, *mut u32, *mut c_void) -> u32) -> Option<usize> {
        for _ in 0..4 {
            let mut size = (self.buf.len() * 8) as u32;
            let mut count = 0u32;
            let rc = f(&mut size, &mut count, self.buf.as_mut_ptr() as *mut c_void);
            if rc == PDH_MORE_DATA {
                self.buf.resize(size as usize / 8 + 1024, 0);
                continue;
            }
            return (rc == 0).then_some(count as usize);
        }
        None
    }
}

impl Drop for Query {
    fn drop(&mut self) {
        unsafe {
            win::PdhCloseQuery(self.handle);
        }
    }
}
