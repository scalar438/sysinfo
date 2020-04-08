//
// Sysinfo
//
// Copyright (c) 2017 Guillaume Gomez
//

use std::collections::HashMap;
use std::mem;
use std::ops::DerefMut;
use std::ptr::null_mut;
use std::sync::Mutex;

use windows::tools::KeyHandler;
use LoadAvg;
use ProcessorExt;

use ntapi::ntpoapi::PROCESSOR_POWER_INFORMATION;

use winapi::shared::minwindef::FALSE;
use winapi::shared::winerror::ERROR_SUCCESS;
use winapi::um::handleapi::CloseHandle;
use winapi::um::pdh::{
    PdhAddCounterW, PdhAddEnglishCounterA, PdhCloseQuery, PdhCollectQueryData,
    PdhCollectQueryDataEx, PdhGetFormattedCounterValue, PdhOpenQueryA, PdhRemoveCounter,
    PDH_FMT_COUNTERVALUE, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY,
};
use winapi::um::powerbase::CallNtPowerInformation;
use winapi::um::synchapi::CreateEventA;
use winapi::um::sysinfoapi::SYSTEM_INFO;
use winapi::um::winbase::{RegisterWaitForSingleObject, INFINITE};
use winapi::um::winnt::{ProcessorInformation, BOOLEAN, HANDLE, PVOID, WT_EXECUTEDEFAULT};

// This formula comes from linux's include/linux/sched/loadavg.h
// https://github.com/torvalds/linux/blob/345671ea0f9258f410eb057b9ced9cefbbe5dc78/include/linux/sched/loadavg.h#L20-L23
const LOADAVG_FACTOR_1F: f64 = 0.9200444146293232478931553241;
const LOADAVG_FACTOR_5F: f64 = 0.6592406302004437462547604110;
const LOADAVG_FACTOR_15F: f64 = 0.2865047968601901003248854266;
// The time interval in seconds between taking load counts, same as Linux
const SAMPLING_INTERVAL: usize = 5;

// maybe use a read/write lock instead?
static LOAD_AVG: once_cell::sync::Lazy<Mutex<Option<LoadAvg>>> =
    once_cell::sync::Lazy::new(|| unsafe { init_load_avg() });

pub(crate) fn get_load_average() -> LoadAvg {
    if let Ok(avg) = LOAD_AVG.lock() {
        if let Some(avg) = &*avg {
            return avg.clone();
        }
    }
    return LoadAvg::default();
}

unsafe extern "system" fn load_avg_callback(counter: PVOID, _: BOOLEAN) {
    let mut display_value: PDH_FMT_COUNTERVALUE = mem::MaybeUninit::uninit().assume_init();

    if PdhGetFormattedCounterValue(counter as _, PDH_FMT_DOUBLE, null_mut(), &mut display_value)
        != ERROR_SUCCESS as _
    {
        return;
    }
    if let Ok(mut avg) = LOAD_AVG.lock() {
        if let Some(avg) = avg.deref_mut() {
            let current_load = display_value.u.doubleValue();

            avg.one = avg.one * LOADAVG_FACTOR_1F + current_load * (1.0 - LOADAVG_FACTOR_1F);
            avg.five = avg.five * LOADAVG_FACTOR_5F + current_load * (1.0 - LOADAVG_FACTOR_5F);
            avg.fifteen =
                avg.fifteen * LOADAVG_FACTOR_15F + current_load * (1.0 - LOADAVG_FACTOR_15F);
        }
    }
}

unsafe fn init_load_avg() -> Mutex<Option<LoadAvg>> {
    // You can see the original implementation here: https://github.com/giampaolo/psutil
    let mut query = null_mut();

    if PdhOpenQueryA(null_mut(), 0, &mut query) != ERROR_SUCCESS as _ {
        return Mutex::new(None);
    }

    let mut counter: PDH_HCOUNTER = mem::zeroed();
    if PdhAddEnglishCounterA(
        query,
        b"\\System\\Processor Queue Length\0".as_ptr() as _,
        0,
        &mut counter,
    ) != ERROR_SUCCESS as _
    {
        PdhCloseQuery(query);
        return Mutex::new(None);
    }

    let event = CreateEventA(null_mut(), FALSE, FALSE, b"LoadUpdateEvent\0".as_ptr() as _);
    if event.is_null() {
        PdhCloseQuery(query);
        return Mutex::new(None);
    }

    if PdhCollectQueryDataEx(query, SAMPLING_INTERVAL as _, event) != ERROR_SUCCESS as _ {
        PdhCloseQuery(query);
        return Mutex::new(None);
    }

    let mut wait_handle = null_mut();
    if RegisterWaitForSingleObject(
        &mut wait_handle,
        event,
        Some(load_avg_callback),
        counter as _,
        INFINITE,
        WT_EXECUTEDEFAULT,
    ) == 0
    {
        PdhRemoveCounter(counter);
        PdhCloseQuery(query);
        Mutex::new(None)
    } else {
        Mutex::new(Some(LoadAvg::default()))
    }
}

