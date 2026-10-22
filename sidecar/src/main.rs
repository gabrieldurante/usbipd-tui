use std::process::ExitCode;

#[cfg(windows)]
use serde::{Deserialize, Serialize};
#[cfg(windows)]
use std::{env, fs, fs::OpenOptions, io::Write, process::Command};

#[cfg(windows)]
#[derive(Deserialize)]
struct Request {
    args: Vec<String>,
}

#[cfg(windows)]
#[derive(Serialize)]
struct Response {
    code: i32,
    stdout: String,
    stderr: String,
}

fn main() -> ExitCode {
    #[cfg(windows)]
    {
        windows_sidecar::run()
    }
    #[cfg(not(windows))]
    {
        eprintln!("usbipd-tui-elevate must be built for Windows.");
        ExitCode::FAILURE
    }
}

#[cfg(windows)]
mod windows_sidecar {
    use super::*;
    use std::{ffi::c_void, ptr};

    const SEE_MASK_NOCLOSEPROCESS: u32 = 0x0000_0040;
    const SEE_MASK_NO_CONSOLE: u32 = 0x0000_8000;
    const SW_HIDE: i32 = 0;
    const INFINITE: u32 = 0xFFFF_FFFF;
    const ERROR_CANCELLED: u32 = 1223;

    #[repr(C)]
    struct ShellExecuteInfoW {
        cb_size: u32,
        f_mask: u32,
        hwnd: *mut c_void,
        lp_verb: *const u16,
        lp_file: *const u16,
        lp_parameters: *const u16,
        lp_directory: *const u16,
        n_show: i32,
        h_inst_app: *mut c_void,
        lp_id_list: *mut c_void,
        lp_class: *const u16,
        hkey_class: *mut c_void,
        dw_hot_key: u32,
        h_icon_or_monitor: *mut c_void,
        h_process: *mut c_void,
    }

    #[link(name = "shell32")]
    unsafe extern "system" {
        fn ShellExecuteExW(info: *mut ShellExecuteInfoW) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetLastError() -> u32;
        fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
        fn GetExitCodeProcess(handle: *mut c_void, exit_code: *mut u32) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }

    fn quote_windows_arg(value: &str) -> String {
        let mut quoted = String::from("\"");
        let mut slashes = 0usize;
        for ch in value.chars() {
            if ch == '\\' {
                slashes += 1;
            } else if ch == '"' {
                quoted.push_str(&"\\".repeat(slashes * 2 + 1));
                quoted.push('"');
                slashes = 0;
            } else {
                quoted.push_str(&"\\".repeat(slashes));
                quoted.push(ch);
                slashes = 0;
            }
        }
        quoted.push_str(&"\\".repeat(slashes * 2));
        quoted.push('"');
        quoted
    }

    fn trusted_temp_path(path: &str, suffix: &str) -> bool {
        let normalized = path.replace('/', "\\").to_ascii_lowercase();
        normalized.starts_with("c:\\windows\\temp\\wsl2_usb_")
            && normalized.ends_with(suffix)
            && !normalized.contains("..")
    }

    fn allowed_usbipd_args(args: &[String]) -> bool {
        let valid_token = |value: &str| {
            !value.is_empty()
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
        };
        match args.first().map(String::as_str) {
            Some("bind") if args.len() == 3 => {
                if args[1] == "--busid" {
                    valid_token(&args[2])
                } else if args[1] == "--hardware-id" {
                    let parts: Vec<_> = args[2].split(':').collect();
                    parts.len() == 2
                        && parts.iter().all(|part| {
                            part.len() == 4 && part.bytes().all(|b| b.is_ascii_hexdigit())
                        })
                } else {
                    false
                }
            }
            Some("unbind") if args.len() == 2 && args[1] == "--all" => true,
            Some("unbind") if args.len() == 3 => {
                matches!(args[1].as_str(), "--busid" | "--guid") && valid_token(&args[2])
            }
            _ => false,
        }
    }

