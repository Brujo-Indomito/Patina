//! Links the source DLL against the static C runtime, so it loads inside the
//! Frame Server process without the VC++ redistributable. (Only this crate:
//! the app links ONNX Runtime, which is built against the dynamic runtime.)

fn main() {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    for dynamic in ["msvcrt.lib", "vcruntime.lib", "ucrt.lib"] {
        println!("cargo:rustc-cdylib-link-arg=/NODEFAULTLIB:{dynamic}");
    }
    for static_lib in ["libcmt.lib", "libvcruntime.lib", "libucrt.lib"] {
        println!("cargo:rustc-cdylib-link-arg={static_lib}");
    }
}
