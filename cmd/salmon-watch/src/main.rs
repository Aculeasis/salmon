#![windows_subsystem = "windows"]

#[cfg(windows)]
unsafe extern "system" {
    fn AttachConsole(dw_process_id: u32) -> i32;
    fn GetStdHandle(n_std_handle: u32) -> *mut std::ffi::c_void;
    fn SetStdHandle(n_std_handle: u32, h_handle: *mut std::ffi::c_void) -> i32;
    fn SetCurrentProcessExplicitAppUserModelID(app_id: *const u16) -> i32;
    fn CreateFileW(
        lp_file_name: *const u16,
        dw_desired_access: u32,
        dw_share_mode: u32,
        lp_security_attributes: *mut std::ffi::c_void,
        dw_creation_disposition: u32,
        dw_flags_and_attributes: u32,
        h_template_file: *mut std::ffi::c_void,
    ) -> *mut std::ffi::c_void;
}

#[cfg(windows)]
const ATTACH_PARENT_PROCESS: u32 = 0xFFFFFFFF;
#[cfg(windows)]
const STD_OUTPUT_HANDLE: u32 = 0xFFFFFFF5;
#[cfg(windows)]
const STD_ERROR_HANDLE: u32 = 0xFFFFFFF4;
#[cfg(windows)]
const GENERIC_READ: u32 = 0x80000000;
#[cfg(windows)]
const GENERIC_WRITE: u32 = 0x40000000;
#[cfg(windows)]
const FILE_SHARE_READ: u32 = 1;
#[cfg(windows)]
const FILE_SHARE_WRITE: u32 = 2;
#[cfg(windows)]
const OPEN_EXISTING: u32 = 3;
#[cfg(windows)]
const INVALID_HANDLE_VALUE: *mut std::ffi::c_void = -1isize as *mut std::ffi::c_void;

fn main() {
    #[cfg(windows)]
    {
        salmon_watch::notification::register_app_id();
        unsafe {
            let app_id: Vec<u16> = "Salmon Watch\0".encode_utf16().collect();
            let _ = SetCurrentProcessExplicitAppUserModelID(app_id.as_ptr());

            if AttachConsole(ATTACH_PARENT_PROCESS) != 0 {
                let out_handle = GetStdHandle(STD_OUTPUT_HANDLE);
                if out_handle.is_null() || out_handle == INVALID_HANDLE_VALUE {
                    let conout: Vec<u16> = "CONOUT$\0".encode_utf16().collect();
                    let handle = CreateFileW(
                        conout.as_ptr(),
                        GENERIC_READ | GENERIC_WRITE,
                        FILE_SHARE_READ | FILE_SHARE_WRITE,
                        std::ptr::null_mut(),
                        OPEN_EXISTING,
                        0,
                        std::ptr::null_mut(),
                    );
                    if handle != INVALID_HANDLE_VALUE {
                        SetStdHandle(STD_OUTPUT_HANDLE, handle);
                        SetStdHandle(STD_ERROR_HANDLE, handle);
                    }
                }
            }
        }
    }

    if let Err(error) = salmon_watch::app::execute() {
        if salmon_watch::logging::is_initialized() {
            log::error!("{error:#}");
        } else {
            eprintln!("salmon-watch: {error:#}");
        }
        std::process::exit(1);
    }
}
