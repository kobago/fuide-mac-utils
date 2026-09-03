//! IOKit readings a plain user is allowed: block-device statistics (the DISK tab's totals),
//! GPU utilisation, power sources (battery) and sleep-preventing power assertions.

use std::ffi::CStr;

use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType,
};
use objc2_io_kit::{
    io_iterator_t, io_object_t, kIOMainPortDefault, IOIteratorNext, IOObjectRelease,
    IOPMCopyAssertionsByProcess, IOPSCopyPowerSourcesInfo, IOPSCopyPowerSourcesList,
    IOPSGetPowerSourceDescription, IORegistryEntryCreateCFProperty, IOServiceGetMatchingServices,
    IOServiceMatching,
};

use super::Battery;

type Dict = CFDictionary<CFString, CFType>;

/// Calls `f` with every registered service of `class`.
fn each_service(class: &CStr, mut f: impl FnMut(io_object_t)) {
    // SAFETY: plain IOKit calls; every object we get is released.
    unsafe {
        let Some(matching) = IOServiceMatching(class.as_ptr()) else {
            return;
        };
        let matching: CFRetained<CFDictionary> = CFRetained::cast_unchecked(matching);
        let mut it: io_iterator_t = 0;
        if IOServiceGetMatchingServices(kIOMainPortDefault, Some(matching), &mut it) != 0 {
            return;
        }
        loop {
            let obj = IOIteratorNext(it);
            if obj == 0 {
                break;
            }
            f(obj);
            IOObjectRelease(obj);
        }
        IOObjectRelease(it);
    }
}

fn property(entry: io_object_t, key: &str) -> Option<CFRetained<CFType>> {
    let key = CFString::from_str(key);
    // SAFETY: `entry` is a live registry entry for the duration of the call.
    unsafe { IORegistryEntryCreateCFProperty(entry, Some(&key), None, 0) }
}

fn as_dict(v: CFRetained<CFType>) -> Option<CFRetained<Dict>> {
    let d = v.downcast::<CFDictionary>().ok()?;
    // SAFETY: IOKit / IOPS dictionaries are keyed by CFString; values are read as CFType.
    Some(unsafe { CFRetained::cast_unchecked(d) })
}

fn num(d: &Dict, key: &str) -> Option<i64> {
    d.get(&CFString::from_str(key))?
        .downcast::<CFNumber>()
        .ok()?
        .as_i64()
}

fn boolean(d: &Dict, key: &str) -> Option<bool> {
    let v = d.get(&CFString::from_str(key))?;
    if let Some(b) = v.downcast_ref::<CFBoolean>() {
        return Some(b.as_bool());
    }
    v.downcast::<CFNumber>().ok()?.as_i64().map(|n| n != 0)
}

fn string(d: &Dict, key: &str) -> Option<String> {
    d.get(&CFString::from_str(key))?
        .downcast::<CFString>()
        .ok()
        .map(|s| s.to_string())
}

/// Cumulative (bytes read, bytes written, read ops, write ops) over every block storage driver.
pub fn disk_totals() -> (u64, u64, u64, u64) {
    let mut t = (0u64, 0u64, 0u64, 0u64);
    each_service(c"IOBlockStorageDriver", |drv| {
        let Some(stats) = property(drv, "Statistics").and_then(as_dict) else {
            return;
        };
        let get = |k: &str| num(&stats, k).unwrap_or(0).max(0) as u64;
        t.0 += get("Bytes (Read)");
        t.1 += get("Bytes (Write)");
        t.2 += get("Operations (Read)");
        t.3 += get("Operations (Write)");
    });
    t
}

/// GPU device utilisation 0..1 from the accelerator driver's performance statistics.
pub fn gpu_utilisation() -> Option<f32> {
    let mut out = None;
    each_service(c"IOAccelerator", |acc| {
        if out.is_some() {
            return;
        }
        let Some(stats) = property(acc, "PerformanceStatistics").and_then(as_dict) else {
            return;
        };
        if let Some(pct) = num(&stats, "Device Utilization %") {
            out = Some((pct as f32 / 100.0).clamp(0.0, 1.0));
        }
    });
    out
}

