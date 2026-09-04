//! `usage-probe-claude` — Anthropic subscription (Claude Max) probe.
//!
//! Reads remaining quota by making one minimal request and inspecting the
//! `anthropic-ratelimit-unified-*` response headers. Nothing on disk carries
//! this: `~/.claude/stats-cache.json` is activity counts and `policy-limits.json`
//! is unrelated, so it must be probed.
//!
//! **This probe spends the allowance it measures** — one Haiku token per
//! observation — so it declares `quota-consuming` and core holds it to a long
//! minimum interval unless the user explicitly refreshes.
//!
//! The account returns three concurrent windows plus a `representative-claim`.
//! The vendor declines to reduce them to one number, and so do we.

use std::process::ExitCode;

use continuum_usage_core::envelope::{
    Facets, FailureKind, KindHint, Monetary, Observation, ObservationCost, Outcome, Resource,
    SideEffect,
};

const PROBE: &str = "claude";
const PROVIDER: &str = "anthropic";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const API: &str = "https://api.anthropic.com/v1/messages";
/// Smallest possible real request. One token.
const PROBE_MODEL: &str = "claude-haiku-4-5-20251001";

fn main() -> ExitCode {
    let obs = probe();
    // Contract: exactly one JSON object on stdout, success or failure.
    println!("{}", serde_json::to_string(&obs).expect("envelope serialises"));
    match obs.outcome {
        Outcome::Ok { .. } => ExitCode::SUCCESS,
        Outcome::Failure { .. } => ExitCode::FAILURE,
    }
}

fn fail(kind: FailureKind, msg: impl Into<String>) -> Observation {
    Observation::failure(PROBE, VERSION, PROVIDER, kind, msg)
}

/// `Debug` deliberately omitted: this holds a live OAuth token, and a derived
/// `Debug` is exactly how a secret ends up in a log line or a panic message.
/// Tests assert on the error string instead, never on this value.
struct Credentials {
    access_token: String,
    subscription: Option<String>,
    /// Claude Code's own `expiresAt` (Unix milliseconds) for the access token.
    /// Absent from older blobs; when present it is authoritative, and the
    /// probe refuses to spend a request on a token the vendor will reject.
    expires_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CredentialOrigin {
    Primary,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    MacOsFileFallback,
}

/// Provenance stays beside the parsed credential so an expired fallback is
/// described accurately without ever logging the credential itself.
struct LoadedCredentials {
    credentials: Credentials,
    origin: CredentialOrigin,
}

struct CredentialSource {
    label: &'static str,
    origin: CredentialOrigin,
    load: fn() -> Result<String, String>,
}

/// How long the access token has been expired, if it has. `None` when the
/// blob carries no expiry (older Claude Code) or the token is still live.
fn expired_for_secs(expires_at_ms: Option<i64>, now_ms: i64) -> Option<i64> {
    let exp = expires_at_ms?;
    (now_ms >= exp).then(|| (now_ms - exp) / 1000)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn human_duration(secs: i64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s => format!("{}h {}m", s / 3600, (s % 3600) / 60),
    }
}

fn stale_credentials_message(origin: CredentialOrigin, secs: i64) -> String {
    match origin {
        CredentialOrigin::MacOsFileFallback => concat!(
            "macOS keychain unavailable to this probe; fallback token is stale. ",
            "Claude Code renews the live token on this machine's next Claude Code session ",
            "(no re-authentication needed). No request spent; the last reading stands."
        )
        .to_string(),
        CredentialOrigin::Primary => format!(
            "Claude Code's OAuth access token expired {} ago; it renews itself on this \
             machine's next Claude Code session (no re-authentication needed). No \
             request spent; the last reading stands.",
            human_duration(secs)
        ),
    }
}

/// macOS keychain service under which Claude Code stores the credential blob.
#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

/// Where the Claude Code credential actually lives, in the order we try.
///
/// **This is platform-dependent and getting it wrong is silent.** On Linux the
/// JSON file is the live store. On macOS it is the *login keychain*, and
/// `~/.claude/.credentials.json` is a stale artefact left behind by an older
/// Claude Code: on the machine where this was found the file was frozen at
/// 2026-07-25 while the keychain item's modification date tracked the current
/// session. Reading only the file made every macOS observation report
/// `InvalidCredentials` on a machine that was authenticated and working — a
/// false negative that looked exactly like a real auth failure.
///
/// So macOS tries the keychain first and falls back to the file; every other
/// platform reads the file, as before. Both are parsed by the same code, since
/// the keychain item holds the same JSON blob.
fn credential_sources() -> Vec<CredentialSource> {
    #[cfg(target_os = "macos")]
    {
        vec![
            CredentialSource {
                label: "macOS login keychain",
                origin: CredentialOrigin::Primary,
                load: read_keychain,
            },
            CredentialSource {
                label: "~/.claude/.credentials.json",
                origin: CredentialOrigin::MacOsFileFallback,
                load: read_credentials_file,
            },
        ]
    }

    #[cfg(not(target_os = "macos"))]
    {
        vec![CredentialSource {
            label: "~/.claude/.credentials.json",
            origin: CredentialOrigin::Primary,
            load: read_credentials_file,
        }]
    }
}

/// Read the credential blob from the macOS login keychain via `security`.
///
/// Shelling out rather than linking a Security-framework crate is deliberate:
/// it adds no dependency, and `/usr/bin/security` is Apple-signed, so its
/// keychain access is governed by the item's own ACL rather than by this
/// binary's code signature — which matters here because the probe is rebuilt
/// often and an ad-hoc signature changes identity on every rebuild.
#[cfg(target_os = "macos")]
fn read_keychain() -> Result<String, String> {
    let out = std::process::Command::new("security")
        .args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-w"])
        .output()
        .map_err(|e| format!("cannot run `security`: {e}"))?;
    if !out.status.success() {
        // Item absent, or the user declined the keychain prompt. Either way
        // this is not fatal — the file fallback is tried next.
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if err.is_empty() {
            format!("no keychain item for service {KEYCHAIN_SERVICE:?}")
        } else {
            err
        });
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        return Err(format!("keychain item {KEYCHAIN_SERVICE:?} is empty"));
    }
    Ok(text)
}

