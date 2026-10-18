//! Owns the virtual camera and the publisher for the settings/tray app.
//!
//! Media Foundation and the publisher run on a background thread in the
//! multithreaded COM apartment, leaving the main thread to the window (which
//! needs a single-threaded apartment for OLE, file dialogs and the tray).

use std::sync::Arc;
use std::thread::JoinHandle;

use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::Media::MediaFoundation::{
    IMFAsyncCallback, IMFVirtualCamera, MFCreateVirtualCamera, MFShutdown, MFStartup, MFVirtualCameraAccess_CurrentUser,
    MFVirtualCameraLifetime_Session, MFVirtualCameraType_SoftwareCameraSource, MFSTARTUP_FULL, MF_VERSION,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::CreateMutexW;
use windows_core::HSTRING;

use crate::publisher::{self, Config, Control};
use crate::{FRIENDLY_NAME, SOURCE_CLSID};

/// Creates and starts the "Rust VCam" virtual camera. It lives as long as the
/// returned object (session lifetime): call [`remove_virtual_camera`] or exit.
pub fn create_virtual_camera() -> windows_core::Result<IMFVirtualCamera> {
    unsafe {
        let camera = MFCreateVirtualCamera(
            MFVirtualCameraType_SoftwareCameraSource,
            MFVirtualCameraLifetime_Session,
            MFVirtualCameraAccess_CurrentUser,
            &HSTRING::from(FRIENDLY_NAME),
            &HSTRING::from(SOURCE_CLSID),
            None,
        )?;
        camera.Start(None::<&IMFAsyncCallback>)?;
        Ok(camera)
    }
}

pub fn remove_virtual_camera(camera: &IMFVirtualCamera) {
    unsafe {
        let _ = camera.Stop();
        let _ = camera.Remove();
        let _ = camera.Shutdown();
    }
}

/// Explains the common failure of [`create_virtual_camera`].
pub fn describe_camera_error(err: &windows_core::Error) -> String {
    if err.code().0 as u32 == 0x80040154 {
        "The Rust VCam media source is not registered. Run scripts\\install.ps1 as administrator.".into()
    } else {
        format!("Could not create the virtual camera: {err}")
    }
}

/// Held while this process owns the "Rust VCam" camera; a second copy would
/// collide with it.
pub struct InstanceGuard(HANDLE);

unsafe impl Send for InstanceGuard {}

impl InstanceGuard {
    /// Returns `None` if another Rust VCam process is already running.
    pub fn acquire() -> Option<Self> {
        let handle = unsafe { CreateMutexW(None, false, &HSTRING::from("Local\\RustVCam_Instance")) }.ok()?;
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe { let _ = CloseHandle(handle); }
            return None;
        }
        Some(Self(handle))
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        unsafe { let _ = CloseHandle(self.0); }
    }
}

/// Runs the virtual camera and publisher until dropped.
pub struct Controller {
    control: Arc<Control>,
    thread: Option<JoinHandle<()>>,
}

impl Controller {
    pub fn start(config: Config) -> Self {
        let control = Arc::new(Control::new(config, false));
        let thread = {
            let control = Arc::clone(&control);
            std::thread::Builder::new()
                .name("vcam-controller".into())
                .spawn(move || controller_thread(&control))
                .expect("failed to spawn the controller thread")
        };
        Self { control, thread: Some(thread) }
    }

    pub fn control(&self) -> &Control {
        &self.control
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        self.control.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn controller_thread(control: &Control) {
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let started = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) };
    if let Err(err) = &started {
        control.log(format!("Media Foundation failed to start: {err}"));
    }

    // Keep publishing even without a virtual camera, so the preview works.
    let camera = match create_virtual_camera() {
        Ok(camera) => {
            control.log(format!("\"{FRIENDLY_NAME}\" is running. Select it in your video app."));
            Some(camera)
        }
        Err(err) => {
            control.log(describe_camera_error(&err));
            None
        }
    };
    publisher::run(control);

    if let Some(camera) = camera {
        remove_virtual_camera(&camera);
    }
    if started.is_ok() {
        unsafe { let _ = MFShutdown(); }
    }
    if com.is_ok() {
        unsafe { CoUninitialize() };
    }
}
