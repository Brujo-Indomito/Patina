//! Rust VCam settings and tray app. Creates the virtual camera and feeds it
//! from your webcam; closing the window keeps it running in the tray.
//!
//! `rust-vcam --minimized` starts hidden in the tray (used by "Start with Windows").

#![windows_subsystem = "windows"]

use vcam_app::controller::InstanceGuard;
use vcam_app::settings::MINIMIZED_FLAG;
use vcam_app::{init_thread_pool, ui, FRIENDLY_NAME};

fn main() {
    let Some(_guard) = InstanceGuard::acquire() else {
        rfd::MessageDialog::new()
            .set_title(FRIENDLY_NAME)
            .set_description("Rust VCam is already running. Look for its icon in the system tray.")
            .set_level(rfd::MessageLevel::Info)
            .show();
        return;
    };
    init_thread_pool();
    let start_hidden = std::env::args().any(|arg| arg == MINIMIZED_FLAG);
    if let Err(err) = ui::run(start_hidden) {
        rfd::MessageDialog::new()
            .set_title(FRIENDLY_NAME)
            .set_description(format!("Rust VCam could not start: {err}"))
            .set_level(rfd::MessageLevel::Error)
            .show();
    }
}