fn read_credentials_file() -> Result<String, String> {
    let home = std::env::var("HOME").map_err(|_| "HOME is not set".to_string())?;
    let path = format!("{home}/.claude/.credentials.json");
    std::fs::read_to_string(&path).map_err(|e| format!("cannot read {path}: {e}"))
}

/// Pull the OAuth token out of a Claude Code credential blob.
fn parse_credentials(text: &str) -> Result<Credentials, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
    let oauth = value
        .get("claudeAiOauth")
        .ok_or_else(|| "no claudeAiOauth block".to_string())?;
    let access_token = oauth
        .get("accessToken")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "no accessToken present".to_string())?
        .to_string();
    Ok(Credentials {
        access_token,
        // Not a secret and not identifying: "max", "pro". Used as the account
        // pseudonym so history can be attributed without storing anything.
        subscription: oauth
            .get("subscriptionType")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        // Claude Code writes this as a JSON number of milliseconds. Tolerate
        // a float (serde_json may parse large numbers that way) and absence.
        expires_at_ms: oauth.get("expiresAt").and_then(|v| {
            v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))
        }),
    })
}

fn read_credentials() -> Result<LoadedCredentials, Observation> {
    // Report every source tried, so a failure says which stores were consulted
    // rather than implying the one hard-coded path is the only one there is.
    let mut tried: Vec<String> = Vec::new();
    for source in credential_sources() {
        match (source.load)().and_then(|text| parse_credentials(&text)) {
            Ok(credentials) => {
                return Ok(LoadedCredentials {
                    credentials,
                    origin: source.origin,
                })
            }
            Err(why) => tried.push(format!("{}: {why}", source.label)),
        }
    }
    Err(fail(
        FailureKind::InvalidCredentials,
        format!("no usable Claude Code credential ({})", tried.join("; ")),
    ))
}

