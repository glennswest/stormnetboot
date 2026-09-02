//! The machine's own name for itself.
//!
//! Dell's service tag is SMBIOS Type 1 (System Information) "Serial Number",
//! published by the firmware in the EFI configuration table. Reading it costs
//! no network, no DHCP, no BMC and no configuration, which is exactly why it
//! is a better boot identity than a MAC address: a NIC can be swapped or added
//! to, and then the machine is a different machine as far as the boot server is
//! concerned. The service tag is the chassis.

use alloc::string::{String, ToString};
use core::ptr;

use uefi::{Guid, guid};

// The uefi crate moves these between releases, so pin them here: they are
// architectural constants, not library API.
const SMBIOS_GUID: Guid = guid!("eb9d2d31-2d88-11d3-9a16-0090273fc14d");
const SMBIOS3_GUID: Guid = guid!("f2fd1544-9794-4a2c-992e-e5bbcf20e394");

/// Read the system serial number (the service tag on Dell hardware).
pub fn service_tag() -> Option<String> {
    let st = uefi::table::system_table_raw()?;
    let entries = unsafe {
        let st = st.as_ref();
        core::slice::from_raw_parts(
            st.configuration_table,
            st.number_of_configuration_table_entries,
        )
    };

    // Prefer the 64-bit entry point; fall back to the legacy one. Machines
    // that publish both agree, but only SMBIOS 3 is guaranteed on UEFI.
    let mut table: *const u8 = ptr::null();
    for e in entries {
        if e.vendor_guid == SMBIOS3_GUID {
            table = unsafe { smbios3_table(e.vendor_table as *const u8) };
            if !table.is_null() {
                break;
            }
        }
        if e.vendor_guid == SMBIOS_GUID && table.is_null() {
            table = unsafe { smbios_table(e.vendor_table as *const u8) };
        }
    }
    if table.is_null() {
        return None;
    }
    unsafe { find_type1_serial(table) }
}

/// `_SM3_` entry point: the structure table address is a 64-bit field at 0x10.
unsafe fn smbios3_table(entry: *const u8) -> *const u8 {
    if entry.is_null() || core::slice::from_raw_parts(entry, 5) != b"_SM3_" {
        return ptr::null();
    }
    (entry.add(0x10) as *const u64).read_unaligned() as *const u8
}

/// `_SM_` entry point: a 32-bit table address at 0x18.
unsafe fn smbios_table(entry: *const u8) -> *const u8 {
    if entry.is_null() || core::slice::from_raw_parts(entry, 4) != b"_SM_" {
        return ptr::null();
    }
    (entry.add(0x18) as *const u32).read_unaligned() as *const u8
}

/// Walk the structure table to Type 1 and return its Serial Number string.
///
/// Every structure is a fixed-length formatted section followed by a string
/// table of NUL-terminated strings terminated by a double NUL. A string
/// *field* inside the formatted section holds a **1-based index** into that
/// table, not an offset — read it as an offset and you get the manufacturer,
/// or nothing, depending on the machine.
unsafe fn find_type1_serial(mut p: *const u8) -> Option<String> {
    // Bounded so a malformed table cannot spin forever in firmware.
    for _ in 0..2048 {
        let stype = *p;
        let len = *p.add(1) as usize;
        if len < 4 {
            return None;
        }
        if stype == 127 {
            return None; // end-of-table marker
        }

        let strings = p.add(len);
        if stype == 1 {
            // Type 1, offset 0x07: Serial Number.
            return smbios_string(strings, *p.add(7));
        }

        // Skip this structure's string table.
        let mut q = strings;
        loop {
            if *q == 0 && *q.add(1) == 0 {
                q = q.add(2);
                break;
            }
            q = q.add(1);
        }
        p = q;
    }
    None
}

unsafe fn smbios_string(mut p: *const u8, index: u8) -> Option<String> {
    if index == 0 {
        return None; // 0 means "no string supplied"
    }
    for _ in 1..index {
        while *p != 0 {
            p = p.add(1);
        }
        p = p.add(1);
    }
    let mut out = String::new();
    for _ in 0..128 {
        if *p == 0 {
            break;
        }
        out.push(*p as char);
        p = p.add(1);
    }
    let out = out.trim().to_string();
    (!out.is_empty()).then_some(out)
}
