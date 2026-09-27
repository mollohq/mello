use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::Rng;
use sha2::{Digest, Sha256};
use tiny_http::{Header, Method, Request, Response, Server};

const REDIRECT_PORT: u16 = 29405;
pub const REDIRECT_URI: &str = "http://localhost:29405/callback";

/// Path of `REDIRECT_URI`. Only a request to this path can complete a flow.
const CALLBACK_PATH: &str = "/callback";
/// Path that the implicit-flow page POSTs the fragment token and `state` to.
const TOKEN_PATH: &str = "/token";
/// How long a flow waits for the browser to come back, across all requests.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(120);
/// Upper bound on a `/token` body. A real body holds one token and one state.
const MAX_TOKEN_BODY: u64 = 8 * 1024;
/// The OAuth 2.0 `error` code for a user who refuses consent (RFC 6749 §4.1.2.1).
const ACCESS_DENIED: &str = "access_denied";
/// Upper bound on a provider error code that the app keeps and shows.
const MAX_ERROR_CODE: usize = 64;

/// The URL to use for a provider endpoint.
///
/// Test builds only (`e2e-oauth` feature): with `MELLO_E2E_OAUTH_BASE` set, the
/// scheme and host are replaced by the fake provider's, and the path and query
/// are kept, because the fake serves each provider's real paths
/// (plans/E2E-QA.md §8). Every other build returns the URL unchanged.
pub fn provider_url(real: &str) -> String {
    #[cfg(feature = "e2e-oauth")]
    if let Some(base) = std::env::var("MELLO_E2E_OAUTH_BASE")
        .ok()
        .filter(|b| !b.is_empty())
    {
        return rebase(real, &base);
    }
    real.to_string()
}

/// `https://discord.com/api/x?y` on base `http://127.0.0.1:8080` gives
/// `http://127.0.0.1:8080/api/x?y`.
#[cfg(any(feature = "e2e-oauth", test))]
fn rebase(real: &str, base: &str) -> String {
    let rest = real.split_once("://").map_or(real, |(_, r)| r);
    let path = rest.find('/').map_or("", |i| &rest[i..]);
    format!("{}{}", base.trim_end_matches('/'), path)
}

/// Open the system browser. In a test build with `MELLO_E2E_BROWSER_FILE` set,
/// write the URL to that file instead: the e2e driver opens it in a headless
/// browser, so a test never opens the developer's real browser.
fn open_browser(url: &str) -> Result<(), OAuthError> {
    #[cfg(feature = "e2e-oauth")]
    if let Some(path) = std::env::var_os("MELLO_E2E_BROWSER_FILE").filter(|p| !p.is_empty()) {
        return std::fs::write(path, url).map_err(|e| OAuthError::Browser(e.to_string()));
    }
    webbrowser::open(url).map_err(|e| OAuthError::Browser(e.to_string()))
}

/// The callback wait. Test builds can shorten it with
/// `MELLO_E2E_OAUTH_TIMEOUT_MS`, so the "browser closed" case does not take 2 minutes.
fn callback_timeout() -> Duration {
    #[cfg(feature = "e2e-oauth")]
    if let Some(ms) = std::env::var("MELLO_E2E_OAUTH_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        return Duration::from_millis(ms);
    }
    CALLBACK_TIMEOUT
}

/// Decoded `key=value` pairs from a query string or a form body.
type Pairs = Vec<(String, String)>;

/// PKCE challenge pair for OAuth2 Authorization Code flow.
pub struct PkceChallenge {
    pub verifier: String,
    pub challenge: String,
}

impl PkceChallenge {
    pub fn generate() -> Self {
        let verifier: String = rand::thread_rng()
            .sample_iter(&rand::distributions::Alphanumeric)
            .take(64)
            .map(char::from)
            .collect();

        let digest = Sha256::digest(verifier.as_bytes());
        let challenge = URL_SAFE_NO_PAD.encode(digest);

        Self {
            verifier,
            challenge,
        }
    }
}

/// Generate a fresh random `state` for one browser flow.
///
/// The value is alphanumeric, so it needs no URL encoding. The callback server
/// accepts only a callback that returns this exact value.
pub fn generate_state() -> String {
    rand::thread_rng()
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(32)
        .map(char::from)
        .collect()
}

pub enum OAuthMode {
    /// Authorization Code flow — token arrives as `?code=` query param.
    AuthorizationCode,
    /// Implicit flow — token arrives as `#access_token=` fragment (not sent to server).
    Implicit,
    /// Steam OpenID 2.0 — the provider redirects back with the `openid.*` response
    /// in the query string; we capture it verbatim for server-side verification.
    OpenIDQuery,
}

