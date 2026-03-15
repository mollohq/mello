# MELLO Social Login Specification

> **Component:** Authentication (Social Login)  
> **Version:** 0.3  
> **Status:** Planned  
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)  
> **Setup Guide:** [06a-SOCIAL-LOGIN-SETUP.md](./06a-SOCIAL-LOGIN-SETUP.md)

---

## 1. Overview

Mello supports six authentication methods. Social logins are **cloud-only**; self-hosted deployments expose email/password only.

| Method | Priority | Use Case | Cloud | Self-hosted |
|--------|----------|----------|:-----:|:-----------:|
| **Steam** | P0 | Core gamer identity | Y | — |
| **Google** | P0 | Universal, lowest friction | Y | — |
| **Twitch** | P0 | Streaming audience | Y | — |
| **Discord** | P1 | Competitor — support but don't promote | Y | — |
| **Apple** | P1 | Future App Store requirement | Y | — |
| **Email/Password** | P1 | Fallback | Y | Y |

All auth flows terminate at Nakama, which handles token validation and session management.

| Provider | Auth Mechanism | Nakama Endpoint |
|----------|----------------|-----------------|
| Steam | Steamworks SDK session ticket | `/v2/account/authenticate/steam` (native) |
| Google | OAuth2 Authorization Code + PKCE | `/v2/account/authenticate/google` (native) |
| Twitch | OAuth2 Authorization Code + PKCE | `/v2/account/authenticate/custom` + before-hook |
| Discord | OAuth2 implicit flow | `/v2/account/authenticate/custom` + before-hook |
| Apple | Sign in with Apple (browser) | `/v2/account/authenticate/apple` (native) |
| Email/Password | Direct API call | `/v2/account/authenticate/email` (native) |

---

## 2. Architecture

```
┌───────────────────────────────────────────────────────────────────────────────┐
│                              CLIENT (mello)                                   │
│                                                                               │
│  P0 ─────────────────────────────────────────────────────────────────         │
│  ┌──────────┐   ┌──────────┐   ┌──────────┐                                  │
│  │  Steam   │   │  Google  │   │  Twitch  │                                  │
│  │  Button  │   │  Button  │   │  Button  │                                  │
│  └────┬─────┘   └────┬─────┘   └────┬─────┘                                  │
│       │              │              │                                         │
│       ▼              ▼              ▼                                         │
│  Steamworks     OAuth2 PKCE    OAuth2 PKCE                                   │
│  SDK            (browser)      (browser)                                     │
│       │              │              │                                         │
│  P1 ─────────────────────────────────────────────────────────────────         │
│  ┌──────────┐   ┌──────────┐   ┌──────────┐                                  │
│  │ Discord  │   │  Apple   │   │  Email   │                                  │
│  │  Button  │   │  Button  │   │  Form    │                                  │
│  └────┬─────┘   └────┬─────┘   └────┬─────┘                                  │
│       │              │              │                                         │
│       ▼              ▼              ▼                                         │
│  OAuth2 Implicit  Apple JS     Direct API                                    │
│  (browser)        (browser)    Call                                           │
│       │              │              │                                         │
│       └──────────────┴──────┬───────┘                                         │
│                             │                                                 │
│                             ▼                                                 │
│                   ┌───────────────────┐                                       │
│                   │   mello-core      │                                       │
│                   │   AuthManager     │                                       │
│                   └─────────┬─────────┘                                       │
│                             │                                                 │
└─────────────────────────────┼─────────────────────────────────────────────────┘
                              │ HTTPS
                              ▼
┌───────────────────────────────────────────────────────────────────────────────┐
│                              NAKAMA                                            │
│                                                                               │
│   POST /v2/account/authenticate/steam    (Steam — native)                     │
│   POST /v2/account/authenticate/google   (Google — native)                    │
│   POST /v2/account/authenticate/custom   (Twitch/Discord — before-hook)       │
│   POST /v2/account/authenticate/apple    (Apple — native)                     │
│   POST /v2/account/authenticate/email    (Email — native)                     │
│                                                                               │
│   Nakama validates tokens with provider APIs                                  │
│   Returns: session token + refresh token                                      │
│                                                                               │
└───────────────────────────────────────────────────────────────────────────────┘
```

---

## 3. Self-hosted vs Cloud

Social login requires OAuth client IDs and secrets that are configured by Mello's cloud deployment. Self-hosted instances don't have these credentials.

**Discovery mechanism:** On startup the client calls a lightweight RPC to find out which providers the backend supports.

```go
// backend/nakama/data/modules/auth.go

func AuthProvidersRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
    providers := []string{"email"}

    if os.Getenv("STEAM_PUBLISHER_KEY") != "" {
        providers = append(providers, "steam")
    }
    if os.Getenv("GOOGLE_CLIENT_ID") != "" {
        providers = append(providers, "google")
    }
    if os.Getenv("TWITCH_CLIENT_ID") != "" {
        providers = append(providers, "twitch")
    }
    if os.Getenv("DISCORD_CLIENT_ID") != "" {
        providers = append(providers, "discord")
    }
    if os.Getenv("APPLE_CLIENT_ID") != "" {
        providers = append(providers, "apple")
    }

    resp, _ := json.Marshal(map[string]interface{}{
        "providers": providers,
    })
    return string(resp), nil
}
```

```rust
// mello-core: call on startup, before showing login UI
let providers: Vec<String> = nakama.rpc("auth/providers", "{}").await?;
```

The login screen adapts: if only `["email"]` is returned, show the email/password form directly without any social buttons.

---

## 4. Login Screen UI

### 4.1 Cloud (all providers)

