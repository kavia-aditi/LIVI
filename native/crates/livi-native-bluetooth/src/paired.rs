// SPDX-License-Identifier: GPL-3.0-or-later

//! Local paired inventory only. This API does not start or configure an Adapter.
//! It must be called on the macOS UI main thread. Names/addresses are user data;
//! no Debug implementation or logging is provided.

#[derive(Clone, PartialEq, Eq)]
pub struct PairedDevice {
    pub name: String,
    pub address: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InventoryError {
    WrongThread,
    Unavailable,
}

fn address(value: &str) -> Option<String> {
    let hex: String = value.chars().filter(|c| !matches!(c, ':' | '-')).collect();
    if hex.len() != 12 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let hex = hex.to_ascii_uppercase();
    if hex == "000000000000" || hex == "FFFFFFFFFFFF" {
        return None;
    }
    Some((0..6).map(|i| &hex[2 * i..2 * i + 2]).collect::<Vec<_>>().join(":"))
}

fn insert(out: &mut Vec<PairedDevice>, name: &str, raw_address: &str) {
    let Some(address) = address(raw_address) else { return };
    if let Some(existing) = out.iter_mut().find(|d| d.address == address) {
        if existing.name.is_empty() && !name.is_empty() {
            existing.name = name.to_string();
        }
        return;
    }
    out.push(PairedDevice { name: name.to_string(), address });
}

#[cfg(target_os = "macos")]
mod mac {
    use super::*;
    use std::ffi::{CStr, c_char, c_void};

    type Visitor = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char);
    unsafe extern "C" {
        fn livi_bt_paired_devices(context: *mut c_void, visit: Visitor) -> u32;
        #[cfg(test)]
        fn livi_bt_paired_offline(context: *mut c_void, visit: Visitor);
    }

    unsafe extern "C" fn visit(context: *mut c_void, name: *const c_char, mac: *const c_char) {
        if context.is_null() || name.is_null() || mac.is_null() { return; }
        // SAFETY: the bridge calls synchronously with live, NUL-terminated UTF-8
        // strings; context is our exclusively borrowed Vec for this call only.
        let out = unsafe { &mut *context.cast::<Vec<PairedDevice>>() };
        let name = unsafe { CStr::from_ptr(name) }.to_string_lossy();
        let mac = unsafe { CStr::from_ptr(mac) }.to_string_lossy();
        insert(out, &name, &mac);
    }

    pub fn list() -> Result<Vec<PairedDevice>, InventoryError> {
        let mut out = Vec::<PairedDevice>::new();
        // SAFETY: context stays live until synchronous enumeration returns.
        match unsafe { livi_bt_paired_devices((&mut out as *mut Vec<PairedDevice>).cast(), visit) } {
            0 => {
                out.sort_by(|a, b| a.address.cmp(&b.address));
                Ok(out)
            }
            1 => Err(InventoryError::WrongThread),
            _ => Err(InventoryError::Unavailable),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn native_inventory_fixture_copies_names_filters_unpaired_and_deduplicates() {
            let mut out = Vec::<PairedDevice>::new();
            unsafe { livi_bt_paired_offline((&mut out as *mut Vec<PairedDevice>).cast(), visit) };
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].name, "Offline phone");
            assert_eq!(out[0].address, "02:00:00:00:00:02");
        }

        #[test]
        fn worker_rejected_before_enumeration() {
            assert!(std::thread::spawn(|| matches!(list(), Err(InventoryError::WrongThread)))
                .join().unwrap());
        }
    }
}

#[cfg(target_os = "macos")]
pub use mac::list;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_formats_normalize_and_bad_values_are_rejected() {
        for value in ["02-AB-CD-00-00-02", "02:ab:cd:00:00:02", "02abcd000002"] {
            assert_eq!(address(value).as_deref(), Some("02:AB:CD:00:00:02"));
        }
        for value in ["", "bad", "00:00:00:00:00:00", "FF:FF:FF:FF:FF:FF", "02:ZZ:00:00:00:02", "éééééé"] {
            assert!(address(value).is_none());
        }
    }

    #[test]
    fn duplicates_preserve_real_names_and_empty_inventory_is_valid() {
        let mut out = Vec::new();
        insert(&mut out, "", "02-00-00-00-00-02");
        insert(&mut out, "Offline phone", "02:00:00:00:00:02");
        insert(&mut out, "Other duplicate", "020000000002");
        insert(&mut out, "Bad address", "");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "Offline phone");
        assert!(Vec::<PairedDevice>::new().is_empty());
    }
}