/// Stops a waiting [`OAuthFlow`] from another thread.
///
/// The app uses it when the flow result is no longer wanted, for example at
/// logout. The flow then returns [`OAuthError::Aborted`] at once and releases
/// the callback port. Clones share one flag.
#[derive(Clone, Default)]
pub struct FlowCancel(Arc<CancelState>);

#[derive(Default)]
struct CancelState {
    cancelled: AtomicBool,
    /// The callback server while the flow waits. `cancel` wakes its wait.
    server: Mutex<Option<Arc<Server>>>,
}

impl FlowCancel {
    /// Stop the flow. A flow that has not started yet returns at its start.
    pub fn cancel(&self) {
        // Set the flag before the wake-up: the wait loop reads the flag after
        // each wake-up, so it cannot miss the cancel.
        self.0.cancelled.store(true, Ordering::SeqCst);
        if let Some(server) = self.lock_server().as_ref() {
            server.unblock();
        }
    }

    /// True after [`FlowCancel::cancel`].
    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::SeqCst)
    }

    fn attach(&self, server: Arc<Server>) {
        *self.lock_server() = Some(server);
    }

    /// Drop the reference to the server, so the port closes when the flow ends.
    fn detach(&self) {
        *self.lock_server() = None;
    }

    fn lock_server(&self) -> std::sync::MutexGuard<'_, Option<Arc<Server>>> {
        // The lock guards one assignment. A panic cannot leave it half-written.
        self.0
            .server
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Blocking OAuth flow using a localhost callback server.
/// Must be called from a blocking context (e.g. `tokio::task::spawn_blocking`).
pub struct OAuthFlow;

impl OAuthFlow {
    /// Open the browser at `auth_url` and wait for the provider callback.
    ///
    /// `state` must be the value that `auth_url` carries (for Steam, inside
    /// `return_to`). Use [`generate_state`] to make it. The server rejects a
    /// callback with a missing or different `state`, and a request to any other
    /// path, and continues to wait until the timeout. `cancel` stops the wait.
    pub fn execute(
        auth_url: &str,
        state: &str,
        mode: OAuthMode,
        cancel: &FlowCancel,
    ) -> Result<String, OAuthError> {
        if cancel.is_cancelled() {
            return Err(OAuthError::Aborted);
        }
        let server = Arc::new(
            Server::http(format!("127.0.0.1:{REDIRECT_PORT}"))
                .map_err(|e| OAuthError::ServerStart(e.to_string()))?,
        );

        cancel.attach(Arc::clone(&server));
        let result = open_browser(&provider_url(auth_url)).and_then(|()| {
            log::info!("[oauth] browser opened, waiting for callback");
            Self::wait(&server, state, &mode, callback_timeout(), cancel)
        });
        cancel.detach();
        result
    }

    /// Serve requests until one completes the flow, `timeout` elapses, or
    /// `cancel` stops it. Any local page can reach this server, so a request
    /// that fails the path or `state` check is answered and ignored. It must
    /// not end the flow.
    fn wait(
        server: &Server,
        state: &str,
        mode: &OAuthMode,
        timeout: Duration,
        cancel: &FlowCancel,
    ) -> Result<String, OAuthError> {
        let deadline = Instant::now() + timeout;
        loop {
            if cancel.is_cancelled() {
                log::info!("[oauth] flow stopped by the app");
                return Err(OAuthError::Aborted);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(OAuthError::Timeout);
            }
            // `None` is the timeout or a wake-up from `cancel`. The loop start
            // tells them apart.
            let Some(request) = server
                .recv_timeout(remaining)
                .map_err(|_| OAuthError::Timeout)?
            else {
                continue;
            };

            let outcome = match mode {
                OAuthMode::AuthorizationCode => Self::handle_code(request, state),
                OAuthMode::Implicit => Self::handle_implicit(request, state),
                OAuthMode::OpenIDQuery => Self::handle_openid(request, state),
            };
            if let Some(result) = outcome {
                return result;
            }
        }
    }

    /// Authorization Code: code and `state` are in the callback query string.
    fn handle_code(request: Request, state: &str) -> Option<Result<String, OAuthError>> {
        let (request, _, pairs) = Self::checked_callback(request, state)?;

        // The provider's answer to this flow: a refusal ends the flow at once.
        if let Some(error) = param(&pairs, "error") {
            return Some(Err(Self::answer_error(request, error)));
        }

        match param(&pairs, "code") {
            Some(code) => {
                log::info!("[oauth] authorization code received");
                let code = code.to_string();
                respond_html(request, 200, SUCCESS_HTML);
                Some(Ok(code))
            }
            None => {
                log::warn!("[oauth] callback had a valid state but no code");
                respond_html(request, 400, FAILURE_HTML);
                Some(Err(OAuthError::NoToken))
            }
        }
    }

