//! `dagpane-auth` — the front door.
//!
//! One job: decide whether a bearer token entitles its holder to open a given app, and say
//! precisely why when it does not.
//!
//! # What this crate deliberately does not do
//!
//! **It never talks to the identity provider.** No discovery request, no JWKS fetch, no token
//! exchange — so there is no HTTP client here and the runtime's "does not phone home"
//! invariant survives having a front door. The operator supplies the JWKS as a file; key
//! rotation is a copy and a reload. An air-gapped install works, and a provider being down
//! cannot stop this process from starting.
//!
//! The consequence, stated rather than discovered: **obtaining** a token is somebody else's
//! job. The bundled client does the PKCE dance in the browser, which is what PKCE is for —
//! a public client with no secret to keep. An operator who already has an OAuth2 proxy in
//! front can skip that entirely and hand the token over; this crate cannot tell the
//! difference and does not want to.
//!
//! # Every check is written down, because the defaults are where JWTs go wrong
//!
//! The dangerous part of a JWT is not the cryptography, which is [`jsonwebtoken`]'s and
//! `ring`'s. It is the validation, and specifically what is *not* checked when nobody said
//! to. So [`Policy`] has no `Default`, every field is required at the call site, and the
//! four failures that have historically cost people their systems are closed by construction:
//!
//! * **`alg: none`** — impossible. The allowlist is [`Algorithm`]s of this crate's own
//!   making, and none of them is `none`; `jsonwebtoken` has no variant for it either.
//! * **`HS256` signed with the RSA public key** — the classic algorithm-confusion attack,
//!   where a verifier that trusts the token's own `alg` header treats a public key as an
//!   HMAC secret. Impossible here for two independent reasons: no symmetric algorithm can be
//!   allowlisted ([`Algorithm`] has none), and the keys built from a JWKS are asymmetric
//!   verification keys that `jsonwebtoken` will not use for HMAC.
//! * **An unchecked audience.** [`Policy::audience`] is not optional. A token minted for a
//!   different service by the same issuer is refused.
//! * **An unchecked issuer.** Same.
//!
//! And one that is this product's own: **which apps.** See [`Access`].

#![forbid(unsafe_code)]
#![deny(missing_debug_implementations)]
#![deny(missing_docs)]

mod error;
mod jwks;
mod policy;

pub use error::AuthError;
pub use jwks::{Jwks, JwksError};
pub use policy::{bearer, Access, Algorithm, Policy, Viewer, AUTH_SUBPROTOCOL_PREFIX};