```
┌─────────────────────────────────────────────────────────────┐
│                                                             │
│                          MELLO                              │
│                                                             │
│                 Hang out with your crew.                    │
│                Jump into anyone's stream.                   │
│                                                             │
│    ┌─────────────────────────────────────────────────┐      │
│    │  🎮  Continue with Steam                        │      │
│    └─────────────────────────────────────────────────┘      │
│    ┌─────────────────────────────────────────────────┐      │
│    │  G   Continue with Google                       │      │
│    └─────────────────────────────────────────────────┘      │
│    ┌─────────────────────────────────────────────────┐      │
│    │  📺  Continue with Twitch                       │      │
│    └─────────────────────────────────────────────────┘      │
│                                                             │
│    ┌───────────────────┐    ┌───────────────────┐           │
│    │  💬  Discord       │    │  🍎  Apple        │           │
│    └───────────────────┘    └───────────────────┘           │
│                                                             │
│    ───────────────────── or ─────────────────────────       │
│                                                             │
│    ┌─────────────────────────────────────────────────┐      │
│    │  Email                                          │      │
│    └─────────────────────────────────────────────────┘      │
│    ┌─────────────────────────────────────────────────┐      │
│    │  Password                                       │      │
│    └─────────────────────────────────────────────────┘      │
│    ┌─────────────────────────────────────────────────┐      │
│    │                    Sign In                      │      │
│    └─────────────────────────────────────────────────┘      │
│                                                             │
│              Don't have an account? Sign up                 │
│                                                             │
│    ─────────────────────────────────────────────────        │
│    By signing in, you agree to our Terms of Service         │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

- **P0 buttons** (Steam, Google, Twitch) are full-width, prominent.
- **P1 buttons** (Discord, Apple) are half-width, secondary row.
- **Email/Password** is below the divider.

### 4.2 Self-hosted (email only)

```
┌─────────────────────────────────────────────────────────────┐
│                                                             │
│                          MELLO                              │
│                                                             │
│    ┌─────────────────────────────────────────────────┐      │
│    │  Email                                          │      │
│    └─────────────────────────────────────────────────┘      │
│    ┌─────────────────────────────────────────────────┐      │
│    │  Password                                       │      │
│    └─────────────────────────────────────────────────┘      │
│    ┌─────────────────────────────────────────────────┐      │
│    │                    Sign In                      │      │
│    └─────────────────────────────────────────────────┘      │
│                                                             │
│              Don't have an account? Sign up                 │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

**Note:** Sessions are automatically persisted to OS secure storage. No "Remember me" checkbox needed.

---

## 5. Shared OAuth Flow

Google, Twitch, and Discord all use a localhost callback server to receive tokens from the browser. This is extracted into a reusable `OAuthFlow` struct.

```rust
// client/src/auth/oauth.rs

use tiny_http::{Server, Response, Header};
use std::time::Duration;
use rand::Rng;
use sha2::{Sha256, Digest};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

const REDIRECT_PORT: u16 = 29405;
const REDIRECT_URI: &str = "http://localhost:29405/callback";

/// PKCE challenge pair
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

        Self { verifier, challenge }
    }
}

pub enum OAuthMode {
    /// Authorization Code flow — token arrives as ?code= query param
    AuthorizationCode,
    /// Implicit flow — token arrives as #access_token= fragment
    Implicit,
}

pub struct OAuthFlow;

impl OAuthFlow {
    /// Opens the browser to `auth_url` and waits for the callback.
    /// Returns the authorization code (AuthorizationCode) or access token (Implicit).
    pub fn execute(auth_url: &str, mode: OAuthMode) -> Result<String, OAuthError> {
        let server = Server::http(format!("127.0.0.1:{}", REDIRECT_PORT))
            .map_err(|e| OAuthError::ServerStart(e.to_string()))?;

        webbrowser::open(auth_url)?;

        match mode {
            OAuthMode::AuthorizationCode => Self::wait_for_code(&server),
            OAuthMode::Implicit => Self::wait_for_fragment(&server),
        }
    }

    /// Authorization Code: code is in the query string, server reads it directly.
    fn wait_for_code(server: &Server) -> Result<String, OAuthError> {
        let request = server
            .recv_timeout(Duration::from_secs(120))
            .map_err(|_| OAuthError::Timeout)?
            .ok_or(OAuthError::Timeout)?;

        let url = request.url().to_string();
        let code = url::Url::parse(&format!("http://localhost{}", url))
            .ok()
            .and_then(|u| u.query_pairs()
                .find(|(k, _)| k == "code")
                .map(|(_, v)| v.to_string()))
            .ok_or(OAuthError::NoToken)?;

        // Respond with success page
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

        // Wait for JS to POST the token
        let token_request = server
            .recv_timeout(Duration::from_secs(30))
            .map_err(|_| OAuthError::Timeout)?
            .ok_or(OAuthError::Timeout)?;

        let mut body = String::new();
        token_request.as_reader().read_to_string(&mut body)?;

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
    Browser(#[from] webbrowser::Error),

    #[error("Timeout waiting for authentication")]
    Timeout,

    #[error("No token/code received")]
    NoToken,

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}
```

---

## 6. Steam Authentication (P0)

### 6.1 Prerequisites

- Steam App ID (Steamworks partner portal)
- Steamworks SDK (client)
- Steam Publisher Web API Key (Nakama server-side validation)

### 6.2 Flow

```
┌────────┐          ┌───────────┐          ┌────────┐
│ Client │          │ Steam SDK │          │ Nakama │
└───┬────┘          └─────┬─────┘          └───┬────┘
    │                     │                    │
    │ 1. Initialize SDK   │                    │
    │────────────────────▶│                    │
    │                     │                    │
    │ 2. Get session      │                    │
    │    ticket           │                    │
    │────────────────────▶│                    │
    │                     │                    │
    │ 3. Ticket bytes     │                    │
    │◀────────────────────│                    │
    │                     │                    │
    │ 4. AuthenticateSteam ───────────────────▶│
    │    (hex-encoded ticket)                  │
    │                     │                    │
    │                     │    5. Validate     │
    │                     │       with Steam   │
    │                     │       Web API      │
    │                     │                    │
    │ 6. Session          │                    │
    │◀──────────────────────────────────────────
    │                     │                    │
```