struct InternalQuery {
    query: PDH_HQUERY,
    event: HANDLE,
    data: HashMap<String, PDH_HCOUNTER>,
}

unsafe impl Send for InternalQuery {}
unsafe impl Sync for InternalQuery {}

impl Drop for InternalQuery {
    fn drop(&mut self) {
        unsafe {
            for (_, counter) in self.data.iter() {
                PdhRemoveCounter(*counter);
            }

            if !self.event.is_null() {
                CloseHandle(self.event);
            }

            if !self.query.is_null() {
                PdhCloseQuery(self.query);
            }
        }
    }
}

pub struct Query {
    internal: InternalQuery,
}

impl Query {
    pub fn new() -> Option<Query> {
        let mut query = null_mut();
        unsafe {
            if PdhOpenQueryA(null_mut(), 0, &mut query) == ERROR_SUCCESS as i32 {
                let q = InternalQuery {
                    query,
                    event: null_mut(),
                    data: HashMap::new(),
                };
                Some(Query { internal: q })
            } else {
                None
            }
        }
    }

    pub fn get(&self, name: &String) -> Option<f32> {
        if let Some(ref counter) = self.internal.data.get(name) {
            unsafe {
                let mut display_value: PDH_FMT_COUNTERVALUE =
                    mem::MaybeUninit::uninit().assume_init();
                let counter: PDH_HCOUNTER = **counter;

                let ret = PdhGetFormattedCounterValue(
                    counter,
                    PDH_FMT_DOUBLE,
                    null_mut(),
                    &mut display_value,
                ) as u32;
                return if ret == ERROR_SUCCESS as _ {
                    let data = *display_value.u.doubleValue();
                    Some(data as f32)
                } else {
                    Some(0.)
                };
            }
        }
        None
    }

    pub fn add_counter(&mut self, name: &String, getter: Vec<u16>) -> bool {
        if self.internal.data.contains_key(name) {
            return false;
        }
        unsafe {
            let mut counter: PDH_HCOUNTER = ::std::mem::zeroed();
            let ret = PdhAddCounterW(self.internal.query, getter.as_ptr(), 0, &mut counter);
            if ret == ERROR_SUCCESS as _ {
                self.internal.data.insert(name.clone(), counter);
            } else {
                sysinfo_debug!("failed to add counter '{}': {:x}...", name, ret);
                return false;
            }
        }
        true
    }

    pub fn refresh(&self) {
        unsafe {
            if PdhCollectQueryData(self.internal.query) != ERROR_SUCCESS as _ {
                sysinfo_debug!("failed to refresh CPU data");
            }
        }
    }
}

/// Struct containing a processor information.
pub struct Processor {
    name: String,
    cpu_usage: f32,
    key_used: Option<KeyHandler>,
    vendor_id: String,
    brand: String,
    frequency: u64,
}

impl ProcessorExt for Processor {
    fn get_cpu_usage(&self) -> f32 {
        self.cpu_usage
    }

    fn get_name(&self) -> &str {
        &self.name
    }

    fn get_frequency(&self) -> u64 {
        self.frequency
    }

    fn get_vendor_id(&self) -> &str {
        &self.vendor_id
    }

    fn get_brand(&self) -> &str {
        &self.brand
    }
}

impl Processor {
    pub(crate) fn new_with_values(
        name: &str,
        vendor_id: String,
        brand: String,
        frequency: u64,
    ) -> Processor {
        Processor {
            name: name.to_owned(),
            cpu_usage: 0f32,
            key_used: None,
            vendor_id,
            brand,
            frequency,
        }
    }

    pub(crate) fn set_cpu_usage(&mut self, value: f32) {
        self.cpu_usage = value;
    }
}