    /// Steam OpenID: forward the callback query string (the `openid.*`
    /// response) without our `state`, for server-side `check_authentication`.
    fn handle_openid(request: Request, state: &str) -> Option<Result<String, OAuthError>> {
        let (request, raw_query, pairs) = Self::checked_callback(request, state)?;

        // OpenID 2.0 §10.2: a refusal is `openid.mode=cancel`, a provider
        // failure is `openid.mode=error` with `openid.error`.
        match param(&pairs, "openid.mode") {
            Some("cancel") => return Some(Err(Self::answer_error(request, ACCESS_DENIED))),
            Some("error") => {
                let error = param(&pairs, "openid.error").unwrap_or_default();
                return Some(Err(Self::answer_error(request, error)));
            }
            _ => {}
        }

        // Drop only the `state` pair and keep every other pair byte-for-byte, so
        // the signed `openid.*` values reach Steam unchanged.
        let forwarded = raw_query
            .split('&')
            .filter(|pair| !pair.is_empty() && pair.split('=').next() != Some("state"))
            .collect::<Vec<_>>()
            .join("&");

        if forwarded.is_empty() {
            log::warn!("[oauth] callback had a valid state but no OpenID response");
            respond_html(request, 400, FAILURE_HTML);
            return Some(Err(OAuthError::NoToken));
        }

        log::info!("[oauth] OpenID response received");
        respond_html(request, 200, SUCCESS_HTML);
        Some(Ok(forwarded))
    }

    /// Implicit: the token and `state` are in the URL fragment, which the
    /// browser does not send. `GET /callback` serves a page that reads the
    /// fragment and POSTs both to `/token`.
    fn handle_implicit(mut request: Request, state: &str) -> Option<Result<String, OAuthError>> {
        let method = request.method().clone();
        let path = split_target(request.url()).0.to_string();

        match (&method, path.as_str()) {
            // The page holds no secret, so it is safe to serve more than once.
            (Method::Get, CALLBACK_PATH) => {
                log::info!("[oauth] serving fragment extractor page");
                respond_html(request, 200, EXTRACTOR_HTML);
                None
            }
            (Method::Post, TOKEN_PATH) => {
                let mut body = String::new();
                if let Err(e) = request
                    .as_reader()
                    .take(MAX_TOKEN_BODY)
                    .read_to_string(&mut body)
                {
                    log::warn!("[oauth] rejected {TOKEN_PATH}: unreadable body: {e}");
                    respond_text(request, 400, "Bad request");
                    return None;
                }

                let params: Pairs = url::form_urlencoded::parse(body.as_bytes())
                    .into_owned()
                    .collect();
                if !state_matches(param(&params, "state"), state) {
                    log::warn!("[oauth] rejected {TOKEN_PATH}: state missing or wrong");
                    respond_text(request, 400, "Invalid state");
                    return None;
                }

                // The page forwards the fragment's `error`: the provider's
                // answer to this flow. A refusal ends the flow at once.
                if let Some(error) = param(&params, "error") {
                    return Some(Err(Self::answer_error(request, error)));
                }

                match param(&params, "access_token").filter(|t| !t.is_empty()) {
                    Some(token) => {
                        log::info!("[oauth] access token received");
                        let token = token.to_string();
                        respond_text(request, 200, "OK");
                        Some(Ok(token))
                    }
                    None => {
                        log::warn!("[oauth] {TOKEN_PATH} had a valid state but no token");
                        respond_text(request, 400, "No token");
                        Some(Err(OAuthError::NoToken))
                    }
                }
            }
            _ => {
                log::warn!("[oauth] rejected {method} {path}: not a callback");
                respond_text(request, 404, "Not found");
                None
            }
        }
    }

    /// Answer a callback that carries the provider's `error` for this flow,
    /// and return the error that ends the flow. The caller checked `state`.
    ///
    /// The answer is the same for each mode. The implicit-flow page shows its
    /// own text, so the body there is for logs only.
    fn answer_error(request: Request, error: &str) -> OAuthError {
        if error == ACCESS_DENIED {
            log::info!("[oauth] the user refused consent at the provider");
            respond_html(request, 200, CANCELLED_HTML);
            OAuthError::Cancelled
        } else {
            let code = error_code(error);
            log::warn!("[oauth] the provider returned an error: {code}");
            respond_html(request, 400, FAILURE_HTML);
            OAuthError::Provider(code)
        }
    }