### 6.3 Client Implementation

```rust
// client/src/auth/steam.rs

use steamworks::{Client, ClientManager, SingleClient};
use std::sync::Arc;
use parking_lot::Mutex;

pub struct SteamAuth {
    client: Client,
    _cb: Arc<Mutex<Option<steamworks::CallbackHandle>>>,
}

impl SteamAuth {
    /// Initialize Steamworks SDK
    /// Requires steam_appid.txt in working directory OR Steam running with the game
    pub fn init() -> Result<Self, SteamAuthError> {
        let client = Client::init()?;

        Ok(Self {
            client,
            _cb: Arc::new(Mutex::new(None)),
        })
    }

    /// Get session ticket for Nakama authentication
    pub fn get_session_ticket(&self) -> Result<SteamTicket, SteamAuthError> {
        let user = self.client.user();
        let (ticket_data, ticket_handle) = user.authentication_session_ticket();
        let steam_id = user.steam_id();

        Ok(SteamTicket {
            data: ticket_data,
            handle: ticket_handle,
            steam_id: steam_id.raw(),
        })
    }

    /// Get Steam display name
    pub fn display_name(&self) -> String {
        self.client.friends().name()
    }

    /// Must be called regularly (in game loop) to process Steam callbacks
    pub fn run_callbacks(&self) {
        self.client.run_callbacks();
    }
}

pub struct SteamTicket {
    pub data: Vec<u8>,
    pub handle: steamworks::AuthTicket,
    pub steam_id: u64,
}

impl SteamTicket {
    /// Hex-encoded ticket for Nakama
    pub fn to_hex(&self) -> String {
        hex::encode(&self.data)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SteamAuthError {
    #[error("Steam initialization failed: {0}")]
    Init(#[from] steamworks::SteamError),

    #[error("Failed to get session ticket")]
    TicketFailed,
}
```

### 6.4 Nakama Integration

```rust
// mello-core/src/nakama/auth.rs

impl NakamaClient {
    /// Authenticate with Steam session ticket
    pub async fn authenticate_steam(
        &mut self,
        ticket: &[u8],
        create: bool,
        username: Option<&str>,
    ) -> Result<Session, Error> {
        let url = format!(
            "{}/v2/account/authenticate/steam?create={}&username={}",
            self.base_url, create, username.unwrap_or("")
        );

        let body = serde_json::json!({ "token": hex::encode(ticket) });

        let response = self.http_client
            .post(&url)
            .basic_auth("", Some(&self.server_key))
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            let error: NakamaError = response.json().await?;
            return Err(Error::Auth(error.message));
        }

        let session: Session = response.json().await?;
        self.session = Some(session.clone());
        Ok(session)
    }
}
```

### 6.5 Backend Configuration

```yaml
# backend/nakama/data/local.yml (add to existing)

steam:
  publisher_key: "${STEAM_PUBLISHER_KEY}"
  app_id: ${STEAM_APP_ID}
```

Nakama handles Steam ticket validation natively — no custom hook needed.

---

## 7. Google Authentication (P0)

### 7.1 Flow

Uses **Authorization Code + PKCE** (no client secret on the native side).

```
┌────────┐        ┌─────────┐        ┌────────┐        ┌────────┐
│ Client │        │ Browser │        │ Google │        │ Nakama │
└───┬────┘        └────┬────┘        └───┬────┘        └───┬────┘
    │                  │                 │                  │
    │ 1. Generate PKCE │                 │                  │
    │    verifier +    │                 │                  │
    │    challenge     │                 │                  │
    │                  │                 │                  │
    │ 2. Start local   │                 │                  │
    │    HTTP server   │                 │                  │
    │    (port 29405)  │                 │                  │
    │                  │                 │                  │
    │ 3. Open ────────▶│                 │                  │
    │    browser       │                 │                  │
    │                  │ 4. Auth URL ───▶│                  │
    │                  │                 │                  │
    │                  │ 5. User signs  │                  │
    │                  │    in + grants  │                  │
    │                  │◀────────────────│                  │
    │                  │                 │                  │
    │ 6. Callback with │                 │                  │
    │    ?code=        │                 │                  │
    │◀─────────────────│                 │                  │
    │                  │                 │                  │
    │ 7. Exchange code │                 │                  │
    │    + verifier ──────────────────────────────────────▶│
    │    for id_token  │                 │                  │
    │                  │                 │                  │
    │                  │                 │  8. Validate     │
    │                  │                 │     id_token     │
    │                  │                 │◀─────────────────│
    │                  │                 │                  │
    │ 9. Session ◀─────────────────────────────────────────│
    │                  │                 │                  │
```

### 7.2 Client Implementation

```rust
// client/src/auth/google.rs

use crate::auth::oauth::{OAuthFlow, OAuthMode, PkceChallenge, OAuthError};

const GOOGLE_CLIENT_ID: &str = env!("GOOGLE_CLIENT_ID");
const REDIRECT_URI: &str = "http://localhost:29405/callback";

pub struct GoogleAuth;

impl GoogleAuth {
    /// Initiates Google OAuth2 PKCE flow and returns an authorization code.
    /// The code + PKCE verifier are sent to Nakama which exchanges them
    /// for an id_token and validates it.
    pub fn authenticate() -> Result<(String, String), OAuthError> {
        let pkce = PkceChallenge::generate();

        let auth_url = format!(
            "https://accounts.google.com/o/oauth2/v2/auth\
             ?client_id={client_id}\
             &redirect_uri={redirect_uri}\
             &response_type=code\
             &scope=openid%20profile%20email\
             &code_challenge={challenge}\
             &code_challenge_method=S256",
            client_id = GOOGLE_CLIENT_ID,
            redirect_uri = urlencoding::encode(REDIRECT_URI),
            challenge = pkce.challenge,
        );

        let code = OAuthFlow::execute(&auth_url, OAuthMode::AuthorizationCode)?;
        Ok((code, pkce.verifier))
    }
}
```

