//! A 128-bit content digest, written in-tree.
//!
//! The engine decides whether a cell's inputs changed by comparing digests, never by
//! comparing values. That choice is what makes the comparison O(1) for a million-row table
//! — the digest is computed **once**, when the value is produced, over data the cell just
//! built anyway, and every later comparison is two `u64`s.
//!
//! Why in-tree rather than a hashing crate:
//!
//!   * This is the one place where a bug is silent. A digest that collides shows a user a
//!     stale number and the app looks like it is working. Twenty lines a reviewer can read
//!     in full are worth more here than a faster function they will not.
//!   * `std::hash::Hasher` is 64-bit and its `DefaultHasher` is explicitly not stable
//!     across releases. A digest that changes meaning when the toolchain moves cannot be
//!     compared against one recorded a moment earlier by the same process, which is all we
//!     need, but it also cannot be written down — and `dagpane explain` writes them down.
//!
//! **What it costs, which the rest of the documentation should not round off.** This hashes
//! one byte at a time with a 128-bit multiply per byte. That is roughly an order of magnitude
//! slower than a word-at-a-time hash, and it is paid once per value produced — so a cell that
//! builds a large table pays it over every byte of that table. For the app sizes this runtime
//! targets it is comfortably below the cost of building the table in the first place; for the
//! million-row tables the value model is *not* sized for, hashing would dominate a pass. The
//! trade is deliberate and stated in ADR-0003: forty lines a reviewer can read in full, in the
//! one place where a bug is silent. If hash throughput ever needs to be a number, it belongs
//! beside apps-per-core in the roadmap's measurement item, not in a claim made without one.
//!
//! **Collisions are possible and the consequence is a missed recompute.** FNV-1a over 128
//! bits puts a chance collision at roughly 2^-128 per comparison, which is not a risk this
//! project manages; an *adversarially* chosen collision is achievable against FNV by anyone
//! who can choose a cell's exact output bytes, and the honest statement is that dagpane's
//! threat model does not include an attacker who controls a cell's output and wants the UI
//! to show a stale figure. If that ever becomes a real threat, this module is the one file
//! that changes.

use std::fmt;

/// The FNV-1a 128-bit offset basis and prime, from the reference specification. Grouped in
/// fours rather than in the specification's own three-part form so that a reader can compare
/// them digit by digit against it; the values are unchanged.
const OFFSET_BASIS: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;

/// A content digest. Equality of two digests is the engine's proxy for equality of two
/// values; see the module docs for what that costs.
#[derive(
    Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct Digest(u128);

impl Digest {
    /// The digest of an empty byte stream. Also the digest a slot carries before it has ever
    /// been computed, which is why a session slot tracks validity separately rather than
    /// treating this as "no value". (`Slot` is private, so it is named here rather than
    /// linked: a broken link in the public documentation is worse than a plain word.)
    pub const EMPTY: Digest = Digest(OFFSET_BASIS);

    /// The low 64 bits, for a short human-readable form.
    pub fn short(self) -> u64 {
        self.0 as u64
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.short())
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.short())
    }
}

/// An FNV-1a hasher over 128 bits.
///
/// Not a `std::hash::Hasher`: that trait is 64-bit and its `write_*` methods are free to
/// encode integers however the implementation likes. Everything hashed here goes through
/// [`Hasher::bytes`] with an explicit big-endian encoding, so the digest of a value is a
/// property of the value and not of the platform.
#[derive(Clone, Copy, Debug)]
pub struct Hasher(u128);

impl Default for Hasher {
    fn default() -> Self {
        Hasher(OFFSET_BASIS)
    }
}

impl Hasher {
    /// A hasher primed with the FNV offset basis. Identical to [`Hasher::default`]; both
    /// exist because the builder chains below read as `Hasher::new().str(..).finish()`.
    pub fn new() -> Self {
        Self::default()
    }

    /// The one primitive: absorb bytes, one at a time, in the order given. Everything else
    /// on this type is a named encoding that ends up here, which is what keeps the digest a
    /// property of the value rather than of the platform.
    pub fn bytes(&mut self, data: &[u8]) -> &mut Self {
        for &b in data {
            self.0 ^= b as u128;
            self.0 = self.0.wrapping_mul(PRIME);
        }
        self
    }