    /// Accept only `GET /callback` with the expected `state`. On success return
    /// the unanswered request, the raw query string and the parsed pairs.
    /// Otherwise answer the request with an error status and return `None`, so
    /// the flow continues to wait.
    fn checked_callback(request: Request, state: &str) -> Option<(Request, String, Pairs)> {
        let (path, raw_query) = split_target(request.url());
        let (path, raw_query) = (path.to_string(), raw_query.to_string());

        if *request.method() != Method::Get || path != CALLBACK_PATH {
            log::warn!(
                "[oauth] rejected {} {path}: not a callback",
                request.method()
            );
            respond_text(request, 404, "Not found");
            return None;
        }

        let pairs: Pairs = url::form_urlencoded::parse(raw_query.as_bytes())
            .into_owned()
            .collect();
        if !state_matches(param(&pairs, "state"), state) {
            log::warn!("[oauth] rejected {CALLBACK_PATH}: state missing or wrong");
            respond_text(request, 400, "Invalid state");
            return None;
        }

        Some((request, raw_query, pairs))
    }
}

/// Split an HTTP request target into path and raw query. The fragment never
/// reaches the server, so there is none to strip.
fn split_target(target: &str) -> (&str, &str) {
    target.split_once('?').unwrap_or((target, ""))
}

fn param<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// A provider error code that is safe to log and show: printable ASCII only,
/// at most [`MAX_ERROR_CODE`] characters.
fn error_code(raw: &str) -> String {
    let code: String = raw
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .take(MAX_ERROR_CODE)
        .collect();
    let code = code.trim();
    if code.is_empty() {
        "unknown_error".to_string()
    } else {
        code.to_string()
    }
}

