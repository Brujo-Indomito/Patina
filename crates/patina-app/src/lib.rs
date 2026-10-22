//! Patina application code, shared by the `patina` settings/tray app
//! and the `patina-cli` command-line tool.

pub mod capture;
pub mod controller;
pub mod devices;
pub mod effects;
/// The app icon, drawn in code (also embedded in the executables by build.rs).
pub mod icon;
pub mod probe;
pub mod publisher;
pub mod segment;
pub mod settings;
pub mod ui;

/// Must match `CLSID_PATINA` in the patina-source crate.
pub const SOURCE_CLSID: &str = "{E3C40D88-220A-4FD1-94D8-BC3F22833AFA}";
pub const FRIENDLY_NAME: &str = "Patina";

/// Worker threads for the row-parallel effect code. Measured on a 20-core
/// machine: 4 threads composite a 720p frame in ~5 ms at about half the CPU of
/// rayon's default one-thread-per-core pool, whose idle threads spin.
/// `RAYON_NUM_THREADS` overrides it.
const EFFECT_THREADS: usize = 4;

/// Sets up the shared rayon pool; call once at startup.
pub fn init_thread_pool() {
    if std::env::var_os("RAYON_NUM_THREADS").is_none() {
        let _ = rayon::ThreadPoolBuilder::new().num_threads(EFFECT_THREADS).build_global();
    }
}