    /// A one-byte type tag. Every composite value writes one before its parts, so that
    /// `Value::Text("1")` and `Value::Int(1)` cannot digest alike, and so that a list of
    /// two items cannot digest as the concatenation of its parts.
    pub fn tag(&mut self, tag: u8) -> &mut Self {
        self.bytes(&[tag])
    }

    /// An unsigned integer, big-endian. Also the encoding used for every length prefix, so
    /// a count and a value of the same magnitude are written the same way.
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.bytes(&v.to_be_bytes())
    }

    /// A signed integer, big-endian two's complement. Note that `-1i64` and `u64::MAX`
    /// absorb identically — the caller's [`Hasher::tag`] is what keeps them apart, which is
    /// why every composite writes one.
    pub fn i64(&mut self, v: i64) -> &mut Self {
        self.bytes(&v.to_be_bytes())
    }

    /// Floats are canonicalised before hashing, because the engine's contract is "an equal
    /// value does not propagate" and IEEE equality does not agree with bitwise equality at
    /// two points:
    ///
    ///   * every NaN hashes as one canonical NaN, so a cell that recomputes NaN from a
    ///     different NaN does not wake its dependents;
    ///   * `-0.0` hashes as `0.0`, because `-0.0 == 0.0` and a user who sees `0` twice has
    ///     not been shown a change.
    pub fn f64(&mut self, v: f64) -> &mut Self {
        let canonical = if v.is_nan() {
            f64::NAN
        } else if v == 0.0 {
            0.0
        } else {
            v
        };
        self.bytes(&canonical.to_bits().to_be_bytes())
    }

    /// Length-prefixed, so `["ab", "c"]` and `["a", "bc"]` differ.
    pub fn str(&mut self, s: &str) -> &mut Self {
        self.u64(s.len() as u64);
        self.bytes(s.as_bytes())
    }

    /// Absorbs a digest already taken. This is how a composite is hashed from its parts'
    /// digests instead of from their bytes, so a cell can be compared against the values it
    /// was built from without walking them again.
    pub fn digest(&mut self, d: Digest) -> &mut Self {
        self.bytes(&d.0.to_be_bytes())
    }

    /// The digest of everything absorbed so far. Takes `&self`, so a hasher can be finished
    /// and then fed more — the intermediate result is a genuine digest of a prefix, not a
    /// half-finished state.
    pub fn finish(&self) -> Digest {
        Digest(self.0)
    }
}

/// Anything the engine can compare by content.
pub trait Digestible {
    /// Absorb this value into `h`. Implementations must write a distinguishing
    /// [`Hasher::tag`] before their parts and a length before anything variable-sized;
    /// without both, two different values can absorb the same bytes and the engine will
    /// skip a recompute it owed the user.
    fn digest_into(&self, h: &mut Hasher);

    /// This value's digest on its own. The engine calls it once per value produced and then
    /// only ever compares the results.
    fn digest(&self) -> Digest {
        let mut h = Hasher::new();
        self.digest_into(&mut h);
        h.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_inputs_digest_distinctly() {
        let a = Hasher::new().bytes(b"hello").finish();
        let b = Hasher::new().bytes(b"world").finish();
        assert_ne!(a, b);
    }

    #[test]
    fn digest_is_deterministic() {
        let a = Hasher::new().str("threshold").u64(7).finish();
        let b = Hasher::new().str("threshold").u64(7).finish();
        assert_eq!(a, b);
    }

    #[test]
    fn length_prefixing_separates_concatenations() {
        let a = Hasher::new().str("ab").str("c").finish();
        let b = Hasher::new().str("a").str("bc").finish();
        assert_ne!(a, b, "without a length prefix these two would collide");
    }

    #[test]
    fn tags_separate_types() {
        let a = Hasher::new().tag(1).u64(1).finish();
        let b = Hasher::new().tag(2).u64(1).finish();
        assert_ne!(a, b);
    }

    #[test]
    fn nan_is_canonical_so_it_does_not_wake_dependents() {
        let one = f64::from_bits(0x7ff8_0000_0000_0001);
        let other = f64::from_bits(0x7ff8_0000_0000_0002);
        assert!(one.is_nan() && other.is_nan());
        assert_eq!(
            Hasher::new().f64(one).finish(),
            Hasher::new().f64(other).finish()
        );
    }

    #[test]
    fn negative_zero_equals_zero() {
        assert_eq!(
            Hasher::new().f64(-0.0).finish(),
            Hasher::new().f64(0.0).finish()
        );
    }
}