### 7.3 Nakama Integration

Nakama has a **native** `/v2/account/authenticate/google` endpoint. The client exchanges the auth code for an `id_token` first, then sends it to Nakama.

```rust
// mello-core/src/nakama/auth.rs

impl NakamaClient {
    /// Exchange Google auth code for id_token, then authenticate with Nakama.
    pub async fn authenticate_google(
        &mut self,
        code: &str,
        pkce_verifier: &str,
        create: bool,
        username: Option<&str>,
    ) -> Result<Session, Error> {
        // Step 1: Exchange code for tokens at Google's token endpoint
        let token_resp = self.http_client
            .post("https://oauth2.googleapis.com/token")
            .form(&[
                ("code", code),
                ("client_id", &self.config.google_client_id),
                ("redirect_uri", "http://localhost:29405/callback"),
                ("grant_type", "authorization_code"),
                ("code_verifier", pkce_verifier),
            ])
            .send()
            .await?;

        #[derive(Deserialize)]
        struct TokenResponse { id_token: String }

        let tokens: TokenResponse = token_resp.json().await
            .map_err(|_| Error::Auth("Google token exchange failed".into()))?;

        // Step 2: Send id_token to Nakama's native Google auth
        let url = format!(
            "{}/v2/account/authenticate/google?create={}&username={}",
            self.base_url, create, username.unwrap_or("")
        );

        let body = serde_json::json!({ "token": tokens.id_token });

        let response = self.http_client
            .post(&url)
            .basic_auth("", Some(&self.server_key))
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            let error: NakamaError = response.json().await?;
            return Err(Error::Auth(error.message));
        }

        let session: Session = response.json().await?;
        self.session = Some(session.clone());
        Ok(session)
    }
}
```

No backend hook needed — Nakama validates Google `id_token` natively.

---

## 8. Twitch Authentication (P0)

### 8.1 Flow

Uses **Authorization Code + PKCE**. Since Nakama has no native Twitch support, we use the custom auth endpoint with a `BeforeAuthenticateCustom` hook.

```
┌────────┐        ┌─────────┐        ┌────────┐        ┌────────┐
│ Client │        │ Browser │        │ Twitch │        │ Nakama │
└───┬────┘        └────┬────┘        └───┬────┘        └───┬────┘
    │                  │                 │                  │
    │ 1. Generate PKCE │                 │                  │
    │ 2. Start server  │                 │                  │
    │ 3. Open ────────▶│                 │                  │
    │                  │ 4. Auth URL ───▶│                  │
    │                  │ 5. Authorize   │                  │
    │                  │◀────────────────│                  │
    │ 6. ?code= ◀──────│                 │                  │
    │                  │                 │                  │
    │ 7. Exchange code ────────────────▶│                  │
    │    + verifier    │                 │                  │
    │ 8. access_token ◀─────────────────│                  │
    │                  │                 │                  │
    │ 9. AuthenticateCustom ───────────────────────────────▶
    │    (token, provider=twitch)       │                  │
    │                  │                 │ 10. Validate     │
    │                  │                 │◀─────────────────│
    │ 11. Session ◀────────────────────────────────────────│
    │                  │                 │                  │
```

### 8.2 Client Implementation

```rust
// client/src/auth/twitch.rs

use crate::auth::oauth::{OAuthFlow, OAuthMode, PkceChallenge, OAuthError};

const TWITCH_CLIENT_ID: &str = env!("TWITCH_CLIENT_ID");
const REDIRECT_URI: &str = "http://localhost:29405/callback";

pub struct TwitchAuth;

impl TwitchAuth {
    /// Initiates Twitch OAuth2 PKCE flow.
    /// Returns an access token (code exchange happens client-side).
    pub async fn authenticate(http: &reqwest::Client) -> Result<String, OAuthError> {
        let pkce = PkceChallenge::generate();

        let auth_url = format!(
            "https://id.twitch.tv/oauth2/authorize\
             ?client_id={client_id}\
             &redirect_uri={redirect_uri}\
             &response_type=code\
             &scope=user:read:email\
             &code_challenge={challenge}\
             &code_challenge_method=S256\
             &force_verify=true",
            client_id = TWITCH_CLIENT_ID,
            redirect_uri = urlencoding::encode(REDIRECT_URI),
            challenge = pkce.challenge,
        );

        let code = OAuthFlow::execute(&auth_url, OAuthMode::AuthorizationCode)?;

        // Exchange code for access token
        let token_resp = http
            .post("https://id.twitch.tv/oauth2/token")
            .form(&[
                ("client_id", TWITCH_CLIENT_ID),
                ("code", &code),
                ("grant_type", "authorization_code"),
                ("redirect_uri", REDIRECT_URI),
                ("code_verifier", &pkce.verifier),
            ])
            .send()
            .await
            .map_err(|_| OAuthError::NoToken)?;

        #[derive(serde::Deserialize)]
        struct TokenResponse { access_token: String }

        let tokens: TokenResponse = token_resp.json().await
            .map_err(|_| OAuthError::NoToken)?;

        Ok(tokens.access_token)
    }
}
```

### 8.3 Nakama Integration

Twitch uses the **custom auth** endpoint, same pattern as Discord.

