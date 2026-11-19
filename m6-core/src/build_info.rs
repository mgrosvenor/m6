//! The one call an app makes for `--version` and `--version-full`.
//!
//! An app supplies one build script line:
//!
//! ```text
//! fn main() { m6_core::build_info::emit(); }
//! ```
//!
//! and one call at the top of `main`, before any argument parsing:
//!
//! ```text
//! m6_core::version_flags!(m6_core::build_info::linked());
//! ```
//!
//! An app that links more than m6-core passes the top m6 crate's list, for
//! example `m6_auth::build_info::linked()`, and that crate's own dependencies
//! follow. The list is derived from the dependency graph at compile time, and
//! the test `m6-core/tests/version_flag.rs` holds it against the workspace's
//! dependency graph.
//!
//! [`BuildIdentity`] and the formatting live in m6-platform, which sits below
//! m6-core. m6-core re-exports them so an app names one crate.

pub use m6_platform::build_identity;
pub use m6_platform::build_info::{emit, full_lines, linked_from, BuildIdentity};

/// This crate's identity, from the constants its build script emitted.
pub fn identity() -> BuildIdentity {
    build_identity!()
}

/// This crate's identity followed by its m6 dependencies' identities.
///
/// The order is m6-core, m6-log, m6-platform: this crate first, then each
/// dependency depth first. [`linked_from`] drops a crate reached by two paths.
pub fn linked() -> Vec<BuildIdentity> {
    linked_from(
        identity(),
        &[
            m6_log::build_info::linked(),
            m6_platform::build_info::linked(),
        ],
    )
}

/// The identity a binary reports first, and the m6 crates listed after it.
///
/// The first is the package's identity under the binary's name. A binary whose
/// name is not its package's, such as `m6-bench` in the m6-http package, lists
/// the package next, because the package's library is an m6 crate the binary
/// links.
pub fn for_binary(
    bin: &'static str,
    package: BuildIdentity,
    linked: Vec<BuildIdentity>,
) -> (BuildIdentity, Vec<BuildIdentity>) {
    let own = package.named(bin);
    if bin == package.name {
        return (own, linked);
    }
    let mut rest = Vec::with_capacity(linked.len() + 1);
    rest.push(package);
    rest.extend(linked);
    (own, rest)
}

/// Answer `--version` or `--version-full` and exit, or return to the caller.
///
/// `--version` prints the tag form of `own`, `v1.16.0-rc.1`, and nothing else.
/// `--version-full` prints `own` first, then `linked`, one `<crate> - <full
/// SemVer>` line each. Both flags work with no site directory and no
/// configuration, because a deploy asks a freshly installed binary what it is
/// before either exists.
///
/// A build without git says `+unknown`. A dirty tree says `+.dirty`. Neither
/// changes the tag form, so `--version` still names the tag a deploy can match.
pub fn handle_version_flags(own: BuildIdentity, linked: &[BuildIdentity]) {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|arg| arg == "--version-full") {
        for line in full_lines(own, linked) {
            println!("{line}");
        }
        std::process::exit(0);
    }

    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("{}", own.tag());
        std::process::exit(0);
    }
}

/// Register the binary that calls it and answer the two version flags.
///
/// The macro reads the caller's own `env!` constants and takes the linked list
/// it is given. The first line names the binary, from `CARGO_BIN_NAME`, because
/// one package can build several: m6-http's package builds `m6-bench` and five
/// more. m6-core is the only crate that implements the answer, so no app
/// formats a version string.
///
/// The same identity is registered for the start-up record's identity line
/// ([`crate::startup::set_app_identity`]), because m6-core cannot read another
/// crate's build constants and a record without it would name m6-core.
#[macro_export]
macro_rules! version_flags {
    ($linked:expr $(,)?) => {{
        let (own, linked) = $crate::build_info::for_binary(
            env!("CARGO_BIN_NAME"),
            $crate::build_info::build_identity!(),
            $linked,
        );
        $crate::startup::set_app_identity(own, linked.clone());
        $crate::build_info::handle_version_flags(own, &linked)
    }};
}