fn get_vendor_id_not_great(info: &SYSTEM_INFO) -> String {
    use winapi::um::winnt;
    // https://docs.microsoft.com/fr-fr/windows/win32/api/sysinfoapi/ns-sysinfoapi-system_info
    match unsafe { info.u.s() }.wProcessorArchitecture {
        winnt::PROCESSOR_ARCHITECTURE_INTEL => "Intel x86",
        winnt::PROCESSOR_ARCHITECTURE_MIPS => "MIPS",
        winnt::PROCESSOR_ARCHITECTURE_ALPHA => "RISC Alpha",
        winnt::PROCESSOR_ARCHITECTURE_PPC => "PPC",
        winnt::PROCESSOR_ARCHITECTURE_SHX => "SHX",
        winnt::PROCESSOR_ARCHITECTURE_ARM => "ARM",
        winnt::PROCESSOR_ARCHITECTURE_IA64 => "Intel Itanium-based x64",
        winnt::PROCESSOR_ARCHITECTURE_ALPHA64 => "RISC Alpha x64",
        winnt::PROCESSOR_ARCHITECTURE_MSIL => "MSIL",
        winnt::PROCESSOR_ARCHITECTURE_AMD64 => "(Intel or AMD) x64",
        winnt::PROCESSOR_ARCHITECTURE_IA32_ON_WIN64 => "Intel Itanium-based x86",
        winnt::PROCESSOR_ARCHITECTURE_NEUTRAL => "unknown",
        winnt::PROCESSOR_ARCHITECTURE_ARM64 => "ARM x64",
        winnt::PROCESSOR_ARCHITECTURE_ARM32_ON_WIN64 => "ARM",
        winnt::PROCESSOR_ARCHITECTURE_IA32_ON_ARM64 => "Intel Itanium-based x86",
        _ => "unknown",
    }
    .to_owned()
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub fn get_vendor_id_and_brand(info: &SYSTEM_INFO) -> (String, String) {
    #[cfg(target_arch = "x86")]
    use std::arch::x86::__cpuid;
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::__cpuid;

    fn add_u32(v: &mut Vec<u8>, i: u32) {
        let i = &i as *const u32 as *const u8;
        unsafe {
            v.push(*i);
            v.push(*i.offset(1));
            v.push(*i.offset(2));
            v.push(*i.offset(3));
        }
    }

    // First, we try to get the complete name.
    let res = unsafe { __cpuid(0x80000000) };
    let n_ex_ids = res.eax;
    let brand = if n_ex_ids >= 0x80000004 {
        let mut extdata = Vec::with_capacity(5);

        for i in 0x80000000..=n_ex_ids {
            extdata.push(unsafe { __cpuid(i) });
        }

        let mut out = Vec::with_capacity(4 * 4 * 3); // 4 * u32 * nb_entries
        for i in 2..5 {
            add_u32(&mut out, extdata[i].eax);
            add_u32(&mut out, extdata[i].ebx);
            add_u32(&mut out, extdata[i].ecx);
            add_u32(&mut out, extdata[i].edx);
        }
        let mut pos = 0;
        for e in out.iter() {
            if *e == 0 {
                break;
            }
            pos += 1;
        }
        match ::std::str::from_utf8(&out[..pos]) {
            Ok(s) => s.to_owned(),
            _ => String::new(),
        }
    } else {
        String::new()
    };

    // Failed to get full name, let's retry for the short version!
    let res = unsafe { __cpuid(0) };
    let mut x = Vec::with_capacity(16); // 3 * u32
    add_u32(&mut x, res.ebx);
    add_u32(&mut x, res.edx);
    add_u32(&mut x, res.ecx);
    let mut pos = 0;
    for e in x.iter() {
        if *e == 0 {
            break;
        }
        pos += 1;
    }
    let vendor_id = match ::std::str::from_utf8(&x[..pos]) {
        Ok(s) => s.to_owned(),
        Err(_) => get_vendor_id_not_great(info),
    };
    (vendor_id, brand)
}

#[cfg(all(not(target_arch = "x86_64"), not(target_arch = "x86")))]
pub fn get_vendor_id_and_brand(info: &SYSTEM_INFO) -> (String, String) {
    (get_vendor_id_not_great(info), String::new())
}

pub fn get_key_used(p: &mut Processor) -> &mut Option<KeyHandler> {
    &mut p.key_used
}

// From https://stackoverflow.com/a/43813138:
//
// If your PC has 64 or fewer logical processors installed, the above code will work fine. However,
// if your PC has more than 64 logical processors installed, use GetActiveProcessorCount() or
// GetLogicalProcessorInformation() to determine the total number of logical processors installed.
pub fn get_frequencies(nb_processors: usize) -> Vec<u64> {
    let size = nb_processors * mem::size_of::<PROCESSOR_POWER_INFORMATION>();
    let mut infos: Vec<PROCESSOR_POWER_INFORMATION> = Vec::with_capacity(nb_processors);

    if unsafe {
        CallNtPowerInformation(
            ProcessorInformation,
            null_mut(),
            0,
            infos.as_mut_ptr() as _,
            size as _,
        )
    } == 0
    {
        unsafe {
            infos.set_len(nb_processors);
        }
        // infos.Number
        infos
            .into_iter()
            .map(|i| i.CurrentMhz as u64)
            .collect::<Vec<_>>()
    } else {
        vec![0; nb_processors]
    }
}
