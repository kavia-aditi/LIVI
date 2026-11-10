// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

#[cfg(all(target_os = "macos", feature = "native-projection"))]
pub use livi_host_proto::isolated_runtime;

#[cfg(target_os = "macos")]
mod control;
#[cfg(target_os = "macos")]
pub mod experimental_metal;
#[cfg(target_os = "macos")]
mod main_thread;
#[cfg(target_os = "macos")]
mod screens;
#[cfg(target_os = "macos")]
mod server;

#[cfg(all(target_os = "macos", feature = "native-projection"))]
mod native_renderer;

#[cfg(all(target_os = "macos", feature = "native-projection"))]
pub fn enable_native_projection() -> bool {
    native_renderer::initialize()
}

#[cfg(all(target_os = "macos", feature = "native-projection"))]
pub fn native_projection_tick() {
    native_renderer::tick();
}

#[cfg(all(target_os = "macos", feature = "native-projection"))]
pub fn stop_native_projection() {
    native_renderer::stop();
}

#[cfg(target_os = "macos")]
pub fn serve(control: &std::path::Path, planes: &std::path::Path) -> std::io::Result<()> {
    server::serve(control, planes)
}

#[cfg(target_os = "macos")]
pub fn set_window(screen: &str, ns_view: usize) {
    screens::set_window(screen, ns_view);
}
