use std::time::Duration;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::Rng;
use sha2::{Digest, Sha256};
use tiny_http::{Header, Response, Server};

const REDIRECT_PORT: u16 = 29405;
pub const REDIRECT_URI: &str = "http://localhost:29405/callback";

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

pub enum OAuthMode {
    /// Authorization Code flow — token arrives as `?code=` query param.
    AuthorizationCode,
    /// Implicit flow — token arrives as `#access_token=` fragment (not sent to server).
    Implicit,
}

/// Blocking OAuth flow using a localhost callback server.
/// Must be called from a blocking context (e.g. `tokio::task::spawn_blocking`).
pub struct OAuthFlow;

impl OAuthFlow {
    pub fn execute(auth_url: &str, mode: OAuthMode) -> Result<String, OAuthError> {
        let server = Server::http(format!("127.0.0.1:{REDIRECT_PORT}"))
            .map_err(|e| OAuthError::ServerStart(e.to_string()))?;

        webbrowser::open(auth_url).map_err(|e| OAuthError::Browser(e.to_string()))?;

        match mode {
            OAuthMode::AuthorizationCode => Self::wait_for_code(&server),
            OAuthMode::Implicit => Self::wait_for_fragment(&server),
        }
    }

    /// Authorization Code: code is in the query string.
    fn wait_for_code(server: &Server) -> Result<String, OAuthError> {
        let request = server
            .recv_timeout(Duration::from_secs(120))
            .map_err(|_| OAuthError::Timeout)?
            .ok_or(OAuthError::Timeout)?;

        let raw_url = request.url().to_string();
        let code = url::Url::parse(&format!("http://localhost{raw_url}"))
            .ok()
            .and_then(|u| {
                u.query_pairs()
                    .find(|(k, _)| k == "code")
                    .map(|(_, v)| v.to_string())
            })
            .ok_or(OAuthError::NoToken)?;

        let html = Self::success_html();
        let response = Response::from_string(html)
            .with_header(Header::from_bytes("Content-Type", "text/html").unwrap());
        let _ = request.respond(response);

        Ok(code)
    }

    /// Implicit: token is in the URL fragment (not sent to server).
    /// Serve JS that extracts it and POSTs it back.
    fn wait_for_fragment(server: &Server) -> Result<String, OAuthError> {
        let request = server
            .recv_timeout(Duration::from_secs(120))
            .map_err(|_| OAuthError::Timeout)?
            .ok_or(OAuthError::Timeout)?;

        let extractor_html = r#"<!DOCTYPE html>
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
        const fragment = window.location.hash.substring(1);
        const params = new URLSearchParams(fragment);
        const token = params.get('access_token');
        const error = params.get('error');

        if (error) {
            document.getElementById('status').innerHTML =
                '<h1>Authentication Failed</h1><p>' + error + '</p>';
        } else if (token) {
            fetch('/token', { method: 'POST', body: token }).then(() => {
                document.getElementById('status').innerHTML =
                    '<h1>Success!</h1><p>You can close this tab and return to Mello.</p>';
            });
        } else {
            document.getElementById('status').innerHTML =
                '<h1>No Token</h1><p>Authentication failed. Please try again.</p>';
        }
    </script>
</body>
</html>"#;

        let response = Response::from_string(extractor_html)
            .with_header(Header::from_bytes("Content-Type", "text/html").unwrap());
        let _ = request.respond(response);

        let mut token_request = server
            .recv_timeout(Duration::from_secs(30))
            .map_err(|_| OAuthError::Timeout)?
            .ok_or(OAuthError::Timeout)?;

        let mut body = String::new();
        std::io::Read::read_to_string(&mut token_request.as_reader(), &mut body)?;

        if body.is_empty() {
            return Err(OAuthError::NoToken);
        }

        let _ = token_request.respond(Response::from_string("OK"));
        Ok(body)
    }

    fn success_html() -> &'static str {
        r#"<!DOCTYPE html>
<html>
<head><title>Mello</title></head>
<body style="font-family: system-ui; display: flex; justify-content: center;
             align-items: center; height: 100vh; margin: 0;
             background: #1a1a1a; color: white;">
    <div><h1>Success!</h1><p>You can close this tab and return to Mello.</p></div>
</body>
</html>"#
    }
}

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