/// Pull one `anthropic-ratelimit-unified-<window>-<field>` header.
fn header<'a>(get: &'a dyn Fn(&str) -> Option<String>, window: &str, field: &str) -> Option<String> {
    get(&format!("anthropic-ratelimit-unified-{window}-{field}"))
}

/// Anthropic sends utilization as a 0..1 fraction. Refuse anything else rather
/// than asserting it: a switch to percent would make 34.0 exceed every
/// threshold and pin the window permanently `Critical`, and a non-finite value
/// serialises as `null` into a non-optional field, which makes core discard the
/// entire observation.
fn fraction(raw: Option<String>) -> Option<f64> {
    let v = raw?.parse::<f64>().ok()?;
    (v.is_finite() && (0.0..=1.0).contains(&v)).then_some(v)
}

fn window_resource(
    get: &dyn Fn(&str) -> Option<String>,
    window: &str,
    label: &str,
    window_secs: i64,
    representative: bool,
) -> Option<Resource> {
    let status = header(&get, window, "status");
    let resets_at = header(&get, window, "reset").and_then(|v| v.parse::<i64>().ok());
    let utilization = fraction(header(&get, window, "utilization"));

    // Gate on ANY of the three headers, not on utilization alone. A rejected
    // window plausibly carries no meaningful percentage, and dropping the whole
    // resource would discard `status` — the vendor's own verdict, and the most
    // decision-relevant fact there is. Kept with utilization absent, it renders
    // `NotAssessable` via `implies_capacity`, which is the truthful outcome.
    if utilization.is_none() && resets_at.is_none() && status.is_none() {
        return None;
    }

    Some(Resource {
        id: format!("unified-{window}"),
        label: label.to_string(),
        kind_hint: KindHint::ResetWindow,
        facets: Facets {
            utilization,
            resets_at,
            window_secs: Some(window_secs),
            // Unused allowance in a subscription window is lost at the reset.
            // This is what makes "might as well" meaningful here.
            expires_unused: Some(true),
            ..Default::default()
        },
        vendor_status: status,
        vendor_representative: representative,
    })
}

/// The overage meter: credit spend, inside a window-shaped plan.
///
/// This single row is why the round-1 tagged union was rejected. It carries a
/// reset *and* spend semantics. `expires_unused: false` is the load-bearing
/// field — without it core would read the reset as perishable and cheerfully
/// advise burning credit.
fn overage_resource(get: &dyn Fn(&str) -> Option<String>) -> Option<Resource> {
    let status = header(&get, "overage", "status");
    let resets_at = header(&get, "overage", "reset").and_then(|v| v.parse::<i64>().ok());
    let utilization = fraction(header(&get, "overage", "utilization"));
    if utilization.is_none() && resets_at.is_none() && status.is_none() {
        return None;
    }
    Some(Resource {
        id: "unified-overage".to_string(),
        label: "Overage (credit spend)".to_string(),
        kind_hint: KindHint::Continuous,
        facets: Facets {
            utilization,
            resets_at,
            expires_unused: Some(false),
            monetary: Some(Monetary {
                currency: "USD".to_string(),
                spent: None,
                cap: None,
            }),
            ..Default::default()
        },
        vendor_status: status,
        vendor_representative: false,
    })
}

