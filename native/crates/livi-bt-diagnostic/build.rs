fn main() {
    println!("cargo:rerun-if-changed=Info.plist");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos")
        && std::env::var_os("CARGO_FEATURE_BLUETOOTH_DIAGNOSTIC").is_some()
    {
        let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("manifest directory");
        println!("cargo:rustc-link-arg-bin=livi-bt-diagnostic=-Wl,-sectcreate,__TEXT,__info_plist,{manifest}/Info.plist");
        println!("cargo:rustc-link-lib=framework=CoreFoundation");
    }
}