    fn write_response(path: &str, response: &Response) -> Result<(), String> {
        if !trusted_temp_path(path, ".response.json") {
            return Err("Rejected response path outside Windows Temp.".into());
        }
        let data = serde_json::to_vec(response).map_err(|error| error.to_string())?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| format!("Could not create helper response: {error}"))?;
        file.write_all(&data)
            .map_err(|error| format!("Could not write helper response: {error}"))
    }

    fn execute_elevated(request_path: &str, response_path: &str) -> i32 {
        if !trusted_temp_path(request_path, ".request.json")
            || !trusted_temp_path(response_path, ".response.json")
        {
            let response = Response {
                code: 1,
                stdout: String::new(),
                stderr: "Rejected request or response path outside Windows Temp.".into(),
            };
            let _ = write_response(response_path, &response);
            return 1;
        }
        let request: Request = match fs::read(request_path)
            .map_err(|error| error.to_string())
            .and_then(|data| serde_json::from_slice(&data).map_err(|error| error.to_string()))
        {
            Ok(request) => request,
            Err(error) => {
                let _ = write_response(
                    response_path,
                    &Response {
                        code: 1,
                        stdout: String::new(),
                        stderr: format!("Could not read elevation request: {error}"),
                    },
                );
                return 1;
            }
        };
        if !allowed_usbipd_args(&request.args) {
            let _ = write_response(
                response_path,
                &Response {
                    code: 1,
                    stdout: String::new(),
                    stderr: "The elevation helper only permits usbipd bind/unbind operations."
                        .into(),
                },
            );
            return 1;
        }

        let result = Command::new("usbipd.exe").args(&request.args).output();
        let response = match result {
            Ok(output) => Response {
                code: output.status.code().unwrap_or(1),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            },
            Err(error) => Response {
                code: 1,
                stdout: String::new(),
                stderr: format!("Could not run usbipd.exe: {error}"),
            },
        };
        let code = response.code;
        if let Err(error) = write_response(response_path, &response) {
            eprintln!("{error}");
            return 1;
        }
        code
    }

    fn elevate(request_path: &str, response_path: &str) -> Result<i32, String> {
        let exe = env::current_exe().map_err(|error| error.to_string())?;
        let exe = exe.to_string_lossy();
        let params = format!(
            "--elevated {} {}",
            quote_windows_arg(request_path),
            quote_windows_arg(response_path)
        );
        let verb = wide("runas");
        let exe = wide(&exe);
        let params = wide(&params);
        let mut info = ShellExecuteInfoW {
            cb_size: std::mem::size_of::<ShellExecuteInfoW>() as u32,
            f_mask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NO_CONSOLE,
            hwnd: ptr::null_mut(),
            lp_verb: verb.as_ptr(),
            lp_file: exe.as_ptr(),
            lp_parameters: params.as_ptr(),
            lp_directory: ptr::null(),
            n_show: SW_HIDE,
            h_inst_app: ptr::null_mut(),
            lp_id_list: ptr::null_mut(),
            lp_class: ptr::null(),
            hkey_class: ptr::null_mut(),
            dw_hot_key: 0,
            h_icon_or_monitor: ptr::null_mut(),
            h_process: ptr::null_mut(),
        };
        let started = unsafe { ShellExecuteExW(&mut info) };
        if started == 0 {
            let error = unsafe { GetLastError() };
            let message = if error == ERROR_CANCELLED {
                "UAC request was cancelled by the user.".to_owned()
            } else {
                format!(
                    "Could not request UAC elevation: {}",
                    std::io::Error::from_raw_os_error(error as i32)
                )
            };
            write_response(
                response_path,
                &Response {
                    code: 1,
                    stdout: String::new(),
                    stderr: message.clone(),
                },
            )?;
            return if error == ERROR_CANCELLED {
                Ok(1223)
            } else {
                Err(message)
            };
        }
        if info.h_process.is_null() {
            return Err("UAC started the helper without returning a process handle.".into());
        }
        unsafe {
            WaitForSingleObject(info.h_process, INFINITE);
            let mut code = 1u32;
            let ok = GetExitCodeProcess(info.h_process, &mut code);
            CloseHandle(info.h_process);
            if ok == 0 {
                Err(format!(
                    "Could not read elevated helper exit code: {}",
                    std::io::Error::last_os_error()
                ))
            } else {
                Ok(code as i32)
            }
        }
    }

    pub(super) fn run() -> ExitCode {
        let mut args = env::args().skip(1);
        let first = args.next();
        let (elevated, request, response) = if first.as_deref() == Some("--elevated") {
            (true, args.next(), args.next())
        } else {
            (false, first, args.next())
        };
        let (Some(request), Some(response)) = (request, response) else {
            return ExitCode::FAILURE;
        };
        if !trusted_temp_path(&request, ".request.json")
            || !trusted_temp_path(&response, ".response.json")
        {
            return ExitCode::FAILURE;
        }
        if elevated {
            return ExitCode::from(execute_elevated(&request, &response).clamp(0, 255) as u8);
        }
        match elevate(&request, &response) {
            Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
            Err(error) => {
                let _ = write_response(
                    &response,
                    &Response {
                        code: 1,
                        stdout: String::new(),
                        stderr: error,
                    },
                );
                ExitCode::FAILURE
            }
        }
    }
}
