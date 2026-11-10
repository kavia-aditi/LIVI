// SPDX-License-Identifier: GPL-3.0-or-later
#[cfg(all(not(all(target_os = "macos", feature = "isolated-helper")),
    any(test, feature = "development-receiver")))]
mod config;

#[cfg(all(target_os = "macos", feature = "isolated-helper"))]
mod isolated_helper;
#[cfg(all(target_os = "macos", feature = "isolated-helper"))]
#[allow(dead_code)]
#[path = "../../livi-helperd/src/native_tcp.rs"]
mod native_tcp;
#[cfg(all(target_os = "macos", feature = "isolated-helper"))]
#[allow(dead_code)]
#[path = "../../livi-helperd/src/native_handoff.rs"]
mod native_handoff;

#[cfg(all(target_os = "macos", any(feature = "isolated-helper", feature = "development-receiver")))]
#[allow(dead_code)]
#[path = "../../livi-helperd/src/aa.rs"]
mod aa;
#[cfg(all(target_os = "macos", feature = "development-receiver", not(feature = "isolated-helper")))]
mod receiver;

fn main() -> std::process::ExitCode {
    if std::env::args().any(|arg| arg == "--isolated-helper") {
        #[cfg(all(target_os = "macos", feature = "isolated-helper"))]
        return match isolated_helper::run() {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(_) => {
                eprintln!("[native-bt-dev] isolated helper refused or stopped");
                std::process::ExitCode::FAILURE
            }
        };
        #[cfg(not(all(target_os = "macos", feature = "isolated-helper")))]
        {
            eprintln!("Isolated helper support is not compiled in.");
            return std::process::ExitCode::FAILURE;
        }
    }
    #[cfg(all(target_os = "macos", feature = "isolated-helper"))]
    {
        eprintln!("The isolated helper build requires --isolated-helper and explicit authorization.");
        std::process::ExitCode::FAILURE
    }
    #[cfg(all(target_os = "macos", feature = "development-receiver", not(feature = "isolated-helper")))]
    match receiver::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[native-bt-dev] {e}");
            std::process::ExitCode::FAILURE
        }
    }
    #[cfg(any(not(target_os = "macos"), all(not(feature = "isolated-helper"), not(feature = "development-receiver"))))]
    {
        eprintln!("The native Bluetooth development receiver requires macOS and the development-receiver feature.");
        std::process::ExitCode::FAILURE
    }
}
