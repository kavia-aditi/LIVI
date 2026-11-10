// SPDX-License-Identifier: GPL-3.0-or-later
//! Explicit, isolated experimental composition. Never changes the installed helper.
use std::path::PathBuf;

pub fn dir() -> Option<PathBuf> {
    if cfg!(all(feature = "native-bt-dev", target_os = "macos")) {
        std::env::var_os("LIVI_NATIVE_BT_DEV_DIR").filter(|p| !p.is_empty()).map(PathBuf::from)
    } else { None }
}

pub fn validate() -> Result<(), String> {
    if std::env::var_os("LIVI_NATIVE_BT_DEV_DIR").is_none() { return Ok(()); }
    let dir = dir().ok_or("native Bluetooth development mode requires the native-bt-dev macOS build")?;
    validate_directory(&dir)?;
    if std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) != Some(dir.clone()) {
        return Err("development XDG_RUNTIME_DIR must match the receiver".into());
    }
    Ok(())
}

fn validate_directory(dir: &std::path::Path) -> Result<(), String> {
    let name = dir.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if dir.parent() != Some(std::path::Path::new("/tmp")) || !name.starts_with("livi-bt-dev-") {
        return Err("development runtime must be the receiver-created /tmp/livi-bt-dev-<pid> directory".into());
    }
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(&dir).map_err(|e| e.to_string())?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
        return Err("development runtime must be an owned private directory, not a symlink".into());
    }
    Ok(())
}

pub fn cp_helper() -> livi_cp::helper_sock::HelperSock {
    match dir() {
        Some(dir) => livi_cp::helper_sock::HelperSock::new(dir.join("cp-disabled.sock")),
        None => livi_cp::helper_sock::HelperSock::default(),
    }
}

pub fn aa_helper() -> livi_aa_stack::helper_sock::AaHelperSock {
    match dir() {
        Some(dir) => livi_aa_stack::helper_sock::AaHelperSock::new(dir.join("aa.sock")),
        None => livi_aa_stack::helper_sock::AaHelperSock::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};

    #[test]
    fn only_private_owned_receiver_directories_are_accepted() {
        let root = std::env::temp_dir();
        let unique = format!("livi-bt-dev-guard-test-{}", std::process::id());
        let path = PathBuf::from("/tmp").join(&unique);
        std::fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        assert!(validate_directory(&path).is_ok());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(validate_directory(&path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let link = PathBuf::from("/tmp").join(format!("{unique}-link"));
        symlink(&path, &link).unwrap();
        assert!(validate_directory(&link).is_err());
        assert!(validate_directory(&root.join("not-a-receiver")).is_err());
        assert!(validate_directory(std::path::Path::new("/tmp")).is_err());
        std::fs::remove_file(link).unwrap();
        std::fs::remove_dir(path).unwrap();
    }
}