```rust
// mello-core/src/nakama/auth.rs

impl NakamaClient {
    /// Authenticate with Twitch access token via custom auth.
    pub async fn authenticate_twitch(
        &mut self,
        access_token: &str,
        create: bool,
        username: Option<&str>,
    ) -> Result<Session, Error> {
        let url = format!(
            "{}/v2/account/authenticate/custom?create={}&username={}",
            self.base_url, create, username.unwrap_or("")
        );

        let body = serde_json::json!({
            "id": access_token,
            "vars": { "provider": "twitch" }
        });

        let response = self.http_client
            .post(&url)
            .basic_auth("", Some(&self.server_key))
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            let error: NakamaError = response.json().await?;
            return Err(Error::Auth(error.message));
        }

        let session: Session = response.json().await?;
        self.session = Some(session.clone());
        Ok(session)
    }
}
```

### 8.4 Backend: Twitch Token Validation

See [Section 14](#14-backend-custom-auth-hook) for the unified `BeforeAuthenticateCustom` hook that handles both Twitch and Discord.

---

## 9. Discord Authentication (P1)

### 9.1 Flow

Uses **implicit flow** (token in URL fragment). Lower priority since Discord is a direct competitor.

```
┌────────┐          ┌─────────┐          ┌─────────┐          ┌────────┐
│ Client │          │ Browser │          │ Discord │          │ Nakama │
└───┬────┘          └────┬────┘          └────┬────┘          └───┬────┘
    │                    │                    │                    │
    │ 1. Start local     │                    │                    │
    │    HTTP server     │                    │                    │
    │    (port 29405)    │                    │                    │
    │                    │                    │                    │
    │ 2. Open browser ──▶│                    │                    │
    │                    │ 3. OAuth URL ─────▶│                    │
    │                    │ 4. User authorizes │                    │
    │                    │◀───────────────────│                    │
    │ 5. Callback with   │                    │                    │
    │    #access_token   │                    │                    │
    │◀───────────────────│                    │                    │
    │                    │                    │                    │
    │ 6. AuthenticateCustom ──────────────────────────────────────▶
    │    (token, provider=discord)            │                    │
    │                    │                    │ 7. Validate token  │
    │                    │                    │◀───────────────────│
    │ 8. Session ◀────────────────────────────────────────────────│
    │                    │                    │                    │
```

### 9.2 Client Implementation

```rust
// client/src/auth/discord.rs

use crate::auth::oauth::{OAuthFlow, OAuthMode, OAuthError};

const DISCORD_CLIENT_ID: &str = env!("DISCORD_CLIENT_ID");
const REDIRECT_URI: &str = "http://localhost:29405/callback";

pub struct DiscordAuth;

impl DiscordAuth {
    /// Initiates Discord OAuth flow and returns access token.
    pub fn authenticate() -> Result<String, OAuthError> {
        let auth_url = format!(
            "https://discord.com/api/oauth2/authorize\
             ?client_id={client_id}\
             &redirect_uri={redirect_uri}\
             &response_type=token\
             &scope=identify",
            client_id = DISCORD_CLIENT_ID,
            redirect_uri = urlencoding::encode(REDIRECT_URI),
        );

        OAuthFlow::execute(&auth_url, OAuthMode::Implicit)
    }
}
```

### 9.3 Nakama Integration

```rust
// mello-core/src/nakama/auth.rs

impl NakamaClient {
    /// Authenticate with Discord access token via custom auth.
    pub async fn authenticate_discord(
        &mut self,
        access_token: &str,
        create: bool,
        username: Option<&str>,
    ) -> Result<Session, Error> {
        let url = format!(
            "{}/v2/account/authenticate/custom?create={}&username={}",
            self.base_url, create, username.unwrap_or("")
        );

        let body = serde_json::json!({
            "id": access_token,
            "vars": { "provider": "discord" }
        });

        let response = self.http_client
            .post(&url)
            .basic_auth("", Some(&self.server_key))
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            let error: NakamaError = response.json().await?;
            return Err(Error::Auth(error.message));
        }

        let session: Session = response.json().await?;
        self.session = Some(session.clone());
        Ok(session)
    }
}
```

---

## 10. Apple Authentication (P1)

### 10.1 Overview

Required for future macOS App Store distribution (Apple mandates Sign in with Apple when other social logins are present). Uses Apple's JS-based web flow in the browser, similar to OAuth.

### 10.2 Flow

```
┌────────┐        ┌─────────┐        ┌────────┐        ┌────────┐
│ Client │        │ Browser │        │ Apple  │        │ Nakama │
└───┬────┘        └────┬────┘        └───┬────┘        └───┬────┘
    │                  │                 │                  │
    │ 1. Start server  │                 │                  │
    │ 2. Open ────────▶│                 │                  │
    │                  │ 3. Apple Sign  │                  │
    │                  │    In page ───▶│                  │
    │                  │                 │                  │
    │                  │ 4. User signs  │                  │
    │                  │    in (Face ID │                  │
    │                  │    / password) │                  │
    │                  │◀────────────────│                  │
    │                  │                 │                  │
    │ 5. Callback with │                 │                  │
    │    id_token      │                 │                  │
    │◀─────────────────│                 │                  │
    │                  │                 │                  │
    │ 6. AuthenticateApple ────────────────────────────────▶
    │    (id_token)    │                 │                  │
    │                  │                 │  7. Validate     │
    │                  │                 │     JWT          │
    │                  │                 │◀─────────────────│
    │ 8. Session ◀─────────────────────────────────────────│
    │                  │                 │                  │
```

### 10.3 Client Implementation

```rust
// client/src/auth/apple.rs

use crate::auth::oauth::{OAuthFlow, OAuthMode, OAuthError};

const APPLE_CLIENT_ID: &str = env!("APPLE_CLIENT_ID");
const REDIRECT_URI: &str = "http://localhost:29405/callback";

pub struct AppleAuth;

impl AppleAuth {
    /// Initiates Apple Sign In flow and returns id_token.
    /// Apple uses response_mode=fragment for native apps.
    pub fn authenticate() -> Result<String, OAuthError> {
        let auth_url = format!(
            "https://appleid.apple.com/auth/authorize\
             ?client_id={client_id}\
             &redirect_uri={redirect_uri}\
             &response_type=code%20id_token\
             &scope=name%20email\
             &response_mode=fragment",
            client_id = APPLE_CLIENT_ID,
            redirect_uri = urlencoding::encode(REDIRECT_URI),
        );

        // id_token comes in the fragment, same as implicit flow
        OAuthFlow::execute(&auth_url, OAuthMode::Implicit)
    }
}
```

### 10.4 Nakama Integration

Nakama has a **native** `/v2/account/authenticate/apple` endpoint.

```rust
// mello-core/src/nakama/auth.rs

impl NakamaClient {
    /// Authenticate with Apple id_token.
    pub async fn authenticate_apple(
        &mut self,
        id_token: &str,
        create: bool,
        username: Option<&str>,
    ) -> Result<Session, Error> {
        let url = format!(
            "{}/v2/account/authenticate/apple?create={}&username={}",
            self.base_url, create, username.unwrap_or("")
        );

        let body = serde_json::json!({ "token": id_token });

        let response = self.http_client
            .post(&url)
            .basic_auth("", Some(&self.server_key))
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            let error: NakamaError = response.json().await?;
            return Err(Error::Auth(error.message));
        }

        let session: Session = response.json().await?;
        self.session = Some(session.clone());
        Ok(session)
    }
}
```

No backend hook needed — Nakama validates Apple JWTs natively using Apple's public keys.

---

## 11. Email/Password (P1)

Standard Nakama email/password auth. Already implemented in codebase.

```rust
// mello-core/src/nakama/client.rs (existing)

pub async fn login_email(&mut self, email: &str, password: &str) -> Result<User> {
    let url = format!(
        "{}/v2/account/authenticate/email?create=true",
        self.config.http_base()
    );

    let resp = self.http.post(&url)
        .basic_auth(&self.config.nakama_key, Some(""))
        .json(&serde_json::json!({ "email": email, "password": password }))
        .send()
        .await?;

    // ... error handling, session parsing ...
}
```

No additional work needed. Always available on both cloud and self-hosted.

---

## 12. Session Persistence

Uses OS-native secure storage via `keyring` crate. Already implemented.

```rust
// client/src/auth/store.rs

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AuthProvider {
    Steam,
    Google,
    Twitch,
    Discord,
    Apple,
    Email,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredSession {
    pub user_id: String,
    pub username: String,
    pub token: String,
    pub refresh_token: String,
    pub expires_at: i64,
    pub provider: AuthProvider,
}

pub struct AuthStore { /* ... same as existing ... */ }
```

See existing `mello-core/src/session.rs` for the current implementation.

---

## 13. Auth Manager (Unified Interface)

```rust
// client/src/auth/mod.rs

mod oauth;
mod steam;
mod google;
mod twitch;
mod discord;
mod apple;
mod store;

pub use store::{AuthStore, StoredSession, AuthProvider};

use mello_core::nakama::Session;

pub struct AuthManager {
    store: AuthStore,
    steam: Option<SteamAuth>,
    enabled_providers: Vec<String>,
}

impl AuthManager {
    pub fn new() -> Result<Self, AuthError> {
        let store = AuthStore::new()?;

        // Try to init Steam (optional, won't fail if Steam not running)
        let steam = SteamAuth::init().ok();
        if steam.is_some() {
            log::info!("Steam SDK initialized");
        }

        Ok(Self {
            store,
            steam,
            enabled_providers: vec!["email".into()], // default until RPC response
        })
    }

    /// Fetch enabled providers from backend (call on startup)
    pub async fn load_providers(&mut self, core: &mut MelloCore) {
        match core.nakama().rpc("auth/providers", "{}").await {
            Ok(resp) => {
                if let Ok(parsed) = serde_json::from_str::<ProvidersResponse>(&resp) {
                    self.enabled_providers = parsed.providers;
                }
            }
            Err(e) => log::warn!("Failed to fetch providers: {}", e),
        }
    }

    pub fn is_provider_enabled(&self, provider: &str) -> bool {
        self.enabled_providers.contains(&provider.to_string())
    }

    /// Check for saved session on startup
    pub async fn try_restore_session(&self, core: &mut MelloCore) -> Result<Option<Session>, AuthError> {
        let stored = match self.store.load()? {
            Some(s) => s,
            None => return Ok(None),
        };

        log::info!("Found stored session for {}", stored.username);

        match core.nakama().restore_session(&stored.token, &stored.refresh_token).await {
            Ok(session) => {
                let updated = StoredSession {
                    token: session.token.clone(),
                    refresh_token: session.refresh_token.clone(),
                    expires_at: session.expires_at,
                    ..stored
                };
                self.store.save(&updated)?;
                Ok(Some(session))
            }
            Err(e) => {
                log::warn!("Session restore failed: {}", e);
                self.store.clear()?;
                Ok(None)
            }
        }
    }

    // --- Login methods ---

    pub async fn login_steam(&self, core: &mut MelloCore) -> Result<Session, AuthError> {
        let steam = self.steam.as_ref().ok_or(AuthError::SteamNotAvailable)?;
        let ticket = steam.get_session_ticket()?;
        let session = core.nakama().authenticate_steam(&ticket.data, true, None).await?;
        self.persist(session, AuthProvider::Steam)
    }

    pub async fn login_google(&self, core: &mut MelloCore) -> Result<Session, AuthError> {
        let (code, verifier) = GoogleAuth::authenticate()?;
        let session = core.nakama().authenticate_google(&code, &verifier, true, None).await?;
        self.persist(session, AuthProvider::Google)
    }

    pub async fn login_twitch(&self, core: &mut MelloCore) -> Result<Session, AuthError> {
        let token = TwitchAuth::authenticate(&core.nakama().http_client()).await?;
        let session = core.nakama().authenticate_twitch(&token, true, None).await?;
        self.persist(session, AuthProvider::Twitch)
    }

    pub async fn login_discord(&self, core: &mut MelloCore) -> Result<Session, AuthError> {
        let token = DiscordAuth::authenticate()?;
        let session = core.nakama().authenticate_discord(&token, true, None).await?;
        self.persist(session, AuthProvider::Discord)
    }

    pub async fn login_apple(&self, core: &mut MelloCore) -> Result<Session, AuthError> {
        let id_token = AppleAuth::authenticate()?;
        let session = core.nakama().authenticate_apple(&id_token, true, None).await?;
        self.persist(session, AuthProvider::Apple)
    }

    pub async fn login_email(
        &self, core: &mut MelloCore, email: &str, password: &str,
    ) -> Result<Session, AuthError> {
        let session = core.nakama().authenticate_email(email, password, false).await?;
        self.persist(session, AuthProvider::Email)
    }

    pub fn logout(&self) -> Result<(), AuthError> {
        self.store.clear()?;
        Ok(())
    }

    pub fn steam_available(&self) -> bool {
        self.steam.is_some()
    }

    // --- Internal ---

    fn persist(&self, session: Session, provider: AuthProvider) -> Result<Session, AuthError> {
        self.store.save(&StoredSession {
            user_id: session.user_id.clone(),
            username: session.username.clone(),
            token: session.token.clone(),
            refresh_token: session.refresh_token.clone(),
            expires_at: session.expires_at,
            provider,
        })?;
        Ok(session)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("Store error: {0}")]
    Store(#[from] store::AuthStoreError),

    #[error("OAuth error: {0}")]
    OAuth(#[from] oauth::OAuthError),

    #[error("Steam error: {0}")]
    Steam(#[from] SteamAuthError),

    #[error("Steam not available")]
    SteamNotAvailable,

    #[error("Nakama error: {0}")]
    Nakama(#[from] mello_core::Error),
}
```

---

## 14. Backend: Custom Auth Hook

The `BeforeAuthenticateCustom` hook dispatches on the `provider` var to validate Twitch and Discord tokens.

```go
// backend/nakama/data/modules/auth.go

package main

import (
    "context"
    "database/sql"
    "encoding/json"
    "fmt"
    "io"
    "net/http"
    "os"

    "github.com/heroiclabs/nakama-common/api"
    "github.com/heroiclabs/nakama-common/runtime"
)

// --- Provider user types ---

type DiscordUser struct {
    ID       string `json:"id"`
    Username string `json:"username"`
    Avatar   string `json:"avatar"`
}

type TwitchUser struct {
    ID          string `json:"id"`
    Login       string `json:"login"`
    DisplayName string `json:"display_name"`
    Email       string `json:"email"`
}

// --- BeforeAuthenticateCustom hook ---

func BeforeAuthenticateCustom(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, in *api.AuthenticateCustomRequest) (*api.AuthenticateCustomRequest, error) {
    if in.Account.Vars == nil {
        return in, nil
    }

    provider := in.Account.Vars["provider"]
    token := in.Account.Id // token is passed as the custom ID

    switch provider {
    case "discord":
        return handleDiscordAuth(logger, in, token)
    case "twitch":
        return handleTwitchAuth(logger, in, token)
    default:
        return in, nil
    }
}

func handleDiscordAuth(logger runtime.Logger, in *api.AuthenticateCustomRequest, token string) (*api.AuthenticateCustomRequest, error) {
    user, err := validateDiscordToken(token)
    if err != nil {
        logger.Error("Discord validation failed: %v", err)
        return nil, runtime.NewError("Invalid Discord token", 16)
    }

    in.Account.Id = fmt.Sprintf("discord_%s", user.ID)
    if in.Username == "" {
        in.Username = user.Username
    }

    logger.Info("Discord auth for user: %s (%s)", user.Username, user.ID)
    return in, nil
}

func handleTwitchAuth(logger runtime.Logger, in *api.AuthenticateCustomRequest, token string) (*api.AuthenticateCustomRequest, error) {
    user, err := validateTwitchToken(token)
    if err != nil {
        logger.Error("Twitch validation failed: %v", err)
        return nil, runtime.NewError("Invalid Twitch token", 16)
    }

    in.Account.Id = fmt.Sprintf("twitch_%s", user.ID)
    if in.Username == "" {
        in.Username = user.Login
    }

    logger.Info("Twitch auth for user: %s (%s)", user.DisplayName, user.ID)
    return in, nil
}

// --- Token validation ---

func validateDiscordToken(token string) (*DiscordUser, error) {
    req, err := http.NewRequest("GET", "https://discord.com/api/users/@me", nil)
    if err != nil {
        return nil, err
    }
    req.Header.Set("Authorization", "Bearer "+token)

    resp, err := http.DefaultClient.Do(req)
    if err != nil {
        return nil, err
    }
    defer resp.Body.Close()

    if resp.StatusCode != 200 {
        body, _ := io.ReadAll(resp.Body)
        return nil, fmt.Errorf("discord API error: %s", body)
    }

    var user DiscordUser
    if err := json.NewDecoder(resp.Body).Decode(&user); err != nil {
        return nil, err
    }
    return &user, nil
}

func validateTwitchToken(token string) (*TwitchUser, error) {
    req, err := http.NewRequest("GET", "https://api.twitch.tv/helix/users", nil)
    if err != nil {
        return nil, err
    }
    req.Header.Set("Authorization", "Bearer "+token)
    req.Header.Set("Client-Id", os.Getenv("TWITCH_CLIENT_ID"))

    resp, err := http.DefaultClient.Do(req)
    if err != nil {
        return nil, err
    }
    defer resp.Body.Close()

    if resp.StatusCode != 200 {
        body, _ := io.ReadAll(resp.Body)
        return nil, fmt.Errorf("twitch API error: %s", body)
    }

    var result struct {
        Data []TwitchUser `json:"data"`
    }
    if err := json.NewDecoder(resp.Body).Decode(&result); err != nil {
        return nil, err
    }
    if len(result.Data) == 0 {
        return nil, fmt.Errorf("no user data from Twitch")
    }
    return &result.Data[0], nil
}

// --- Auth providers discovery RPC ---

func AuthProvidersRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
    providers := []string{"email"}

    if os.Getenv("STEAM_PUBLISHER_KEY") != "" {
        providers = append(providers, "steam")
    }
    if os.Getenv("GOOGLE_CLIENT_ID") != "" {
        providers = append(providers, "google")
    }
    if os.Getenv("TWITCH_CLIENT_ID") != "" {
        providers = append(providers, "twitch")
    }
    if os.Getenv("DISCORD_CLIENT_ID") != "" {
        providers = append(providers, "discord")
    }
    if os.Getenv("APPLE_CLIENT_ID") != "" {
        providers = append(providers, "apple")
    }

    resp, _ := json.Marshal(map[string]interface{}{
        "providers": providers,
    })
    return string(resp), nil
}
```

Register in `InitModule`:

```go
// backend/nakama/data/modules/main.go (add to InitModule)

if err := initializer.RegisterBeforeAuthenticateCustom(BeforeAuthenticateCustom); err != nil {
    return err
}
if err := initializer.RegisterRpc("auth/providers", AuthProvidersRPC); err != nil {
    return err
}
```

---

## 15. Dependencies

```toml
# client/Cargo.toml

[dependencies]
# Auth
keyring = "2"
webbrowser = "1.0"
tiny_http = "0.12"
urlencoding = "2"
hex = "0.4"
rand = "0.8"
sha2 = "0.10"
base64 = "0.22"
url = "2"

# Optional: Steam (feature-flagged)
steamworks = { version = "0.11", optional = true }

[features]
default = []
steam = ["steamworks"]
```

New vs existing: `rand`, `sha2`, `base64`, `url` are added for PKCE support. The rest already exist.

---

## 16. Environment Variables

```bash
# --- Cloud only ---

# Steam
STEAM_PUBLISHER_KEY=your_steam_publisher_key
STEAM_APP_ID=your_steam_app_id

# Google
GOOGLE_CLIENT_ID=your_google_client_id

# Twitch
TWITCH_CLIENT_ID=your_twitch_client_id

# Discord
DISCORD_CLIENT_ID=your_discord_client_id

# Apple
APPLE_CLIENT_ID=your_apple_client_id       # Services ID
APPLE_TEAM_ID=your_apple_team_id
APPLE_KEY_ID=your_apple_key_id

# --- Always required ---

# Nakama
NAKAMA_URL=https://your-nakama.onrender.com
NAKAMA_SERVER_KEY=your_server_key
```

For setup instructions on obtaining these credentials, see [06a-SOCIAL-LOGIN-SETUP.md](./06a-SOCIAL-LOGIN-SETUP.md).

---

## 17. Security Considerations

| Concern | Mitigation |
|---------|------------|
| Token storage | OS-native secure storage (Credential Manager / Keychain) |
| Token in memory | Clear on logout, minimize lifetime |
| OAuth callback | Fixed port 29405, localhost only, binds 127.0.0.1 |
| PKCE | Google and Twitch use S256 challenge — prevents code interception |
| Steam ticket replay | Nakama validates with Steam Web API |
| Discord token scope | `identify` only, no write access |
| Twitch token scope | `user:read:email` only |
| Apple token | JWT validated against Apple's public keys |
| Self-hosted | Social providers disabled entirely, no client IDs exposed |

---

## 18. Testing Checklist

### P0 — Steam
- [ ] Steam login works when Steam is running
- [ ] Steam login fails gracefully when Steam not running
- [ ] Steam button hidden when Steam SDK unavailable

### P0 — Google
- [ ] Google OAuth PKCE flow completes successfully
- [ ] Google token exchange works
- [ ] Nakama session created from Google id_token

### P0 — Twitch
- [ ] Twitch OAuth PKCE flow completes successfully
- [ ] Twitch token exchange works
- [ ] Backend hook validates Twitch token correctly
- [ ] Nakama session created with `twitch_` prefix ID

### P1 — Discord
- [ ] Discord OAuth implicit flow completes
- [ ] Backend hook validates Discord token
- [ ] Nakama session created with `discord_` prefix ID

### P1 — Apple
- [ ] Apple Sign In flow completes
- [ ] Nakama session created from Apple id_token

### P1 — Email/Password
- [ ] Email login still works
- [ ] Email signup creates account

### General
- [ ] Session persists across restarts
- [ ] Logout clears stored credentials
- [ ] Session refresh works when token near expiry
- [ ] Invalid stored session is cleared
- [ ] Self-hosted shows only email/password
- [ ] Provider discovery RPC returns correct list
- [ ] Login UI adapts to available providers

---

*This spec covers authentication. For setup instructions, see [06a-SOCIAL-LOGIN-SETUP.md](./06a-SOCIAL-LOGIN-SETUP.md). For auto-updates, see [07-AUTO-UPDATER.md](./07-AUTO-UPDATER.md).*
