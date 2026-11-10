use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=src/adapter.m");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos")
        || env::var_os("CARGO_FEATURE_DEVELOPMENT_RECEIVER").is_none()
        || env::var_os("CARGO_FEATURE_ISOLATED_HELPER").is_some()
    {
        return;
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let object = out.join("adapter.o");
    assert!(
        Command::new("xcrun")
            .args(["clang", "-fobjc-arc", "-fmodules", "-Wall", "-Wextra", "-c", "src/adapter.m", "-o"])
            .arg(&object)
            .status().expect("Apple command-line tools").success(),
        "native Bluetooth bridge compilation failed"
    );
    assert!(
        Command::new("xcrun").args(["libtool", "-static", "-o"])
            .arg(out.join("liblivi_native_bt.a")).arg(object)
            .status().expect("Apple libtool").success()
    );
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=livi_native_bt");
    for framework in ["AppKit", "Foundation", "IOBluetooth", "CoreBluetooth"] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
}
