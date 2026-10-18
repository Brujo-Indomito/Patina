//! Minimal logging for code running inside the Frame Server process.
//!
//! Messages go to `OutputDebugString` (view with DebugView or a debugger) and,
//! best effort, to `%ProgramData%\RustVCam\source.log`. The install script
//! grants the Frame Server's service account write access to that folder.

use std::fs::OpenOptions;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use windows::Win32::System::Diagnostics::Debug::OutputDebugStringW;
use windows_core::HSTRING;

pub fn write(message: &str) {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let line = format!("[rust-vcam pid={} t={}] {}\n", std::process::id(), millis, message);

    unsafe { OutputDebugStringW(&HSTRING::from(line.as_str())) };

    if let Some(dir) = std::env::var_os("ProgramData") {
        let path = std::path::Path::new(&dir).join("RustVCam").join("source.log");
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = file.write_all(line.as_bytes());
        }
    }
}

macro_rules! log {
    ($($arg:tt)*) => { $crate::log::write(&format!($($arg)*)) };
}

pub(crate) use log;

/// Logs a failed COM call before returning it to the caller.
pub fn trace<T>(name: &str, result: windows_core::Result<T>) -> windows_core::Result<T> {
    if let Err(err) = &result {
        write(&format!("{name} failed: {err}"));
    }
    result
}
