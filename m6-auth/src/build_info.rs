//! The identity m6-auth was built from.
//!
//! m6-auth links m6-core, so its list is m6-auth, m6-core, m6-log and
//! m6-platform. The two auth binaries print this list after their own identity.

pub use m6_core::build_info::{linked_from, BuildIdentity};

/// This crate's identity, from the constants its build script emitted.
pub fn identity() -> BuildIdentity {
    m6_core::build_info::build_identity!()
}

/// This crate's identity followed by the m6 crates it links.
pub fn linked() -> Vec<BuildIdentity> {
    linked_from(identity(), &[m6_core::build_info::linked()])
}
