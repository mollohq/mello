use std::io::Read;
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

/// Blocking OAuth flow using a localhost callback server.
/// Must be called from a blocking context (e.g. `tokio::task::spawn_blocking`).
pub struct OAuthFlow;

impl OAuthFlow {
    /// Open the browser at `auth_url` and wait for the provider callback.
    ///
    /// `state` must be the value that `auth_url` carries (for Steam, inside
    /// `return_to`). Use [`generate_state`] to make it. The server rejects a
    /// callback with a missing or different `state`, and a request to any other
    /// path, and continues to wait until the timeout.
    pub fn execute(auth_url: &str, state: &str, mode: OAuthMode) -> Result<String, OAuthError> {
        let server = Server::http(format!("127.0.0.1:{REDIRECT_PORT}"))
            .map_err(|e| OAuthError::ServerStart(e.to_string()))?;

        webbrowser::open(auth_url).map_err(|e| OAuthError::Browser(e.to_string()))?;
        log::info!("[oauth] browser opened, waiting for callback");

        Self::wait(&server, state, &mode, CALLBACK_TIMEOUT)
    }

    /// Serve requests until one completes the flow or `timeout` elapses. Any
    /// local page can reach this server, so a request that fails the path or
    /// `state` check is answered and ignored. It must not end the flow.
    fn wait(
        server: &Server,
        state: &str,
        mode: &OAuthMode,
        timeout: Duration,
    ) -> Result<String, OAuthError> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let request = server
                .recv_timeout(remaining)
                .map_err(|_| OAuthError::Timeout)?
                .ok_or(OAuthError::Timeout)?;

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
        let (request, raw_query, _) = Self::checked_callback(request, state)?;

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

/// Reads `access_token` and `state` from the fragment and POSTs both to
/// `/token`. Text goes in via `textContent`: the fragment is untrusted input.
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
            show('Authentication Failed', error);
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

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
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
            OAuthFlow::wait(&server, STATE, &mode, Duration::from_secs(30))
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
    fn code_flow_provider_error_with_matching_state_ends_the_flow() {
        let (result, status) = run_flow(OAuthMode::AuthorizationCode, |port| {
            get(
                port,
                &callback(&format!("error=access_denied&state={STATE}")),
            )
        });
        assert!(matches!(result, Err(OAuthError::NoToken)));
        assert_eq!(status, 400);
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
        );
        assert!(matches!(result, Err(OAuthError::Timeout)));
    }
}
