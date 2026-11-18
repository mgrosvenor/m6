//! The identity a crate was built from: its version, commit and dirty flag.
//!
//! Every m6 crate linked into a binary carries the constants its own build
//! script emitted. [`identity`] answers for m6-platform, [`linked`] answers for
//! this crate and its m6 dependencies, and m6-core re-exports both. A binary
//! prints its own identity first and then the list from [`linked`], so a mixed
//! build shows the two commits side by side.
//!
//! The build-script half lives in [`emit`], included from `build.rs` and from
//! this module.

include!("build_info/emit.rs");

/// One crate's build-time identity.
///
/// The fields are `&'static str` because they are `env!` constants and because
/// reading an identity costs no allocation beyond the formatted line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuildIdentity {
    /// The package name, as Cargo writes `CARGO_PKG_NAME`.
    pub name: &'static str,
    /// The package version, as Cargo writes `CARGO_PKG_VERSION`.
    pub version: &'static str,
    /// The commit the crate was built from, twelve characters, or `unknown`.
    pub commit: &'static str,
    /// Whether `git status --porcelain` was non-empty at build time.
    pub dirty: bool,
}

impl BuildIdentity {
    /// Assemble one identity.
    pub const fn new(
        name: &'static str,
        version: &'static str,
        commit: &'static str,
        dirty: bool,
    ) -> Self {
        Self {
            name,
            version,
            commit,
            dirty,
        }
    }

    /// The tag form: `v1.16.0` or `v1.16.0-rc.1`.
    ///
    /// This is `--version`. It carries no crate name, no build metadata and no
    /// dirty marker, so it matches the git tag exactly. The metadata belongs to
    /// [`full`](Self::full).
    pub fn tag(&self) -> String {
        format!("v{}", self.version)
    }

    /// The full SemVer 2.0.0 string with build metadata.
    ///
    /// `1.16.0-rc.1+abc123def456` on a clean build, and
    /// `1.16.0-rc.1+abc123def456.dirty` on a tree with uncommitted changes. A
    /// source tarball has no git and reads `1.16.0-rc.1+unknown`.
    pub fn full(&self) -> String {
        let mut full = String::with_capacity(self.version.len() + self.commit.len() + 8);
        full.push_str(self.version);
        full.push('+');
        full.push_str(self.commit);
        if self.dirty {
            full.push_str(".dirty");
        }
        full
    }

    /// One `--version-full` line: `<crate> - <full SemVer>`.
    pub fn line(&self) -> String {
        format!("{} - {}", self.name, self.full())
    }
}

/// The constants this crate's build script emitted.
///
/// The macro is exported so a crate that links m6-platform can name its own
/// constants from its own compilation. `env!` rather than `option_env!` is the
/// point: a crate with no build script fails to compile instead of reporting
/// another crate's identity.
#[macro_export]
macro_rules! build_identity {
    () => {
        $crate::build_info::BuildIdentity::new(
            env!("M6_BUILD_NAME"),
            env!("M6_BUILD_VERSION"),
            env!("M6_BUILD_COMMIT"),
            env!("M6_BUILD_DIRTY") == "1",
        )
    };
}

/// This crate's identity, from the constants its build script emitted.
pub fn identity() -> BuildIdentity {
    build_identity!()
}

/// This crate's identity followed by its m6 dependencies' identities.
///
/// m6-platform is the bottom of the m6 stack and links no m6 crate, so its list
/// is itself alone. m6-log and m6-core prepend their own identity and pass their
/// dependencies to [`linked_from`].
pub fn linked() -> Vec<BuildIdentity> {
    vec![identity()]
}

/// An identity followed by each dependency list, keeping the first of each crate.
///
/// Dependencies are listed depth first in the order the caller names them. A
/// crate reached by two paths appears once, at its first position. The order is
/// stable, so `--version-full` output is deterministic.
pub fn linked_from(own: BuildIdentity, deps: &[Vec<BuildIdentity>]) -> Vec<BuildIdentity> {
    let mut out = vec![own];
    for list in deps {
        for id in list {
            if !out.iter().any(|seen| seen.name == id.name) {
                out.push(*id);
            }
        }
    }
    out
}

/// The lines `--version-full` prints, as strings a test can compare exactly.
///
/// The binary's own identity is first. A crate reached by two paths appears
/// once, so the list is the set of m6 crates in the binary in dependency order.
pub fn full_lines(own: BuildIdentity, linked: &[BuildIdentity]) -> Vec<String> {
    let mut seen = Vec::new();
    let mut lines = Vec::new();
    for id in std::iter::once(&own).chain(linked.iter()) {
        if seen.contains(&id.name) {
            continue;
        }
        seen.push(id.name);
        lines.push(id.line());
    }
    lines
}

#[cfg(test)]
mod tests {
    use semver::Version;

    use super::*;

    fn id(version: &'static str, commit: &'static str, dirty: bool) -> BuildIdentity {
        BuildIdentity::new("m6-http", version, commit, dirty)
    }

    #[test]
    fn a_release_tag_is_the_bare_tag_and_the_full_string_is_semver() {
        let release = id("1.16.0", "abc123def456", false);
        assert_eq!(release.tag(), "v1.16.0");
        assert_eq!(release.full(), "1.16.0+abc123def456");

        let parsed = Version::parse(&release.full()).expect("the full form is SemVer 2.0.0");
        assert!(parsed.pre.is_empty());
        assert_eq!(parsed.build.as_str(), "abc123def456");
    }

    #[test]
    fn a_release_candidate_keeps_its_prerelease_before_the_metadata() {
        let rc = id("1.16.0-rc.1", "abc123def456", false);
        assert_eq!(rc.tag(), "v1.16.0-rc.1");
        assert_eq!(rc.full(), "1.16.0-rc.1+abc123def456");

        let parsed = Version::parse(&rc.full()).expect("the full form is SemVer 2.0.0");
        assert_eq!(parsed.pre.as_str(), "rc.1");
        assert_eq!(parsed.build.as_str(), "abc123def456");
    }

    #[test]
    fn a_dirty_build_marks_the_metadata_and_leaves_the_tag_alone() {
        let dirty = id("1.16.0-rc.1", "abc123def456", true);
        assert_eq!(dirty.tag(), "v1.16.0-rc.1");
        assert_eq!(dirty.full(), "1.16.0-rc.1+abc123def456.dirty");

        let parsed = Version::parse(&dirty.full()).expect("the dirty form is SemVer 2.0.0");
        assert_eq!(parsed.pre.as_str(), "rc.1");
        assert_eq!(parsed.build.as_str(), "abc123def456.dirty");
    }

    #[test]
    fn a_build_without_git_says_unknown() {
        let tarball = id("1.16.0-rc.1", "unknown", false);
        assert_eq!(tarball.full(), "1.16.0-rc.1+unknown");
        Version::parse(&tarball.full()).expect("unknown is a legal metadata identifier");
    }

    #[test]
    fn version_full_names_each_crate_once_in_order() {
        let own = id("1.16.0-rc.1", "abc123def456", false);
        let core = BuildIdentity::new("m6-core", "1.16.0-rc.1", "abc123def456", false);
        let log = BuildIdentity::new("m6-log", "1.16.0-rc.1", "abc123def456", false);
        let lines = full_lines(own, &[own, core, log, core]);
        assert_eq!(
            lines,
            vec![
                "m6-http - 1.16.0-rc.1+abc123def456",
                "m6-core - 1.16.0-rc.1+abc123def456",
                "m6-log - 1.16.0-rc.1+abc123def456",
            ]
        );
    }
}
