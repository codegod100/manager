//! Agent Manager — Android NativeActivity (`android_main`).
//!
//! Desktop sessions use the root crate (`cargo run` / `nix run`). This package
//! is cargo-apk only: egui_term / cursor-agent PTYs are Linux-desktop.

mod app;

#[cfg(target_os = "android")]
pub use app::run_android;

#[cfg(not(target_os = "android"))]
pub use app::run_desktop;

#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(android_app: winit::platform::android::activity::AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Info),
    );
    log::info!("manager android_main start");
    match run_android(android_app) {
        Ok(()) => log::info!("manager run_android returned Ok"),
        Err(e) => log::error!("manager run_android error: {e}"),
    }
}