/// Compare in constant time, so response timing does not show how much of a
/// guessed `state` is correct. An empty expected value never matches.
fn state_matches(received: Option<&str>, expected: &str) -> bool {
    let Some(received) = received else {
        return false;
    };
    !expected.is_empty()
        && received.len() == expected.len()
        && received
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

fn respond_html(request: Request, status: u16, body: &str) {
    respond(request, status, "text/html; charset=utf-8", body);
}

fn respond_text(request: Request, status: u16, body: &str) {
    respond(request, status, "text/plain; charset=utf-8", body);
}

fn respond(request: Request, status: u16, content_type: &str, body: &str) {
    let header = Header::from_bytes("Content-Type", content_type)
        .expect("static Content-Type header is valid");
    let response = Response::from_string(body)
        .with_status_code(status)
        .with_header(header);
    let _ = request.respond(response);
}

/// Reads `access_token` (or `error`) and `state` from the fragment and POSTs
/// them to `/token`. Text goes in via `textContent`: the fragment is untrusted
/// input.
const EXTRACTOR_HTML: &str = r#"<!DOCTYPE html>
<html>
<head><title>Mello - Authenticating</title></head>
<body style="font-family: system-ui; display: flex; justify-content: center;
             align-items: center; height: 100vh; margin: 0;
             background: #1a1a1a; color: white;">
    <div id="status">
        <h1>Authenticating...</h1>
        <p>Please wait while we complete sign-in.</p>
    </div>
    <script>
        const params = new URLSearchParams(window.location.hash.substring(1));
        const token = params.get('access_token');
        const state = params.get('state');
        const error = params.get('error');
        const status = document.getElementById('status');

        function show(title, text) {
            const h = document.createElement('h1');
            h.textContent = title;
            const p = document.createElement('p');
            p.textContent = text;
            status.replaceChildren(h, p);
        }

        if (error) {
            // Tell the app, so the flow ends now and not at the timeout. The
            // server checks `state` and ignores a report without it.
            if (state) {
                fetch('/token', {
                    method: 'POST',
                    body: new URLSearchParams({ error: error, state: state }),
                }).catch(() => {});
            }
            if (error === 'access_denied') {
                show('Sign-in Cancelled', 'You can close this tab and return to Mello.');
            } else {
                show('Authentication Failed', error);
            }
        } else if (token && state) {
            fetch('/token', {
                method: 'POST',
                body: new URLSearchParams({ access_token: token, state: state }),
            }).then((resp) => {
                if (resp.ok) {
                    show('Success!', 'You can close this tab and return to Mello.');
                } else {
                    show('Authentication Failed', 'Please try again from Mello.');
                }
            }).catch(() => {
                show('Authentication Failed', 'Please try again from Mello.');
            });
        } else {
            show('No Token', 'Authentication failed. Please try again.');
        }
    </script>
</body>
</html>"#;

const SUCCESS_HTML: &str = r#"<!DOCTYPE html>
<html>
<head><title>Mello</title></head>
<body style="font-family: system-ui; display: flex; justify-content: center;
             align-items: center; height: 100vh; margin: 0;
             background: #1a1a1a; color: white;">
    <div><h1>Success!</h1><p>You can close this tab and return to Mello.</p></div>
</body>
</html>"#;

const FAILURE_HTML: &str = r#"<!DOCTYPE html>
<html>
<head><title>Mello</title></head>
<body style="font-family: system-ui; display: flex; justify-content: center;
             align-items: center; height: 100vh; margin: 0;
             background: #1a1a1a; color: white;">
    <div><h1>Authentication Failed</h1><p>Please try again from Mello.</p></div>
</body>
</html>"#;

const CANCELLED_HTML: &str = r#"<!DOCTYPE html>
<html>
<head><title>Mello</title></head>
<body style="font-family: system-ui; display: flex; justify-content: center;
             align-items: center; height: 100vh; margin: 0;
             background: #1a1a1a; color: white;">
    <div><h1>Sign-in Cancelled</h1><p>You can close this tab and return to Mello.</p></div>
</body>
</html>"#;

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("Failed to start callback server: {0}")]
    ServerStart(String),

    #[error("Failed to open browser: {0}")]
    Browser(String),

    #[error("Timeout waiting for authentication")]
    Timeout,

    #[error("No token/code received")]
    NoToken,

    /// The user refused consent at the provider (`access_denied`, or Steam
    /// `openid.mode=cancel`).
    #[error("The sign-in was cancelled")]
    Cancelled,

    /// The provider answered this flow with an error other than a refusal.
    #[error("The provider returned an error: {0}")]
    Provider(String),

    /// The app stopped the flow with [`FlowCancel`].
    #[error("The sign-in was stopped")]
    Aborted,

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    #[test]
    fn rebase_keeps_path_and_query_and_swaps_the_host() {
        assert_eq!(
            rebase(
                "https://discord.com/api/oauth2/authorize?client_id=c&state=s",
                "http://127.0.0.1:8080/"
            ),
            "http://127.0.0.1:8080/api/oauth2/authorize?client_id=c&state=s"
        );
        assert_eq!(
            rebase("https://oauth2.googleapis.com/token", "http://h:1"),
            "http://h:1/token"
        );
    }

    #[test]
    fn provider_url_is_unchanged_without_the_override() {
        // No e2e override in unit tests: production behaviour.
        let url = "https://steamcommunity.com/openid/login?openid.mode=checkid_setup";
        if std::env::var("MELLO_E2E_OAUTH_BASE").is_err() {
            assert_eq!(provider_url(url), url);
        }
    }

    use super::*;
    use std::io::Write;
    use std::net::TcpStream;

    const STATE: &str = "Q7fZkP2xLm9RtV4cWn8Hs3JdYb6GaE1u";

    /// Run `wait` on an ephemeral port in a thread, drive it from `client`, and
    /// return the flow result with whatever `client` returned. Every request
    /// reads its response before the next is sent, so the order is fixed.
    fn run_flow<T>(
        mode: OAuthMode,
        client: impl FnOnce(u16) -> T,
    ) -> (Result<String, OAuthError>, T) {
        let server = Server::http("127.0.0.1:0").expect("bind ephemeral port");
        let port = server.server_addr().to_ip().expect("ip listener").port();
        let flow = std::thread::spawn(move || {
            OAuthFlow::wait(
                &server,
                STATE,
                &mode,
                Duration::from_secs(30),
                &FlowCancel::default(),
            )
        });
        let out = client(port);
        (flow.join().expect("flow thread"), out)
    }

    /// Send one request and return the response status code, or 0 if the
    /// server is gone (the flow already returned).
    fn send(port: u16, method: &str, target: &str, body: &str) -> u16 {
        let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
            return 0;
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("set read timeout");
        let request = format!(
            "{method} {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
             Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        if stream.write_all(request.as_bytes()).is_err() {
            return 0;
        }
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        response
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or(0)
    }

    fn get(port: u16, target: &str) -> u16 {
        send(port, "GET", target, "")
    }

    fn callback(query: &str) -> String {
        format!("{CALLBACK_PATH}?{query}")
    }

    #[test]
    fn state_is_random_alphanumeric() {
        let a = generate_state();
        let b = generate_state();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(a, b);
    }

    #[test]
    fn state_match_needs_the_exact_value() {
        assert!(state_matches(Some(STATE), STATE));
        assert!(!state_matches(None, STATE));
        assert!(!state_matches(Some(""), STATE));
        assert!(!state_matches(Some(&STATE[1..]), STATE));
        assert!(!state_matches(Some(&STATE.to_lowercase()), STATE));
        assert!(
            !state_matches(Some(""), ""),
            "empty expected state must never match"
        );
    }

    #[test]
    fn redirect_uri_points_at_callback_path() {
        assert!(REDIRECT_URI.ends_with(CALLBACK_PATH));
    }

    #[test]
    fn code_flow_accepts_callback_with_matching_state() {
        let (result, status) = run_flow(OAuthMode::AuthorizationCode, |port| {
            get(port, &callback(&format!("code=good&state={STATE}")))
        });
        assert_eq!(result.unwrap(), "good");
        assert_eq!(status, 200);
    }

    #[test]
    fn code_flow_rejects_missing_or_wrong_state_and_keeps_waiting() {
        let (result, statuses) = run_flow(OAuthMode::AuthorizationCode, |port| {
            vec![
                get(port, &callback("code=evil")),
                get(port, &callback("code=evil&state=wrong")),
                get(port, &callback(&format!("code=good&state={STATE}"))),
            ]
        });
        assert_eq!(result.unwrap(), "good");
        assert_eq!(statuses, vec![400, 400, 200]);
    }

    #[test]
    fn code_flow_rejects_other_paths_and_keeps_waiting() {
        let (result, statuses) = run_flow(OAuthMode::AuthorizationCode, |port| {
            vec![
                get(port, "/favicon.ico"),
                get(port, &format!("/?code=evil&state={STATE}")),
                send(
                    port,
                    "POST",
                    &callback(&format!("code=evil&state={STATE}")),
                    "",
                ),
                get(port, &callback(&format!("code=good&state={STATE}"))),
            ]
        });
        assert_eq!(result.unwrap(), "good");
        assert_eq!(statuses, vec![404, 404, 404, 200]);
    }

    #[test]
    fn code_flow_refusal_with_matching_state_ends_the_flow_as_cancelled() {
        let (result, status) = run_flow(OAuthMode::AuthorizationCode, |port| {
            get(
                port,
                &callback(&format!("error=access_denied&state={STATE}")),
            )
        });
        assert!(matches!(result, Err(OAuthError::Cancelled)), "{result:?}");
        assert_eq!(status, 200);
    }

    #[test]
    fn code_flow_other_provider_error_ends_the_flow_with_the_code() {
        let (result, status) = run_flow(OAuthMode::AuthorizationCode, |port| {
            get(
                port,
                &callback(&format!("error=server_error&state={STATE}")),
            )
        });
        assert!(
            matches!(&result, Err(OAuthError::Provider(code)) if code == "server_error"),
            "{result:?}"
        );
        assert_eq!(status, 400);
    }

    #[test]
    fn code_flow_callback_without_code_or_error_has_no_token() {
        let (result, status) = run_flow(OAuthMode::AuthorizationCode, |port| {
            get(port, &callback(&format!("state={STATE}")))
        });
        assert!(matches!(result, Err(OAuthError::NoToken)), "{result:?}");
        assert_eq!(status, 400);
    }

    #[test]
    fn code_flow_ignores_a_refusal_with_missing_or_wrong_state() {
        let (result, statuses) = run_flow(OAuthMode::AuthorizationCode, |port| {
            vec![
                get(port, &callback("error=access_denied")),
                get(port, &callback("error=access_denied&state=wrong")),
                get(port, &callback(&format!("code=good&state={STATE}"))),
            ]
        });
        assert_eq!(result.unwrap(), "good");
        assert_eq!(statuses, vec![400, 400, 200]);
    }

    #[test]
    fn implicit_flow_accepts_token_post_with_matching_state() {
        let (result, statuses) = run_flow(OAuthMode::Implicit, |port| {
            vec![
                get(port, CALLBACK_PATH),
                send(
                    port,
                    "POST",
                    TOKEN_PATH,
                    &format!("access_token=good&state={STATE}"),
                ),
            ]
        });
        assert_eq!(result.unwrap(), "good");
        assert_eq!(statuses, vec![200, 200]);
    }

    #[test]
    fn implicit_flow_rejects_token_post_with_missing_or_wrong_state() {
        let (result, statuses) = run_flow(OAuthMode::Implicit, |port| {
            vec![
                // A local page can skip the extractor page and POST directly.
                send(port, "POST", TOKEN_PATH, "evil"),
                send(port, "POST", TOKEN_PATH, "access_token=evil"),
                send(port, "POST", TOKEN_PATH, "access_token=evil&state=wrong"),
                send(
                    port,
                    "POST",
                    TOKEN_PATH,
                    &format!("access_token=good&state={STATE}"),
                ),
            ]
        });
        assert_eq!(result.unwrap(), "good");
        assert_eq!(statuses, vec![400, 400, 400, 200]);
    }

    #[test]
    fn implicit_flow_ignores_other_requests_between_page_and_token() {
        let (result, statuses) = run_flow(OAuthMode::Implicit, |port| {
            vec![
                get(port, CALLBACK_PATH),
                // Browsers fetch a favicon after the page loads.
                get(port, "/favicon.ico"),
                get(
                    port,
                    &format!("{TOKEN_PATH}?access_token=evil&state={STATE}"),
                ),
                get(port, CALLBACK_PATH),
                send(
                    port,
                    "POST",
                    TOKEN_PATH,
                    &format!("access_token=good&state={STATE}"),
                ),
            ]
        });
        assert_eq!(result.unwrap(), "good");
        assert_eq!(statuses, vec![200, 404, 404, 200, 200]);
    }

    #[test]
    fn implicit_flow_refusal_with_matching_state_ends_the_flow_as_cancelled() {
        // #87: the extractor page forwards the fragment's `error`. Before the
        // fix, the flow answered "No token" and waited for the timeout.
        let (result, statuses) = run_flow(OAuthMode::Implicit, |port| {
            vec![
                get(port, CALLBACK_PATH),
                send(
                    port,
                    "POST",
                    TOKEN_PATH,
                    &format!("error=access_denied&state={STATE}"),
                ),
            ]
        });
        assert!(matches!(result, Err(OAuthError::Cancelled)), "{result:?}");
        assert_eq!(statuses, vec![200, 200]);
    }

    #[test]
    fn implicit_flow_other_provider_error_ends_the_flow_with_the_code() {
        let (result, status) = run_flow(OAuthMode::Implicit, |port| {
            send(
                port,
                "POST",
                TOKEN_PATH,
                &format!("error=temporarily_unavailable&state={STATE}"),
            )
        });
        assert!(
            matches!(&result, Err(OAuthError::Provider(code)) if code == "temporarily_unavailable"),
            "{result:?}"
        );
        assert_eq!(status, 400);
    }

    #[test]
    fn implicit_flow_ignores_a_refusal_with_missing_or_wrong_state() {
        // PR #80: only this flow's `state` can end it, also with an error.
        let (result, statuses) = run_flow(OAuthMode::Implicit, |port| {
            vec![
                send(port, "POST", TOKEN_PATH, "error=access_denied"),
                send(port, "POST", TOKEN_PATH, "error=access_denied&state=wrong"),
                send(
                    port,
                    "POST",
                    TOKEN_PATH,
                    &format!("access_token=good&state={STATE}"),
                ),
            ]
        });
        assert_eq!(result.unwrap(), "good");
        assert_eq!(statuses, vec![400, 400, 200]);
    }

    #[test]
    fn extractor_page_reports_a_fragment_error_with_state() {
        // The page is the only way the fragment's `error` reaches the server.
        assert!(EXTRACTOR_HTML.contains("new URLSearchParams({ error: error, state: state })"));
    }

    #[test]
    fn openid_flow_cancel_with_matching_state_ends_the_flow_as_cancelled() {
        let (result, status) = run_flow(OAuthMode::OpenIDQuery, |port| {
            get(
                port,
                &callback(&format!(
                    "state={STATE}&openid.ns=http%3A%2F%2Fspecs.openid.net%2Fauth%2F2.0&openid.mode=cancel"
                )),
            )
        });
        assert!(matches!(result, Err(OAuthError::Cancelled)), "{result:?}");
        assert_eq!(status, 200);
    }

    #[test]
    fn openid_flow_error_mode_ends_the_flow_with_the_provider_error() {
        let (result, status) = run_flow(OAuthMode::OpenIDQuery, |port| {
            get(
                port,
                &callback(&format!(
                    "state={STATE}&openid.mode=error&openid.error=Bad%20realm%0A"
                )),
            )
        });
        assert!(
            matches!(&result, Err(OAuthError::Provider(code)) if code == "Bad realm"),
            "{result:?}"
        );
        assert_eq!(status, 400);
    }

    #[test]
    fn openid_flow_ignores_a_cancel_with_missing_or_wrong_state() {
        let (result, statuses) = run_flow(OAuthMode::OpenIDQuery, |port| {
            vec![
                get(port, &callback("openid.mode=cancel")),
                get(port, &callback("state=wrong&openid.mode=cancel")),
                get(
                    port,
                    &callback(&format!("state={STATE}&openid.mode=id_res")),
                ),
            ]
        });
        assert_eq!(result.unwrap(), "openid.mode=id_res");
        assert_eq!(statuses, vec![400, 400, 200]);
    }

    #[test]
    fn error_code_keeps_printable_ascii_and_bounds_the_length() {
        assert_eq!(error_code("access_denied"), "access_denied");
        assert_eq!(error_code(" a\u{7}b\n "), "ab");
        assert_eq!(error_code("\u{e9}\u{7}"), "unknown_error");
        assert_eq!(error_code(&"x".repeat(500)).len(), MAX_ERROR_CODE);
    }

    #[test]
    fn cancel_wakes_a_receive_on_the_attached_server() {
        // The wait loop blocks in `recv_timeout`. `cancel` must wake it, not
        // only set the flag that the loop reads at the next request.
        let server = Arc::new(Server::http("127.0.0.1:0").expect("bind ephemeral port"));
        let cancel = FlowCancel::default();
        cancel.attach(Arc::clone(&server));
        cancel.cancel();

        let started = Instant::now();
        let request = server
            .recv_timeout(Duration::from_secs(20))
            .expect("no receive error");
        assert!(request.is_none());
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the receive waited for its timeout"
        );
    }

    #[test]
    fn detach_releases_the_server() {
        let server = Arc::new(Server::http("127.0.0.1:0").expect("bind ephemeral port"));
        let cancel = FlowCancel::default();
        cancel.attach(Arc::clone(&server));
        cancel.detach();
        // Only the flow holds the server now, so the port closes when it ends.
        assert_eq!(Arc::strong_count(&server), 1);
    }

    #[test]
    fn cancel_ends_a_waiting_flow_at_once() {
        let server = Arc::new(Server::http("127.0.0.1:0").expect("bind ephemeral port"));
        let cancel = FlowCancel::default();
        cancel.attach(Arc::clone(&server));
        let flow = {
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                let started = Instant::now();
                let result = OAuthFlow::wait(
                    &server,
                    STATE,
                    &OAuthMode::Implicit,
                    Duration::from_secs(60),
                    &cancel,
                );
                (result, started.elapsed())
            })
        };
        cancel.cancel();
        let (result, elapsed) = flow.join().expect("flow thread");
        assert!(matches!(result, Err(OAuthError::Aborted)), "{result:?}");
        // The wait is 60 s. A cancel that took effect only at the timeout
        // fails here.
        assert!(elapsed < Duration::from_secs(30), "took {elapsed:?}");
    }

    #[test]
    fn openid_flow_accepts_matching_state_and_forwards_only_openid_pairs() {
        let (result, status) = run_flow(OAuthMode::OpenIDQuery, |port| {
            get(
                port,
                &callback(&format!(
                    "state={STATE}&openid.ns=http%3A%2F%2Fspecs&openid.mode=id_res&openid.sig=a%2Bb%3D"
                )),
            )
        });
        assert_eq!(
            result.unwrap(),
            "openid.ns=http%3A%2F%2Fspecs&openid.mode=id_res&openid.sig=a%2Bb%3D"
        );
        assert_eq!(status, 200);
    }

    #[test]
    fn openid_flow_rejects_missing_or_wrong_state_and_other_paths() {
        let (result, statuses) = run_flow(OAuthMode::OpenIDQuery, |port| {
            vec![
                get(port, &callback("openid.mode=id_res&openid.sig=evil")),
                get(
                    port,
                    &callback("state=wrong&openid.mode=id_res&openid.sig=evil"),
                ),
                get(port, &format!("/other?state={STATE}&openid.sig=evil")),
                get(
                    port,
                    &callback(&format!("state={STATE}&openid.mode=id_res")),
                ),
            ]
        });
        assert_eq!(result.unwrap(), "openid.mode=id_res");
        assert_eq!(statuses, vec![400, 400, 404, 200]);
    }

    #[test]
    fn openid_flow_with_only_state_has_no_response() {
        let (result, status) = run_flow(OAuthMode::OpenIDQuery, |port| {
            get(port, &callback(&format!("state={STATE}")))
        });
        assert!(matches!(result, Err(OAuthError::NoToken)));
        assert_eq!(status, 400);
    }

    #[test]
    fn flow_times_out_when_no_request_arrives() {
        let server = Server::http("127.0.0.1:0").expect("bind ephemeral port");
        let result = OAuthFlow::wait(
            &server,
            STATE,
            &OAuthMode::AuthorizationCode,
            Duration::from_millis(50),
            &FlowCancel::default(),
        );
        assert!(matches!(result, Err(OAuthError::Timeout)));
    }
}
