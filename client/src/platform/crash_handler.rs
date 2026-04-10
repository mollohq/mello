use std::sync::OnceLock;

static LOG_DIR: OnceLock<std::path::PathBuf> = OnceLock::new();

pub fn set_log_dir(dir: std::path::PathBuf) {
    LOG_DIR.set(dir).ok();
}

pub fn install() {
    unsafe {
        SetUnhandledExceptionFilter(Some(crash_filter));
    }
}

unsafe extern "system" fn crash_filter(info: *const ExceptionPointers) -> i32 {
    let record = unsafe { &*(*info).exception_record };
    let code = record.exception_code;
    let addr = record.exception_address as u64;

    let module = module_name_for_address(record.exception_address);

    log::error!(
        "CRASH: exception 0x{:08X} at 0x{:016X} in {}",
        code,
        addr,
        module.as_deref().unwrap_or("unknown")
    );

    if let Some(dir) = LOG_DIR.get() {
        let path = dir.join("crash.log");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            use std::io::Write;
            let _ = writeln!(
                f,
                "CRASH: exception 0x{:08X} at 0x{:016X} in {}",
                code,
                addr,
                module.as_deref().unwrap_or("unknown")
            );
            let _ = f.flush();
        }
    }

    0 // EXCEPTION_CONTINUE_SEARCH
}

fn module_name_for_address(addr: *const std::ffi::c_void) -> Option<String> {
    unsafe {
        let mut module: *mut std::ffi::c_void = std::ptr::null_mut();
        let ok = GetModuleHandleExW(
            0x0004, // GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
            addr as *const u16,
            &mut module,
        );
        if ok == 0 || module.is_null() {
            return None;
        }
        let mut buf = [0u16; 260];
        let len = GetModuleFileNameW(module, buf.as_mut_ptr(), buf.len() as u32);
        if len == 0 {
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

// --- Win32 FFI ---

type ExceptionFilter = unsafe extern "system" fn(info: *const ExceptionPointers) -> i32;

#[repr(C)]
struct ExceptionPointers {
    exception_record: *const ExceptionRecord,
    _context_record: *const std::ffi::c_void,
}

#[repr(C)]
struct ExceptionRecord {
    exception_code: u32,
    _exception_flags: u32,
    _exception_record: *const ExceptionRecord,
    exception_address: *const std::ffi::c_void,
    _number_parameters: u32,
    _exception_information: [usize; 15],
}

unsafe impl Send for ExceptionPointers {}
unsafe impl Sync for ExceptionPointers {}

extern "system" {
    fn SetUnhandledExceptionFilter(filter: Option<ExceptionFilter>) -> Option<ExceptionFilter>;

    fn GetModuleHandleExW(
        flags: u32,
        module_name: *const u16,
        module: *mut *mut std::ffi::c_void,
    ) -> i32;

    fn GetModuleFileNameW(module: *mut std::ffi::c_void, filename: *mut u16, size: u32) -> u32;
}