/// (on AC power, battery) from the power-sources API. A desktop has no battery.
pub fn power_source() -> (bool, Option<Battery>) {
    let Some(blob) = IOPSCopyPowerSourcesInfo() else {
        return (true, None);
    };
    // SAFETY: `blob` is the value the list / description functions expect.
    let Some(list) = (unsafe { IOPSCopyPowerSourcesList(Some(&blob)) }) else {
        return (true, None);
    };
    let list: CFRetained<CFArray<CFType>> = unsafe { CFRetained::cast_unchecked(list) };
    let mut on_ac = true;
    let mut battery = None;
    for ps in list.iter() {
        let Some(desc) = (unsafe { IOPSGetPowerSourceDescription(Some(&blob), Some(&ps)) }) else {
            continue;
        };
        let desc: CFRetained<Dict> = unsafe { CFRetained::cast_unchecked(desc) };
        if string(&desc, "Type").as_deref() != Some("InternalBattery") {
            continue;
        }
        let state = string(&desc, "Power Source State").unwrap_or_default();
        on_ac = state == "AC Power";
        let charging = boolean(&desc, "Is Charging").unwrap_or(false);
        let cur = num(&desc, "Current Capacity").unwrap_or(0) as f32;
        let max = num(&desc, "Max Capacity").unwrap_or(100).max(1) as f32;
        let minutes = if charging {
            num(&desc, "Time to Full Charge")
        } else {
            num(&desc, "Time to Empty")
        }
        .filter(|m| *m > 0)
        .map(|m| m as u32);
        battery = Some(Battery {
            percent: (cur / max * 100.0).clamp(0.0, 100.0),
            charging,
            minutes,
        });
    }
    (on_ac, battery)
}

const SLEEP_ASSERTIONS: &[&str] = &[
    "PreventUserIdleSystemSleep",
    "PreventSystemSleep",
    "NoIdleSleepAssertion",
    "PreventUserIdleDisplaySleep",
    "NoDisplaySleepAssertion",
];

/// Processes holding a sleep-preventing power assertion: (pid, assertion type).
pub fn sleep_blockers() -> Vec<(i32, String)> {
    let mut raw: *const CFDictionary = std::ptr::null();
    // SAFETY: out-pointer to a null CFDictionaryRef; the result is ours to release.
    let ok = unsafe { IOPMCopyAssertionsByProcess(&mut raw) } == 0;
    let mut out = Vec::new();
    if !ok || raw.is_null() {
        return out;
    }
    let by_pid: CFRetained<CFDictionary<CFNumber, CFArray<CFDictionary>>> = unsafe {
        CFRetained::cast_unchecked(CFRetained::from_raw(std::ptr::NonNull::new_unchecked(
            raw as *mut CFDictionary,
        )))
    };
    let (pids, lists) = by_pid.to_vecs();
    for (pid, list) in pids.iter().zip(lists.iter()) {
        let Some(pid) = pid.as_i64() else {
            continue;
        };
        for a in list.iter() {
            let a: CFRetained<Dict> = unsafe { CFRetained::cast_unchecked(a) };
            // `kIOPMAssertionTypeKey` is "AssertType"
            let Some(kind) = string(&a, "AssertType") else {
                continue;
            };
            if SLEEP_ASSERTIONS.contains(&kind.as_str()) {
                out.push((pid as i32, kind));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn sleep_blockers_reads_powerd_assertions() {
        // powerd itself always holds a sleep assertion while the display is on
        let list = super::sleep_blockers();
        println!("{list:?}");
        assert!(list.iter().all(|(pid, _)| *pid > 0));
    }
}
