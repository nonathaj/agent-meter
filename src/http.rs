//! The HTTP client used for provider APIs.

use std::sync::OnceLock;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::Value;
use ureq::Body;
use ureq::http::Response;

/// Set this to a non-empty value other than `0` to stop agent-meter from
/// contacting the providers at all. Everything that works from stored data
/// keeps working; usage readings simply go stale.
pub const OFFLINE_ENV: &str = "AGENT_METER_OFFLINE";

/// User agent sent with every request.
pub fn user_agent() -> String {
    format!("agent-meter/{}", env!("CARGO_PKG_VERSION"))
}

/// Whether network access is switched off for this run.
pub fn is_offline() -> bool {
    offline_from(std::env::var_os(OFFLINE_ENV).as_deref())
}

/// The rule [`is_offline`] applies, separated from the environment so it can be
/// tested without mutating process-wide state that other tests are reading.
fn offline_from(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|value| !value.is_empty() && value != "0")
}

/// A shared agent: connection pooling matters because the watcher polls the
/// same few hosts indefinitely.
fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(20)))
            // Provider APIs never legitimately redirect; following one would
            // leak the bearer token to whatever host it pointed at.
            .max_redirects(0)
            // Status codes are classified by `read_json`, which needs the body.
            .http_status_as_error(false)
            .user_agent(user_agent())
            .build()
            .into()
    })
}

/// GETs JSON from `url` with the given headers.
pub fn get_json<T: DeserializeOwned>(url: &str, headers: &[(&str, &str)]) -> Result<T> {
    get_json_unless(is_offline(), url, headers)
}

/// POSTs a JSON body to `url` and reads a JSON response.
pub fn post_json<T: DeserializeOwned>(url: &str, body: Value) -> Result<T> {
    if is_offline() {
        return Err(Error::Offline);
    }
    read_json(url, agent().post(url).send_json(body))
}

/// The body of [`get_json`], with the offline decision passed in so a test can
/// prove that being offline sends nothing at all.
fn get_json_unless<T: DeserializeOwned>(offline: bool, url: &str, headers: &[(&str, &str)]) -> Result<T> {
    if offline {
        return Err(Error::Offline);
    }
    let mut request = agent().get(url);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    read_json(url, request.call())
}

/// Convenience for the bearer header every provider endpoint expects.
pub fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

/// What went wrong with a provider request.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The credential was rejected (401/403); refresh or re-login is needed.
    #[error("{message}")]
    Unauthorized { message: String, code: Option<String> },
    /// We are being rate limited; retry no sooner than `retry_after`.
    #[error("rate limited by the provider")]
    RateLimited { retry_after: Option<Duration> },
    /// Any other unsuccessful HTTP status.
    #[error("HTTP {status} from {url}: {body}")]
    Status { status: u16, url: String, body: String },
    /// Transport failure, timeout, or a malformed response body.
    #[error(transparent)]
    Transport(#[from] anyhow::Error),
    /// Network access is switched off.
    #[error("offline: {OFFLINE_ENV} is set, so no usage was read")]
    Offline,
}

