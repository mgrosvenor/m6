//! The identity m6-log was built from.
//!
//! m6-log sits above m6-platform and below m6-core. It carries its own
//! constants so a binary that links both reports each crate's own build, and it
//! passes m6-platform's identity up to m6-core.

pub use m6_platform::build_info::{linked_from, BuildIdentity};

/// This crate's identity, from the constants its build script emitted.
pub fn identity() -> BuildIdentity {
    m6_platform::build_identity!()
}

/// This crate's identity followed by the m6 crates it links.
pub fn linked() -> Vec<BuildIdentity> {
    linked_from(identity(), &[m6_platform::build_info::linked()])
}
