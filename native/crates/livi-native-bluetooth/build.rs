use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=src/adapter.m");
    println!("cargo:rerun-if-changed=src/paired.m");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_OFFLINE_NATIVE_TESTS");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let object = out.join("adapter.o");
    let mut compiler = Command::new("xcrun");
    compiler.args([
            "clang",
            "-fobjc-arc",
            "-fmodules",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-c",
            "src/adapter.m",
            "-o",
        ])
        .arg(&object);
    if env::var_os("CARGO_FEATURE_OFFLINE_NATIVE_TESTS").is_some() {
        compiler.arg("-DLIVI_OFFLINE_INQUIRY_TESTS=1");
    }
    let status = compiler.status().expect("Apple command-line tools");
    assert!(
        status.success(),
        "native Bluetooth bridge compilation failed"
    );
    let paired = out.join("paired.o");
    let status = Command::new("xcrun")
        .args([
            "clang", "-fobjc-arc", "-fmodules", "-Wall", "-Wextra", "-Werror",
            "-c", "src/paired.m", "-o",
        ])
        .arg(&paired)
        .status()
        .expect("Apple command-line tools");
    assert!(status.success(), "paired-device bridge compilation failed");
    let status = Command::new("xcrun")
        .args(["libtool", "-static", "-o"])
        .arg(out.join("liblivi_native_bluetooth.a"))
        .arg(&object)
        .arg(&paired)
        .status()
        .expect("Apple libtool");
    assert!(status.success(), "native Bluetooth archive failed");
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=livi_native_bluetooth");
    for framework in ["Foundation", "IOBluetooth", "CoreBluetooth"] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
}