fn probe() -> Observation {
    let loaded = match read_credentials() {
        Ok(c) => c,
        Err(obs) => return obs,
    };
    let LoadedCredentials {
        credentials: creds,
        origin,
    } = loaded;

    // Claude Code refreshes this token only while a session is running, and
    // it lives about eight hours — so on an idle machine it expires as a
    // matter of routine. That is not a credential fault: the vendor will say
    // 401, but the fix is the next Claude Code session, not a re-login. Say so,
    // spend nothing, and leave the last real reading in place.
    if let Some(secs) = expired_for_secs(creds.expires_at_ms, now_ms()) {
        return fail(
            FailureKind::StaleCredentials,
            stale_credentials_message(origin, secs),
        );
    }

    let body = serde_json::json!({
        "model": PROBE_MODEL,
        "max_tokens": 1,
        "messages": [{"role": "user", "content": "hi"}]
    })
    .to_string();

    let request = ureq::post(API)
        .set("authorization", &format!("Bearer {}", creds.access_token))
        .set("anthropic-version", "2023-06-01")
        .set("anthropic-beta", "oauth-2025-04-20")
        .set("content-type", "application/json")
        .timeout(std::time::Duration::from_secs(20));

    // A 429 still carries the rate-limit headers, and those headers are the
    // whole point — so a throttle is a *reading*, not an error to discard.
    let (response, throttled) = match request.send_string(&body) {
        Ok(r) => (r, false),
        Err(ureq::Error::Status(429, r)) => (r, true),
        Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => {
            // The expiry pre-check above already handled the routine case, so
            // a rejection here means an unexpired token was refused: revoked,
            // rotated elsewhere, or a blob with no expiry field. That one does
            // warrant a re-login.
            return fail(
                FailureKind::InvalidCredentials,
                "Anthropic rejected an unexpired OAuth token (401/403); re-authenticate Claude Code",
            )
        }
        Err(ureq::Error::Status(402, r)) => {
            return fail(
                FailureKind::QuotaDenied,
                format!(
                    "402 Payment Required: {}",
                    r.into_string().unwrap_or_default().chars().take(300).collect::<String>()
                ),
            )
        }
        Err(ureq::Error::Status(code, _)) if (500..600).contains(&code) => {
            return fail(FailureKind::ProviderOutage, format!("Anthropic returned {code}"))
        }
        Err(ureq::Error::Status(code, _)) => {
            return fail(FailureKind::MalformedResponse, format!("unexpected status {code}"))
        }
        Err(ureq::Error::Transport(t)) => {
            return fail(FailureKind::NetworkFailure, format!("transport error: {t}"))
        }
    };

    let headers: Vec<(String, String)> = response
        .headers_names()
        .into_iter()
        .filter_map(|n| response.header(&n).map(|v| (n.to_lowercase(), v.to_string())))
        .collect();
    let get = move |name: &str| -> Option<String> {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    };

    // Which window the vendor considers representative of the account. Carried
    // as a display hint; core still renders every resource.
    let representative = get("anthropic-ratelimit-unified-representative-claim");
    let rep_id = match representative.as_deref() {
        Some("five_hour") => "5h",
        Some("seven_day") => "7d",
        _ => "",
    };

    let mut resources = vec![];
    if let Some(r) = window_resource(&get, "5h", "Session (5 hours)", 5 * 3600, rep_id == "5h") {
        resources.push(r);
    }
    if let Some(r) = window_resource(&get, "7d", "Weekly (7 days)", 7 * 86_400, rep_id == "7d") {
        resources.push(r);
    }
    if let Some(r) = overage_resource(&get) {
        resources.push(r);
    }

    if resources.is_empty() {
        return fail(
            FailureKind::MalformedResponse,
            "response carried no anthropic-ratelimit-unified-* headers",
        );
    }

    let mut obs = Observation::ok(
        PROBE,
        VERSION,
        PROVIDER,
        SideEffect::QuotaConsuming,
        resources,
    );
    obs.assistant = Some("claude-code".to_string());
    obs.account = creds.subscription;
    if let Outcome::Ok { cost, raw, .. } = &mut obs.outcome {
        *cost = Some(ObservationCost {
            requests: Some(1),
            tokens: Some(1),
            note: Some("one minimal Haiku request; spends the quota it measures".into()),
        });
        *raw = Some(serde_json::json!({
            "unified_status": get("anthropic-ratelimit-unified-status"),
            "representative_claim": representative,
            "fallback_percentage": get("anthropic-ratelimit-unified-fallback-percentage"),
            "throttled": throttled,
        }));
    }
    obs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape Claude Code writes, in the file and in the keychain item alike
    /// — which is the whole reason one parser serves both sources.
    const BLOB: &str = r#"{"claudeAiOauth":{"accessToken":"tok-123","subscriptionType":"max"}}"#;

    #[test]
    fn parses_token_and_subscription() {
        let c = parse_credentials(BLOB).expect("valid blob parses");
        assert_eq!(c.access_token, "tok-123");
        assert_eq!(c.subscription.as_deref(), Some("max"));
    }

    #[test]
    fn parses_expiry_when_present_and_tolerates_absence() {
        let c = parse_credentials(
            r#"{"claudeAiOauth":{"accessToken":"t","expiresAt":1788553396591}}"#,
        )
        .expect("blob with expiry parses");
        assert_eq!(c.expires_at_ms, Some(1_788_553_396_591));
        let c = parse_credentials(BLOB).expect("blob without expiry parses");
        assert_eq!(c.expires_at_ms, None);
    }

    /// The whole point of the pre-check: an expired token must be reported as
    /// stale *before* any request, and a live or unknown one must not be.
    #[test]
    fn expiry_precheck_is_strict_and_tolerant() {
        let now = 1_000_000_000_000;
        assert_eq!(expired_for_secs(Some(now - 90_000), now), Some(90));
        assert_eq!(expired_for_secs(Some(now), now), Some(0));
        assert_eq!(expired_for_secs(Some(now + 1), now), None);
        assert_eq!(expired_for_secs(None, now), None);
    }

    #[test]
    fn human_duration_reads_naturally() {
        assert_eq!(human_duration(45), "45s");
        assert_eq!(human_duration(600), "10m");
        assert_eq!(human_duration(3600 * 14 + 120), "14h 2m");
    }

    #[test]
    fn stale_fallback_names_the_unavailable_keychain_not_epoch_time() {
        let message =
            stale_credentials_message(CredentialOrigin::MacOsFileFallback, 496_820 * 3600);
        assert!(message.contains("keychain unavailable to this probe"));
        assert!(message.contains("fallback token is stale"));
        assert!(!message.contains("496820"));
        assert!(!message.contains("ago"));

        let primary = stale_credentials_message(CredentialOrigin::Primary, 90);
        assert!(primary.contains("1m ago"));
    }

    #[test]
    fn subscription_is_optional() {
        let c = parse_credentials(r#"{"claudeAiOauth":{"accessToken":"t"}}"#)
            .expect("token alone is enough");
        assert_eq!(c.subscription, None);
    }

    /// Each rejection must say which thing was missing: these messages are
    /// joined into the failure envelope and are all a reader gets.
    #[test]
    fn rejects_malformed_blobs_with_a_reason() {
        for (input, expect) in [
            ("not json at all", "not JSON"),
            (r#"{"somethingElse":{}}"#, "no claudeAiOauth block"),
            (r#"{"claudeAiOauth":{}}"#, "no accessToken present"),
            (r#"{"claudeAiOauth":{"accessToken":42}}"#, "no accessToken present"),
        ] {
            // Not `expect_err`: that would need `Debug` on `Credentials`, and
            // deriving it would put a live token one panic away from a log.
            let err = match parse_credentials(input) {
                Ok(_) => panic!("{input:?} should have been rejected"),
                Err(e) => e,
            };
            assert!(
                err.contains(expect),
                "{input:?} should report {expect:?}, got {err:?}"
            );
        }
    }

    /// Trailing whitespace matters: `security -w` emits a trailing newline, so
    /// the keychain path hands us a string the file path never would.
    #[test]
    fn tolerates_trailing_newline_from_security_cli() {
        let c = parse_credentials(&format!("{BLOB}\n")).expect("trailing newline is fine");
        assert_eq!(c.access_token, "tok-123");
    }

    /// The file must stay in the list on every platform, and on macOS the
    /// keychain must be tried FIRST — the stale-file bug was precisely the
    /// wrong order.
    #[test]
    fn source_order_is_platform_correct() {
        let sources = credential_sources();
        let labels: Vec<&str> = sources.iter().map(|source| source.label).collect();
        assert!(labels.contains(&"~/.claude/.credentials.json"));
        #[cfg(target_os = "macos")]
        {
            assert_eq!(labels[0], "macOS login keychain");
            assert_eq!(sources[1].origin, CredentialOrigin::MacOsFileFallback);
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert_eq!(labels.len(), 1);
            assert_eq!(sources[0].origin, CredentialOrigin::Primary);
        }
    }
}
