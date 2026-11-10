//! Optional NVML v1 ABI, loaded only for a GPU whose runtime state is active.
//! API: https://docs.nvidia.com/deploy/nvml-api/latest/api/group__nvmlDeviceQueries.html
use super::{Gpu, Usage};
use std::ffi::{c_char, c_int, c_uint, c_void, CStr, CString};
type Device = *mut c_void;
type Result = c_int;
#[repr(C)]
#[derive(Default)]
struct Utilization {
    gpu: c_uint,
    memory: c_uint,
}
#[repr(C)]
#[derive(Default)]
struct Memory {
    total: u64,
    free: u64,
    used: u64,
}
pub(super) struct Nvml {
    library: *mut c_void,
    shutdown: unsafe extern "C" fn() -> Result,
    handle: unsafe extern "C" fn(*const c_char, *mut Device) -> Result,
    name: unsafe extern "C" fn(Device, *mut c_char, c_uint) -> Result,
    utilization: unsafe extern "C" fn(Device, *mut Utilization) -> Result,
    temperature: unsafe extern "C" fn(Device, c_uint, *mut c_uint) -> Result,
    memory: unsafe extern "C" fn(Device, *mut Memory) -> Result,
}
impl Nvml {
    pub(super) fn load() -> Option<Self> {
        // SAFETY: static NUL-terminated soname; all symbols are checked before use.
        let library = unsafe {
            libc::dlopen(
                c"libnvidia-ml.so.1".as_ptr(),
                libc::RTLD_NOW | libc::RTLD_LOCAL,
            )
        };
        if library.is_null() {
            return None;
        }
        let loaded = (|| {
            macro_rules! symbol {
                ($name:literal, $ty:ty) => {{
                    let name = CString::new($name).ok()?;
                    // SAFETY: library is live; the explicit type matches the NVML C ABI.
                    let pointer = unsafe { libc::dlsym(library, name.as_ptr()) };
                    if pointer.is_null() {
                        return None;
                    }
                    unsafe { std::mem::transmute::<*mut c_void, $ty>(pointer) }
                }};
            }
            let init = symbol!("nvmlInitWithFlags", unsafe extern "C" fn(c_uint) -> Result);
            let nvml = Self {
                library,
                shutdown: symbol!("nvmlShutdown", unsafe extern "C" fn() -> Result),
                handle: symbol!(
                    "nvmlDeviceGetHandleByPciBusId_v2",
                    unsafe extern "C" fn(*const c_char, *mut Device) -> Result
                ),
                name: symbol!(
                    "nvmlDeviceGetName",
                    unsafe extern "C" fn(Device, *mut c_char, c_uint) -> Result
                ),
                utilization: symbol!(
                    "nvmlDeviceGetUtilizationRates",
                    unsafe extern "C" fn(Device, *mut Utilization) -> Result
                ),
                temperature: symbol!(
                    "nvmlDeviceGetTemperature",
                    unsafe extern "C" fn(Device, c_uint, *mut c_uint) -> Result
                ),
                memory: symbol!(
                    "nvmlDeviceGetMemoryInfo",
                    unsafe extern "C" fn(Device, *mut Memory) -> Result
                ),
            };
            // Lazy initialization: no device state until the PCI-specific lookup.
            // NO_ATTACH prevents even active GPUs from being found on some drivers.
            // The caller has already checked runtime_status before reaching here.
            if unsafe { init(0) } != 0 {
                std::mem::forget(nvml); // No successful init to balance with shutdown.
                return None;
            }
            Some(nvml)
        })();
        if loaded.is_none() {
            unsafe {
                libc::dlclose(library);
            }
        }
        loaded
    }
    pub(super) fn sample(&self, pci: &str, gpu: &mut Gpu) {
        let Ok(pci) = CString::new(pci) else {
            return;
        };
        let mut device = std::ptr::null_mut();
        // SAFETY: live library, checked symbols, initialized NVML, valid out-pointers.
        unsafe {
            if (self.handle)(pci.as_ptr(), &mut device) != 0 || device.is_null() {
                return;
            }
            let mut name = [0 as c_char; 128];
            if (self.name)(device, name.as_mut_ptr(), name.len() as c_uint) == 0 {
                name[127] = 0;
                gpu.name = CStr::from_ptr(name.as_ptr()).to_string_lossy().into_owned();
            }
            let mut utilization = Utilization::default();
            if (self.utilization)(device, &mut utilization) == 0 && utilization.gpu <= 100 {
                gpu.usage = Some(f64::from(utilization.gpu));
            }
            let mut temperature = 0;
            if (self.temperature)(device, 0, &mut temperature) == 0 && temperature <= 150 {
                gpu.temperature = Some(f64::from(temperature));
            }
            let mut memory = Memory::default();
            if (self.memory)(device, &mut memory) == 0 {
                gpu.vram = Usage::new(memory.used, memory.total);
            }
        }
    }
}
impl Drop for Nvml {
    fn drop(&mut self) {
        // SAFETY: every successful init is balanced before unloading its symbols.
        unsafe {
            (self.shutdown)();
            libc::dlclose(self.library);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ffi_layout_matches_nvml_v1_structs() {
        assert_eq!(std::mem::size_of::<Memory>(), 24);
        assert_eq!(std::mem::size_of::<Utilization>(), 8);
    }
}
