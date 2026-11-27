//! G4: every (interface, slot) -> Rust function the lead wires into `QueryApiImpl`. The objects
//! are built from these definitions, so the table and what the title calls cannot drift apart.

use crate::areas::{queue, xgame, xnetworking, xpackage, xstore, xsystem, xuser};
use crate::object::InterfaceDef;

pub static ALL: &[&InterfaceDef] = &[
    &queue::QUEUE,
    &xuser::USER_IFACE,
    &xuser::GAMERTAG_IFACE,
    &xgame::TITLE_IFACE,
    &xgame::FEATURES_IFACE,
    &xgame::INVITES_IFACE,
    &xgame::EVENTS_IFACE,
    &xsystem::SYSTEM_IFACE,
    &xsystem::ANALYTICS_IFACE,
    &xpackage::PACKAGE_IFACE,
    &xstore::STORE_IFACE,
    &xnetworking::NETWORKING_IFACE,
];

/// One row per implemented slot: area, provider, interface ids, offset, API name, arguments.
pub fn rows() -> Vec<[String; 6]> {
    let mut out = Vec::new();
    for d in ALL {
        let iids: Vec<String> = d.iids.iter().map(|g| g.to_string()).collect();
        for s in d.slots {
            out.push([
                d.area.to_string(),
                d.provider.to_string(),
                iids.join(" "),
                format!("{:#x}", s.offset),
                s.name.to_string(),
                s.args.to_string(),
            ]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hr::*;
    use crate::object::Object;
    use crate::runtime::QueryApiImpl;
    use std::ffi::c_void;

    /// Every listed slot resolves: `QueryApiImpl` hands out an object for each (provider, iid), and
    /// the object's table holds the listed function at the listed offset.
    #[test]
    fn every_listed_slot_resolves() {
        let mut slots = 0;
        for d in ALL {
            for iid in d.iids {
                let mut out: *mut c_void = std::ptr::null_mut();
                assert_eq!(unsafe { QueryApiImpl(&d.provider, iid, &mut out) }, S_OK, "{} {iid}", d.area);
                let object = unsafe { &*(out as *const Object) };
                assert!(std::ptr::eq(object.def, *d));
                // The title reads the table through the object's first word.
                let vtable = unsafe { *(out as *const *const *const ()) };
                for s in d.slots {
                    let seen = unsafe { *vtable.add(s.offset / 8) };
                    assert_eq!(seen, s.func.0, "{} {:#x} {}", d.area, s.offset, s.name);
                    slots += 1;
                }
            }
        }
        eprintln!("table: {} interfaces, {slots} slot checks, {} rows", ALL.len(), rows().len());
        assert_eq!(rows().len(), ALL.iter().map(|d| d.slots.len()).sum::<usize>());
    }

    /// No two definitions answer the same (provider, iid), and no slot is listed twice.
    #[test]
    fn no_duplicates() {
        let mut pairs = Vec::new();
        for d in ALL {
            for iid in d.iids {
                assert!(!pairs.contains(&(d.provider, *iid)), "{} {iid}", d.area);
                pairs.push((d.provider, *iid));
            }
            let mut offsets: Vec<usize> = d.slots.iter().map(|s| s.offset).collect();
            offsets.sort();
            offsets.dedup();
            assert_eq!(offsets.len(), d.slots.len(), "{}", d.area);
        }
    }
}
