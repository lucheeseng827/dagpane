//! The front door, against the tokens it exists to refuse.
//!
//! Every keypair here is generated **at run time**. This repository does not commit keys —
//! `.gitignore` refuses `*.pem`, `*.key` and `*.pkcs8` — and a test that needs one makes it,
//! uses it, and drops it. `ring` is already in the tree under `jsonwebtoken`, so the
//! dev-dependency costs no compilation.

use std::collections::BTreeSet;

use base64::Engine;
use dagpane_auth::{Access, Algorithm, AuthError, Jwks, JwksError, Policy};
use jsonwebtoken::{encode, EncodingKey, Header};
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// A throwaway P-256 keypair, as a signing key and as the JWKS a server would be given.
struct Idp {
    pkcs8: Vec<u8>,
    kid: String,
    x: String,
    y: String,
}

impl Idp {
    fn new(kid: &str) -> Idp {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng)
            .unwrap();
        // Uncompressed SEC1: 0x04 || X(32) || Y(32).
        let point = pair.public_key().as_ref().to_vec();
        Idp {
            pkcs8: pkcs8.as_ref().to_vec(),
            kid: kid.to_string(),
            x: B64.encode(&point[1..33]),
            y: B64.encode(&point[33..65]),
        }
    }

    fn jwks(&self) -> String {
        format!(
            r#"{{"keys":[{{"kty":"EC","crv":"P-256","use":"sig","kid":"{}","x":"{}","y":"{}"}}]}}"#,
            self.kid, self.x, self.y
        )
    }

    /// Sign whatever claims a test wants, including invalid ones.
    fn sign(&self, claims: serde_json::Value) -> String {
        let mut header = Header::new(jsonwebtoken::Algorithm::ES256);
        header.kid = Some(self.kid.clone());
        encode(&header, &claims, &EncodingKey::from_ec_der(&self.pkcs8)).unwrap()
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn claims() -> serde_json::Value {
    serde_json::json!({
        "sub": "alice",
        "iss": "https://idp.test",
        "aud": "dagpane",
        "exp": now() + 300,
        "name": "Alice",
        "dagpane_apps": ["sales", "ops"],
    })
}

fn policy(access: Access) -> Policy {
    Policy {
        issuer: "https://idp.test".into(),
        audience: "dagpane".into(),
        algorithms: BTreeSet::from([Algorithm::ES256]),
        leeway_seconds: 5,
        access,
    }
}

fn apps() -> Policy {
    policy(Access::Claim("dagpane_apps".into()))
}

// ── the door opens for the right token ─────────────────────────────────────────────────────

#[test]
fn a_good_token_admits_its_holder_to_the_apps_its_claim_lists() {
    let idp = Idp::new("k1");
    let jwks = Jwks::parse(&idp.jwks()).unwrap();
    let token = idp.sign(claims());

    let viewer = apps().admit(&jwks, &token, "sales").unwrap();
    assert_eq!(viewer.subject, "alice");
    assert_eq!(viewer.name.as_deref(), Some("Alice"));
    assert!(viewer.may_open("ops"));
    assert!(!viewer.may_open("payroll"));

    // A viewer's Debug is the subject and nothing else: a name and an email are personal
    // data and a `{:?}` in a log line is the least deliberate way for them to leave.
    let printed = format!("{viewer:?}");
    assert_eq!(printed, "Viewer(alice)");
    assert!(!printed.contains("Alice"));
}

#[test]
fn a_star_in_the_claim_opens_everything_and_any_app_needs_no_claim_at_all() {
    let idp = Idp::new("k1");
    let jwks = Jwks::parse(&idp.jwks()).unwrap();

    let mut starred = claims();
    starred["dagpane_apps"] = serde_json::json!(["*"]);
    assert!(apps().admit(&jwks, &idp.sign(starred), "anything").is_ok());

    // `AnyApp` is the single-app case: a valid token is the whole check.
    let mut bare = claims();
    bare.as_object_mut().unwrap().remove("dagpane_apps");
    assert!(policy(Access::AnyApp)
        .admit(&jwks, &idp.sign(bare), "whatever")
        .is_ok());
}

// ── and refuses everything else ────────────────────────────────────────────────────────────

#[test]
fn a_token_for_an_app_the_claim_does_not_list_is_forbidden_and_not_unauthorised() {
    let idp = Idp::new("k1");
    let jwks = Jwks::parse(&idp.jwks()).unwrap();

    match apps().admit(&jwks, &idp.sign(claims()), "payroll") {
        Err(AuthError::Forbidden { subject, app }) => {
            assert_eq!(subject, "alice");
            assert_eq!(app, "payroll");
        }
        other => panic!("expected Forbidden, got {other:?}"),
    }
    // 403, not 401: this person authenticated. Telling them to log in again would send them
    // round a loop that cannot succeed.
    assert_eq!(
        apps()
            .admit(&jwks, &idp.sign(claims()), "payroll")
            .unwrap_err()
            .status(),
        403
    );
}

#[test]
fn a_missing_access_claim_refuses_rather_than_granting_nothing_quietly() {
    // The misconfigured-provider case. An absent claim is far more likely a provider that was
    // never told to mint it than a deliberate grant of nothing, and refusing is the direction
    // that fails safely.
    let idp = Idp::new("k1");
    let jwks = Jwks::parse(&idp.jwks()).unwrap();

    let mut bare = claims();
    bare.as_object_mut().unwrap().remove("dagpane_apps");
    assert!(matches!(
        apps().admit(&jwks, &idp.sign(bare), "sales"),
        Err(AuthError::Forbidden { .. })
    ));

    // And a claim of the wrong shape is not quietly read as a grant either.
    for shape in [
        serde_json::json!("sales"),
        serde_json::json!({"sales": true}),
        serde_json::json!(7),
    ] {
        let mut wrong = claims();
        wrong["dagpane_apps"] = shape.clone();
        assert!(
            matches!(
                apps().admit(&jwks, &idp.sign(wrong), "sales"),
                Err(AuthError::Forbidden { .. })
            ),
            "a claim of {shape} was accepted"
        );
    }
}

#[test]
fn a_token_signed_by_somebody_else_is_refused() {
    let ours = Idp::new("k1");
    // A different key under THE SAME kid: the attacker is not guessing which key we hold,
    // they are claiming to be it.
    let theirs = Idp::new("k1");
    let jwks = Jwks::parse(&ours.jwks()).unwrap();

    match apps().admit(&jwks, &theirs.sign(claims()), "sales") {
        Err(AuthError::Rejected(_)) => {}
        other => panic!("a foreign signature was accepted: {other:?}"),
    }
}

#[test]
fn an_expired_token_is_refused_and_the_leeway_is_the_policy_s_and_not_a_library_default() {
    let idp = Idp::new("k1");
    let jwks = Jwks::parse(&idp.jwks()).unwrap();

    let mut stale = claims();
    stale["exp"] = serde_json::json!(now() - 30);
    assert!(matches!(
        apps().admit(&jwks, &idp.sign(stale.clone()), "sales"),
        Err(AuthError::Rejected(_))
    ));

    // `jsonwebtoken`'s own default leeway is 60 seconds, which would have accepted that
    // token. The policy says 5, and the policy is what runs.
    let mut generous = apps();
    generous.leeway_seconds = 120;
    assert!(
        generous.admit(&jwks, &idp.sign(stale), "sales").is_ok(),
        "the leeway field is not reaching the validation"
    );
}

#[test]
fn a_token_with_no_expiry_is_refused() {
    // A JWT with no `exp` never stops being valid, which makes revocation impossible.
    let idp = Idp::new("k1");
    let jwks = Jwks::parse(&idp.jwks()).unwrap();
    let mut forever = claims();
    forever.as_object_mut().unwrap().remove("exp");
    assert!(matches!(
        apps().admit(&jwks, &idp.sign(forever), "sales"),
        Err(AuthError::Rejected(_))
    ));
}

#[test]
fn a_token_for_another_service_or_another_issuer_is_refused() {
    let idp = Idp::new("k1");
    let jwks = Jwks::parse(&idp.jwks()).unwrap();

    // The same provider mints tokens for many services. One of them is not this one, and a
    // verifier that skipped `aud` would accept every token the company issues.
    let mut elsewhere = claims();
    elsewhere["aud"] = serde_json::json!("grafana");
    assert!(matches!(
        apps().admit(&jwks, &idp.sign(elsewhere), "sales"),
        Err(AuthError::Rejected(_))
    ));

    let mut stranger = claims();
    stranger["iss"] = serde_json::json!("https://evil.test");
    assert!(matches!(
        apps().admit(&jwks, &idp.sign(stranger), "sales"),
        Err(AuthError::Rejected(_))
    ));

    for missing in ["aud", "iss"] {
        let mut without = claims();
        without.as_object_mut().unwrap().remove(missing);
        assert!(
            matches!(
                apps().admit(&jwks, &idp.sign(without), "sales"),
                Err(AuthError::Rejected(_))
            ),
            "a token with no {missing} was accepted"
        );
    }
}

#[test]
fn alg_none_is_refused_and_cannot_even_be_allowlisted() {
    // Hand-built, because no signing library will produce one: header `{"alg":"none"}`, a
    // real-looking payload, empty signature.
    let idp = Idp::new("k1");
    let jwks = Jwks::parse(&idp.jwks()).unwrap();
    let header = B64.encode(br#"{"alg":"none","typ":"JWT","kid":"k1"}"#);
    let payload = B64.encode(serde_json::to_vec(&claims()).unwrap());
    let token = format!("{header}.{payload}.");

    let err = apps().admit(&jwks, &token, "sales").unwrap_err();
    assert!(
        matches!(err, AuthError::Malformed(_) | AuthError::Algorithm { .. }),
        "an unsigned token got further than the header: {err:?}"
    );
    // And the policy could not have been configured to accept it: the enum has no variant.
    assert!(Algorithm::parse("none").is_err());
}

#[test]
fn an_hs256_token_signed_with_the_public_key_is_refused() {
    // The classic algorithm-confusion attack: take the verifier's PUBLIC key, use it as an
    // HMAC secret, and sign a token that asks to be verified with HS256. A verifier that
    // trusts the token's own `alg` header treats the public key as a shared secret and lets
    // the attacker mint anything.
    let idp = Idp::new("k1");
    let jwks = Jwks::parse(&idp.jwks()).unwrap();

    // The public key exactly as the JWKS publishes it — which is what an attacker has.
    let secret = format!("{}{}", idp.x, idp.y);
    let mut header = Header::new(jsonwebtoken::Algorithm::HS256);
    header.kid = Some("k1".into());
    let forged = encode(
        &header,
        &claims(),
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .unwrap();

    match apps().admit(&jwks, &forged, "sales") {
        Err(AuthError::Algorithm { asked }) => assert!(asked.contains("HS256"), "{asked}"),
        other => panic!("an HS256 forgery got past the allowlist: {other:?}"),
    }
    // Closed twice: even if the allowlist were somehow bypassed, `Algorithm` cannot name a
    // symmetric algorithm and the JWKS loader skips `oct` keys, so there is no secret here
    // to verify against.
    assert!(Algorithm::parse("HS256").is_err());
    assert!(matches!(
        Jwks::parse(r#"{"keys":[{"kty":"oct","kid":"k1","k":"c2VjcmV0"}]}"#),
        Err(JwksError::NoUsableKeys { .. })
    ));
}

#[test]
fn an_algorithm_off_the_allowlist_is_refused_even_though_the_key_would_verify_it() {
    // ES256 is a perfectly good algorithm and this policy does not permit it. A token is not
    // acceptable because it *could* be verified; it is acceptable because it was allowed.
    let idp = Idp::new("k1");
    let jwks = Jwks::parse(&idp.jwks()).unwrap();
    let mut rsa_only = apps();
    rsa_only.algorithms = BTreeSet::from([Algorithm::RS256]);

    match rsa_only.admit(&jwks, &idp.sign(claims()), "sales") {
        Err(AuthError::Algorithm { asked }) => assert!(asked.contains("ES256"), "{asked}"),
        other => panic!("expected an allowlist refusal, got {other:?}"),
    }
}

#[test]
fn a_kid_that_names_no_key_is_refused_and_so_is_an_absent_one_when_there_are_several() {
    let a = Idp::new("k1");
    let b = Idp::new("k2");
    let two = format!(
        r#"{{"keys":[{},{}]}}"#,
        &a.jwks()[9..a.jwks().len() - 2],
        &b.jwks()[9..b.jwks().len() - 2]
    );
    let jwks = Jwks::parse(&two).unwrap();
    assert_eq!(jwks.len(), 2);

    // Signed by a key the JWKS does not have.
    let stranger = Idp::new("k9");
    match apps().admit(&jwks, &stranger.sign(claims()), "sales") {
        Err(AuthError::UnknownKey { kid }) => assert_eq!(kid.as_deref(), Some("k9")),
        other => panic!("expected UnknownKey, got {other:?}"),
    }

    // No `kid` and two keys: trying each until one fits would make the outcome depend on map
    // order, and "which key signed this" stops being answerable from the token.
    let mut header = Header::new(jsonwebtoken::Algorithm::ES256);
    header.kid = None;
    let anonymous = encode(&header, &claims(), &EncodingKey::from_ec_der(&a.pkcs8)).unwrap();
    assert!(matches!(
        apps().admit(&jwks, &anonymous, "sales"),
        Err(AuthError::UnknownKey { kid: None })
    ));

    // With exactly one key there is nothing to choose, so an anonymous token resolves.
    let one = Jwks::parse(&a.jwks()).unwrap();
    assert!(apps().admit(&one, &anonymous, "sales").is_ok());
}

// ── the JWKS itself ────────────────────────────────────────────────────────────────────────

#[test]
fn a_jwks_that_would_refuse_everyone_is_an_error_rather_than_an_empty_key_set() {
    // Starting a server with no usable key is starting a door that never opens, and it would
    // look exactly like every viewer's token being wrong.
    for (document, what) in [
        (r#"{"keys":[]}"#, "empty"),
        (
            r#"{"keys":[{"kty":"oct","kid":"a","k":"c2VjcmV0"}]}"#,
            "symmetric",
        ),
        (
            r#"{"keys":[{"kty":"RSA","kid":"a","e":"AQAB"}]}"#,
            "RSA without n",
        ),
        (
            r#"{"keys":[{"kty":"EC","kid":"a","x":"abc"}]}"#,
            "EC without y",
        ),
    ] {
        assert!(
            matches!(Jwks::parse(document), Err(JwksError::NoUsableKeys { .. })),
            "{what} was accepted"
        );
    }
    assert!(matches!(
        Jwks::parse("not json"),
        Err(JwksError::Malformed(_))
    ));
}

#[test]
fn an_encryption_key_is_skipped_by_name_rather_than_used_to_verify() {
    let idp = Idp::new("k1");
    let enc = format!(
        r#"{{"keys":[{{"kty":"EC","crv":"P-256","use":"enc","kid":"{}","x":"{}","y":"{}"}}]}}"#,
        idp.kid, idp.x, idp.y
    );
    match Jwks::parse(&enc) {
        Err(JwksError::NoUsableKeys { reasons, .. }) => {
            assert!(reasons[0].contains("not sig"), "{reasons:?}");
        }
        other => panic!("an `enc` key was loaded for verification: {other:?}"),
    }
}