impl Error {
    /// Whether retrying the same request later could plausibly succeed.
    pub fn is_transient(&self) -> bool {
        match self {
            Error::Unauthorized { .. } => false,
            // Offline is "transient" in the sense that matters here: nothing is
            // wrong with the credential, so nothing should be marked broken.
            Error::RateLimited { .. } | Error::Transport(_) | Error::Offline => true,
            Error::Status { status, .. } => *status >= 500 || *status == 408,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Classifies a response and deserializes its JSON body.
///
/// `url` is only used for error messages; it never carries credentials because
/// every provider endpoint takes its token in a header.
fn read_json<T: DeserializeOwned>(
    url: &str,
    response: std::result::Result<Response<Body>, ureq::Error>,
) -> Result<T> {
    let mut response =
        response.map_err(|e| Error::Transport(anyhow::Error::new(e).context(format!("requesting {url}"))))?;

    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    let text = response.body_mut().read_to_string().map_err(|e| {
        Error::Transport(anyhow::Error::new(e).context(format!("reading the response from {url}")))
    })?;

    match status {
        200..=299 => serde_json::from_str(&text).map_err(|e| {
            Error::Transport(anyhow::Error::new(e).context(format!("parsing the response from {url}")))
        }),
        401 | 403 => Err(Error::Unauthorized {
            message: describe(status, &text),
            code: serde_json::from_str(&text).ok().as_ref().and_then(error_code),
        }),
        429 => Err(Error::RateLimited { retry_after }),
        _ => Err(Error::Status {
            status,
            url: url.to_string(),
            body: detail(&text),
        }),
    }
}

/// Renders an error body as a single line, preferring its structured message.
fn describe(status: u16, body: &str) -> String {
    let detail = detail(body);
    if detail.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {detail}")
    }
}

fn detail(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| error_message(&v))
        .unwrap_or_else(|| truncate(body, 200))
}

/// Pulls a message out of the several error shapes providers use.
pub fn error_message(value: &Value) -> Option<String> {
    let error = value.get("error")?;
    if let Some(text) = error.as_str() {
        let description = value
            .get("error_description")
            .and_then(Value::as_str)
            .map(|d| format!(" ({d})"))
            .unwrap_or_default();
        return Some(format!("{text}{description}"));
    }
    let message = error.get("message").and_then(Value::as_str);
    let code = error
        .get("code")
        .and_then(Value::as_str)
        .or_else(|| error.get("type").and_then(Value::as_str));
    match (code, message) {
        (Some(code), Some(message)) => Some(format!("{code}: {message}")),
        (Some(text), None) | (None, Some(text)) => Some(text.to_string()),
        (None, None) => None,
    }
}

/// Machine-readable error code from a provider error body, e.g. `invalid_grant`.
pub fn error_code(value: &Value) -> Option<String> {
    let error = value.get("error").or_else(|| value.get("code"))?;
    error
        .as_str()
        .or_else(|| error.get("code").and_then(Value::as_str))
        .or_else(|| error.get("type").and_then(Value::as_str))
        .map(ToString::to_string)
}

fn truncate(text: &str, max: usize) -> String {
    let text = text.trim();
    match text.char_indices().nth(max) {
        Some((idx, _)) => format!("{}…", &text[..idx]),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_messages_and_codes() {
        let oauth = json!({"error": "invalid_grant", "error_description": "expired"});
        assert_eq!(error_message(&oauth).unwrap(), "invalid_grant (expired)");
        assert_eq!(error_code(&oauth).unwrap(), "invalid_grant");

        let anthropic = json!({"error": {"type": "authentication_error", "message": "bad token"}});
        assert_eq!(
            error_message(&anthropic).unwrap(),
            "authentication_error: bad token"
        );
        assert_eq!(error_code(&anthropic).unwrap(), "authentication_error");

        assert!(error_message(&json!({"ok": true})).is_none());
    }

    #[test]
    fn transient_classification() {
        assert!(
            !Error::Unauthorized {
                message: "x".into(),
                code: None
            }
            .is_transient()
        );
        assert!(Error::RateLimited { retry_after: None }.is_transient());
        assert!(
            Error::Status {
                status: 503,
                url: String::new(),
                body: String::new()
            }
            .is_transient()
        );
        assert!(
            !Error::Status {
                status: 404,
                url: String::new(),
                body: String::new()
            }
            .is_transient()
        );
    }

    #[test]
    fn classifies_live_statuses() {
        let mut server = mockito::Server::new();
        let ok = server.mock("GET", "/ok").with_body(r#"{"value":7}"#).create();
        let denied = server
            .mock("GET", "/denied")
            .with_status(401)
            .with_body(r#"{"error":{"type":"authentication_error","message":"nope"}}"#)
            .create();
        let limited = server
            .mock("GET", "/limited")
            .with_status(429)
            .with_header("retry-after", "120")
            .with_body("{}")
            .create();

        #[derive(serde::Deserialize)]
        struct Payload {
            value: u32,
        }
        let url = format!("{}/ok", server.url());
        let payload: Payload = get_json(&url, &[("authorization", "Bearer x")]).unwrap();
        assert_eq!(payload.value, 7);

        let url = format!("{}/denied", server.url());
        let err = get_json::<Value>(&url, &[]).unwrap_err();
        assert!(matches!(&err, Error::Unauthorized { message, code }
            if message.contains("nope") && code.as_deref() == Some("authentication_error")));

        let url = format!("{}/limited", server.url());
        let err = get_json::<Value>(&url, &[]).unwrap_err();
        assert!(matches!(err, Error::RateLimited { retry_after: Some(d) } if d.as_secs() == 120));

        ok.assert();
        denied.assert();
        limited.assert();
    }

    #[test]
    fn offline_mode_makes_no_request_at_all() {
        let mut server = mockito::Server::new();
        // `expect(0)` fails the test if anything reaches the server.
        let never = server.mock("GET", "/usage").expect(0).create();
        let url = format!("{}/usage", server.url());

        let err = get_json_unless::<Value>(true, &url, &[]).unwrap_err();
        assert!(matches!(err, Error::Offline));
        // Offline must never be mistaken for a bad credential.
        assert!(err.is_transient());
        // The message has to name the switch, or nobody will know why.
        assert!(err.to_string().contains(OFFLINE_ENV));
        never.assert();
    }

    #[test]
    fn the_offline_switch_reads_the_usual_ways_of_saying_no() {
        let os = std::ffi::OsStr::new;
        assert!(!offline_from(None));
        assert!(!offline_from(Some(os(""))));
        assert!(!offline_from(Some(os("0"))));
        assert!(offline_from(Some(os("1"))));
        assert!(offline_from(Some(os("true"))));
    }
}
