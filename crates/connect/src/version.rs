//! The cheap staleness token, and the one thing it is allowed to be wrong about.

use std::fmt;

use dagpane_core::digest::Hasher;
use dagpane_core::Digest;

/// A source's own claim about whether it has changed.
///
/// # What an equal version means
///
/// *"Nothing I can cheaply see has changed."* That is a claim about a file's modification
/// time and length, an HTTP `ETag`, or a query's row count — **not** about bytes. Two
/// sources holding identical data routinely have different versions, and that is fine: an
/// unequal version costs one load, and the engine's digest of what was loaded is what
/// decides whether any cell recomputes. A version that changes too eagerly is a wasted read.
///
/// # The direction it can be wrong in, stated plainly
///
/// The costly failure is the opposite one — an *equal* version over data that did change,
/// because then no load happens and a viewer reads a stale number that looks fresh. Every
/// implementation has to say when that can happen:
///
/// * [`crate::FileSource`]: a rewrite that lands within the filesystem's timestamp
///   granularity **and** leaves the length identical. Rare, and not impossible — a program
///   that writes a fixed-width record in place hits both.
/// * `HttpSource` (feature `http`): a server that sends a wrong or absent validator. With no `ETag`
///   and no `Last-Modified` the source reports a version that never repeats, so it reloads
///   every tick rather than risking the stale answer.
/// * `SqlSource` (feature `sql`): a table whose row count is unchanged after an update. Common
///   enough that the version also carries the `max` of any column the source is told to
///   watch, and the documentation says to name one.
///
/// A refresh that must not rely on any of that skips the check — `dagpane refresh --force`,
/// and the scheduled refresh's equivalent — and pays the load.
///
/// # Comparable against what
///
/// **Only against another version from the same source.** There is deliberately no ordering
/// and no way to ask whether one version is newer: a version is an opaque token, and the
/// only question it answers is "is this the same one I saw last time?".
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Version(Digest);

impl Version {
    /// Build a version from whatever the source watches: a tag per part, then the parts.
    ///
    /// The tag is the caller's own discriminator — `b'f'` for a file, `b'h'` for HTTP — so
    /// two implementations cannot produce equal versions from coincidentally equal
    /// metadata. Not that they would ever be compared, but a type whose values are only
    /// meaningful under a convention is a type that eventually gets compared anyway.
    pub fn of(kind: u8, parts: &[VersionPart<'_>]) -> Version {
        let mut h = Hasher::new();
        h.tag(kind);
        h.u64(parts.len() as u64);
        for part in parts {
            match part {
                VersionPart::Num(v) => {
                    h.tag(0);
                    h.u64(*v);
                }
                VersionPart::Text(s) => {
                    h.tag(1);
                    h.str(s);
                }
                VersionPart::Absent => {
                    h.tag(2);
                }
            }
        }
        Version(h.finish())
    }

    /// A version that never equals another. What a source returns when it has no validator
    /// worth trusting: "I cannot tell you, so assume it moved."
    ///
    /// Needs a counter rather than a random number because this crate has no clock and no
    /// entropy source, and it does not need one: the only requirement is that two calls
    /// differ.
    pub fn unknowable() -> Version {
        use std::sync::atomic::{AtomicU64, Ordering};
        static TICK: AtomicU64 = AtomicU64::new(0);
        Version::of(
            b'?',
            &[VersionPart::Num(TICK.fetch_add(1, Ordering::Relaxed))],
        )
    }

    /// The token's low 64 bits, for a log line. Never for a comparison — compare the
    /// [`Version`] itself.
    pub fn short(self) -> u64 {
        self.0.short()
    }
}

/// One thing a version is built from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VersionPart<'a> {
    /// A number: a length, a modification time, a row count.
    Num(u64),
    /// A string: an `ETag`, a `Last-Modified`, a watched column's maximum.
    Text(&'a str),
    /// The source looked for this and the answer was "there isn't one".
    ///
    /// Distinct from an empty string on purpose: a server sending `ETag: ""` and a server
    /// sending no `ETag` at all are different situations and must not produce one version.
    Absent,
}

impl fmt::Debug for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Version({:016x})", self.0.short())
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0.short())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_parts_make_the_same_version_and_a_different_kind_does_not() {
        let parts = [VersionPart::Num(17), VersionPart::Text("abc")];
        assert_eq!(Version::of(b'f', &parts), Version::of(b'f', &parts));
        assert_ne!(Version::of(b'f', &parts), Version::of(b'h', &parts));
    }

    #[test]
    fn an_absent_validator_is_not_an_empty_one() {
        assert_ne!(
            Version::of(b'h', &[VersionPart::Absent]),
            Version::of(b'h', &[VersionPart::Text("")]),
        );
    }

    #[test]
    fn a_length_and_a_time_cannot_swap_places_unnoticed() {
        assert_ne!(
            Version::of(b'f', &[VersionPart::Num(1), VersionPart::Num(2)]),
            Version::of(b'f', &[VersionPart::Num(2), VersionPart::Num(1)]),
        );
    }

    #[test]
    fn an_unknowable_version_never_repeats() {
        let a = Version::unknowable();
        let b = Version::unknowable();
        assert_ne!(
            a, b,
            "a source that cannot tell must not claim it is unchanged"
        );
    }
}
