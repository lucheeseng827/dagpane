//! Turning command-line flags into a front door, or refusing to start.
//!
//! Every refusal here happens **before the socket binds**. A server that starts with an
//! unusable policy and then turns every viewer away looks, from outside, exactly like a
//! server whose viewers all have bad tokens — and it is the operator who has to tell the
//! difference at the worst possible moment.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use dagpane_auth::{Access, Algorithm, Jwks, Policy};
use dagpane_serve::{Auth, Login};

use crate::args::AuthArgs;

/// Build the front door the flags describe.
///
/// `Ok(None)` means the operator asked for no authentication. That is a legitimate choice —
/// loopback, or something already in front — and the caller warns about it rather than this
/// function refusing.
///
/// # Errors
///
/// A message for the operator, naming the flag that is missing or wrong.
pub fn front_door(args: &AuthArgs) -> Result<Option<Arc<Auth>>, String> {
    let Some(jwks_path) = &args.auth_jwks else {
        // Every other flag is meaningless without keys. Silently ignoring them would let an
        // operator believe they had configured authentication when they had configured
        // nothing, which is the worst of the three possible outcomes.
        let stray = [
            ("--auth-issuer", args.auth_issuer.is_some()),
            ("--auth-audience", args.auth_audience.is_some()),
            ("--auth-apps-claim", args.auth_apps_claim.is_some()),
            ("--auth-any-app", args.auth_any_app),
            ("--auth-client-id", args.auth_client_id.is_some()),
        ];
        let named: Vec<&str> = stray
            .iter()
            .filter(|(_, set)| *set)
            .map(|(n, _)| *n)
            .collect();
        if !named.is_empty() {
            return Err(format!(
                "{} was given without --auth-jwks, so nothing is authenticated. Point \
                 --auth-jwks at the provider's JWKS document to turn the front door on.",
                named.join(", ")
            ));
        }
        return Ok(None);
    };

    let issuer = args.auth_issuer.clone().ok_or(
        "--auth-jwks needs --auth-issuer: a token from another provider is still a \
                valid token, and it is not one of yours",
    )?;
    let audience = args.auth_audience.clone().ok_or(
        "--auth-jwks needs --auth-audience: your provider mints tokens for every service \
         you run, and without this any of them opens this one",
    )?;

    let mut algorithms = BTreeSet::new();
    for name in args
        .auth_alg
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        algorithms.insert(Algorithm::parse(name)?);
    }
    if algorithms.is_empty() {
        return Err("--auth-alg is empty, so every token would be refused".into());
    }

    // The one decision with no safe default. Requiring a claim nobody mints refuses everyone
    // on the first day and looks broken; letting any valid token open any app is an access
    // control nobody notices is missing. So the operator says which, in the open.
    let access = match (&args.auth_apps_claim, args.auth_any_app) {
        (Some(_), true) => {
            return Err("--auth-apps-claim and --auth-any-app contradict each other".into())
        }
        (Some(claim), false) => Access::Claim(claim.clone()),
        (None, true) => Access::AnyApp,
        (None, false) => {
            return Err(
                "choose how a token says which apps it may open: --auth-apps-claim <name> \
                 for a claim listing app ids (`*` for all), or --auth-any-app to let any \
                 valid token open anything this process serves. There is no default because \
                 both wrong answers are silent ones."
                    .into(),
            )
        }
    };

    let policy = Policy {
        issuer,
        audience,
        algorithms,
        leeway_seconds: args.auth_leeway_secs,
        access,
    };
    policy.check()?;

    let jwks = Jwks::from_file(Path::new(jwks_path)).map_err(|e| e.to_string())?;

    // A browser flow needs all three or none. Two of them is a sign-in button that fails
    // after the redirect, which is the worst place to find out.
    let login = match (
        &args.auth_authorize_url,
        &args.auth_token_url,
        &args.auth_client_id,
    ) {
        (Some(authorize), Some(token), Some(client)) => Some(Login {
            authorize_endpoint: authorize.clone(),
            token_endpoint: token.clone(),
            client_id: client.clone(),
            scope: args.auth_scope.clone(),
        }),
        (None, None, None) => None,
        _ => {
            return Err(
                "a browser sign-in needs all three of --auth-authorize-url, \
                 --auth-token-url and --auth-client-id, or none of them. With none, the \
                 page says a token has to arrive another way — which is right when \
                 something in front already issues one."
                    .into(),
            )
        }
    };

    Ok(Some(Arc::new(Auth {
        jwks,
        policy,
        login,
    })))
}

/// What to print at start-up about who can reach this.
///
/// The `--host` warning is not deleted when a front door exists — it is **replaced**, because
/// binding a public address is still a thing an operator should see confirmed, and because
/// "there is a door" and "the door is the one you meant" are different claims.
pub fn describe(addr: &std::net::SocketAddr, auth: Option<&Auth>, apps: &str) {
    match auth {
        Some(auth) => {
            let access = match &auth.policy.access {
                Access::AnyApp => "any valid token opens it".to_string(),
                Access::Claim(name) => format!("a token's `{name}` must list it"),
            };
            println!(
                "dagpane: authenticated — {} key(s), issuer {}, audience {}; {access}",
                auth.jwks.len(),
                auth.policy.issuer,
                auth.policy.audience,
            );
            if auth.login.is_none() {
                println!(
                    "         no browser sign-in configured: a token has to arrive from \
                     something in front"
                );
            }
        }
        None if addr.ip().is_loopback() => {}
        None => {
            eprintln!(
                "dagpane: warning — binding {addr}, which is reachable from outside this \
                 machine,\n         with no authentication. Anyone who can reach that \
                 address can read every\n         pane of {apps}. Pass --auth-jwks to put a \
                 front door on it, or put one in\n         front. See SECURITY.md."
            );
        }
    }
}
