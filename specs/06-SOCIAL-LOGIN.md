# MELLO Social Login Specification

> **Component:** Authentication (Social Login)  
> **Version:** 0.2  
> **Status:** Planned  
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)

---

## 1. Overview

Mello supports three authentication methods to minimize friction for new users:

| Method | Priority | Use Case |
|--------|----------|----------|
| **Discord** | P0 | Primary — most gamers have Discord |
| **Steam** | P1 | Secondary — validates gamer identity |
| **Email/Password** | P0 | Fallback — already implemented |

All auth flows terminate at Nakama, which handles token validation and session management.

---

## 2. Architecture

```
┌─────────────────────────────────────────────────────────────────────────┐
│                         CLIENT (mello)                                  │
│                                                                         │
│   ┌───────────────┐   ┌───────────────┐   ┌───────────────┐            │
│   │    Discord    │   │     Steam     │   │     Email     │            │
│   │    Button     │   │    Button     │   │     Form      │            │
│   └───────┬───────┘   └───────┬───────┘   └───────┬───────┘            │
│           │                   │                   │                     │
│           ▼                   ▼                   ▼                     │
│   ┌───────────────┐   ┌───────────────┐   ┌───────────────┐            │
│   │  OAuth2 Flow  │   │  Steamworks   │   │  Direct API   │            │
│   │  (browser)    │   │  SDK          │   │  Call         │            │
│   └───────┬───────┘   └───────┬───────┘   └───────┬───────┘            │
│           │                   │                   │                     │
│           │ Access Token      │ Session Ticket    │ email/pass          │
│           │                   │                   │                     │
│           └─────────────────┬─┴───────────────────┘                     │
│                             │                                           │
│                             ▼                                           │
│                   ┌───────────────────┐                                 │
│                   │   mello-core      │                                 │
│                   │   AuthManager     │                                 │
│                   └─────────┬─────────┘                                 │
│                             │                                           │
└─────────────────────────────┼───────────────────────────────────────────┘
                              │ HTTPS
                              ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                           NAKAMA                                        │
│                                                                         │
│   POST /v2/account/authenticate/custom   (Discord)                      │
│   POST /v2/account/authenticate/steam    (Steam)                        │
│   POST /v2/account/authenticate/email    (Email)                        │
│                                                                         │
│   Nakama validates tokens with Discord/Steam APIs                       │
│   Returns: session token + refresh token                                │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

---

## 3. Login Screen UI

```
┌─────────────────────────────────────────────────────────────┐
│                                                             │
│                          MELLO                              │
│                                                             │
│                 Hang out with your crew.                    │
│                Jump into anyone's stream.                   │
│                                                             │
│    ┌─────────────────────┐    ┌─────────────────────┐       │
│    │  💬  Discord        │    │  🎮  Steam          │       │
│    └─────────────────────┘    └─────────────────────┘       │
│                                                             │
│    ───────────────────── or ─────────────────────────       │
│                                                             │
│    ┌─────────────────────────────────────────────────┐      │
│    │  Email                                          │      │
│    └─────────────────────────────────────────────────┘      │
│    ┌─────────────────────────────────────────────────┐      │
│    │  Password                                       │      │
│    └─────────────────────────────────────────────────┘      │
│                                                             │
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

**Note:** Sessions are automatically persisted to OS secure storage. No "Remember me" checkbox needed.

---

## 4. Discord Authentication

### 4.1 Flow

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
    │                    │                    │                    │
    │                    │ 3. OAuth URL ─────▶│                    │
    │                    │                    │                    │
    │                    │ 4. User authorizes │                    │
    │                    │◀───────────────────│                    │
    │                    │                    │                    │
    │ 5. Callback with   │                    │                    │
    │    access_token    │                    │                    │
    │◀───────────────────│                    │                    │
    │                    │                    │                    │
    │ 6. Authenticate ─────────────────────────────────────────────▶
    │    (access_token)  │                    │                    │
    │                    │                    │                    │
    │                    │                    │ 7. Validate token  │
    │                    │                    │◀───────────────────│
    │                    │                    │                    │
    │ 8. Session ◀──────────────────────────────────────────────────
    │                    │                    │                    │
```

### 4.2 Client Implementation

```rust
// client/src/auth/discord.rs

use tiny_http::{Server, Response, Header};
use webbrowser;
use std::time::Duration;

const DISCORD_CLIENT_ID: &str = env!("DISCORD_CLIENT_ID");
const REDIRECT_PORT: u16 = 29405;
const REDIRECT_URI: &str = "http://localhost:29405/callback";

pub struct DiscordAuth;

impl DiscordAuth {
    /// Initiates Discord OAuth flow and returns access token
    pub async fn authenticate() -> Result<String, DiscordAuthError> {
        // 1. Start local callback server
        let server = Server::http(format!("127.0.0.1:{}", REDIRECT_PORT))
            .map_err(|e| DiscordAuthError::ServerStart(e.to_string()))?;
        
        // 2. Build OAuth URL
        //    Using "token" response type for implicit flow (no server-side exchange needed)
        let auth_url = format!(
            "https://discord.com/api/oauth2/authorize\
             ?client_id={client_id}\
             &redirect_uri={redirect_uri}\
             &response_type=token\
             &scope=identify",
            client_id = DISCORD_CLIENT_ID,
            redirect_uri = urlencoding::encode(REDIRECT_URI),
        );
        
        // 3. Open in default browser
        log::info!("Opening Discord OAuth in browser...");
        webbrowser::open(&auth_url)?;
        
        // 4. Wait for redirect (token is in URL fragment)
        let token = Self::wait_for_callback(&server)?;
        
        log::info!("Discord authentication successful");
        Ok(token)
    }
    
    fn wait_for_callback(server: &Server) -> Result<String, DiscordAuthError> {
        // First request: Discord redirects with token in fragment
        // Fragments aren't sent to server, so we need JS to extract it
        let request = server
            .recv_timeout(Duration::from_secs(120))?
            .ok_or(DiscordAuthError::Timeout)?;
        
        // Serve HTML that extracts token from fragment
        let extractor_html = r#"<!DOCTYPE html>
<html>
<head><title>Mello - Authenticating</title></head>
<body style="font-family: system-ui; display: flex; justify-content: center; align-items: center; height: 100vh; margin: 0; background: #1a1a1a; color: white;">
    <div id="status">
        <h1>🔐 Authenticating...</h1>
        <p>Please wait while we complete sign-in.</p>
    </div>
    <script>
        const fragment = window.location.hash.substring(1);
        const params = new URLSearchParams(fragment);
        const token = params.get('access_token');
        const error = params.get('error');
        
        if (error) {
            document.getElementById('status').innerHTML = 
                '<h1>❌ Authentication Failed</h1><p>' + error + '</p>';
        } else if (token) {
            fetch('/token', {
                method: 'POST',
                body: token
            }).then(() => {
                document.getElementById('status').innerHTML = 
                    '<h1>✅ Success!</h1><p>You can close this tab and return to Mello.</p>';
            });
        } else {
            document.getElementById('status').innerHTML = 
                '<h1>❌ No Token</h1><p>Authentication failed. Please try again.</p>';
        }
    </script>
</body>
</html>"#;
        
        let response = Response::from_string(extractor_html)
            .with_header(Header::from_bytes("Content-Type", "text/html").unwrap());
        request.respond(response)?;
        
        // Second request: JS posts the token to us
        let token_request = server
            .recv_timeout(Duration::from_secs(30))?
            .ok_or(DiscordAuthError::Timeout)?;
        
        // Read token from body
        let mut body = String::new();
        token_request.as_reader().read_to_string(&mut body)?;
        
        if body.is_empty() {
            return Err(DiscordAuthError::NoToken);
        }
        
        // Respond to close the connection
        token_request.respond(Response::from_string("OK"))?;
        
        Ok(body)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DiscordAuthError {
    #[error("Failed to start callback server: {0}")]
    ServerStart(String),
    
    #[error("Failed to open browser: {0}")]
    Browser(#[from] webbrowser::Error),
    
    #[error("Timeout waiting for authentication")]
    Timeout,
    
    #[error("No token received from Discord")]
    NoToken,
    
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<Option<tiny_http::Request>> for DiscordAuthError {
    fn from(_: Option<tiny_http::Request>) -> Self {
        DiscordAuthError::Timeout
    }
}
```

### 4.3 Nakama Integration

```rust
// mello-core/src/nakama/auth.rs

impl NakamaClient {
    /// Authenticate with Discord access token
    pub async fn authenticate_discord(
        &mut self,
        access_token: &str,
        create: bool,
        username: Option<&str>,
    ) -> Result<Session, Error> {
        #[derive(Serialize)]
        struct Request<'a> {
            token: &'a str,
            vars: Option<HashMap<String, String>>,
        }
        
        let url = format!(
            "{}/v2/account/authenticate/custom?create={}&username={}",
            self.base_url,
            create,
            username.unwrap_or("")
        );
        
        // Nakama custom auth with Discord token
        // Server-side hook validates with Discord API
        let body = Request {
            token: access_token,
            vars: Some({
                let mut m = HashMap::new();
                m.insert("provider".into(), "discord".into());
                m
            }),
        };
        
        let response = self.http_client
            .post(&url)
            .basic_auth("", Some(&self.server_key))
            .json(&body)
            .send()
            .await?;
        
        let session: Session = response.json().await?;
        self.session = Some(session.clone());
        
        Ok(session)
    }
}
```

### 4.4 Backend: Discord Token Validation

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
    
    "github.com/heroiclabs/nakama-common/api"
    "github.com/heroiclabs/nakama-common/runtime"
)

type DiscordUser struct {
    ID            string `json:"id"`
    Username      string `json:"username"`
    Discriminator string `json:"discriminator"`
    Avatar        string `json:"avatar"`
}

func BeforeAuthenticateCustom(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, in *api.AuthenticateCustomRequest) (*api.AuthenticateCustomRequest, error) {
    // Check if this is a Discord auth
    if in.Account.Vars == nil || in.Account.Vars["provider"] != "discord" {
        return in, nil
    }
    
    // Validate token with Discord API
    discordUser, err := validateDiscordToken(in.Account.Id) // token is passed as ID
    if err != nil {
        logger.Error("Discord validation failed: %v", err)
        return nil, runtime.NewError("Invalid Discord token", 16) // UNAUTHENTICATED
    }
    
    // Replace the token with Discord user ID (stable identifier)
    in.Account.Id = fmt.Sprintf("discord_%s", discordUser.ID)
    
    // Set username if not provided
    if in.Username == "" {
        in.Username = discordUser.Username
    }
    
    logger.Info("Discord auth for user: %s (%s)", discordUser.Username, discordUser.ID)
    return in, nil
}

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
```

---

## 5. Steam Authentication

### 5.1 Prerequisites

- Steam App ID (get from Steamworks partner portal)
- Steamworks SDK (for client)
- Steam Publisher Key (for Nakama server-side validation)

### 5.2 Flow

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

### 5.3 Client Implementation

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
        
        // Get encrypted app ticket
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
    
    /// Get Steam avatar URL (if available)
    pub fn avatar_url(&self) -> Option<String> {
        // Steam avatars are accessed differently, this is simplified
        let steam_id = self.client.user().steam_id();
        Some(format!(
            "https://avatars.steamstatic.com/{}_full.jpg",
            steam_id.raw()
        ))
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

### 5.4 Nakama Integration

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
        #[derive(Serialize)]
        struct Request {
            token: String,  // Hex-encoded ticket
        }
        
        let url = format!(
            "{}/v2/account/authenticate/steam?create={}&username={}",
            self.base_url,
            create,
            username.unwrap_or("")
        );
        
        let body = Request {
            token: hex::encode(ticket),
        };
        
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

### 5.5 Backend Configuration

```yaml
# backend/nakama/data/local.yml (add to existing)

steam:
  publisher_key: "${STEAM_PUBLISHER_KEY}"
  app_id: ${STEAM_APP_ID}
```

---

## 6. Remember Me (Session Persistence)

Uses OS-native secure storage via `keyring` crate.

### 6.1 Implementation

```rust
// client/src/auth/store.rs

use keyring::Entry;
use serde::{Deserialize, Serialize};

const SERVICE_NAME: &str = "mello";
const ENTRY_NAME: &str = "session";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredSession {
    pub user_id: String,
    pub username: String,
    pub token: String,
    pub refresh_token: String,
    pub expires_at: i64,
    pub provider: AuthProvider,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AuthProvider {
    Email,
    Discord,
    Steam,
}

pub struct AuthStore {
    entry: Entry,
}

impl AuthStore {
    pub fn new() -> Result<Self, AuthStoreError> {
        let entry = Entry::new(SERVICE_NAME, ENTRY_NAME)?;
        Ok(Self { entry })
    }
    
    /// Save session to OS secure storage
    pub fn save(&self, session: &StoredSession) -> Result<(), AuthStoreError> {
        let json = serde_json::to_string(session)?;
        self.entry.set_password(&json)?;
        log::info!("Session saved to secure storage");
        Ok(())
    }
    
    /// Load session from OS secure storage
    pub fn load(&self) -> Result<Option<StoredSession>, AuthStoreError> {
        match self.entry.get_password() {
            Ok(json) => {
                let session: StoredSession = serde_json::from_str(&json)?;
                Ok(Some(session))
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    
    /// Clear stored session (logout)
    pub fn clear(&self) -> Result<(), AuthStoreError> {
        match self.entry.delete_credential() {
            Ok(()) => {
                log::info!("Session cleared from secure storage");
                Ok(())
            }
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
    
    /// Check if session needs refresh (expires within 5 minutes)
    pub fn needs_refresh(session: &StoredSession) -> bool {
        let now = chrono::Utc::now().timestamp();
        session.expires_at < now + 300
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AuthStoreError {
    #[error("Keyring error: {0}")]
    Keyring(#[from] keyring::Error),
    
    #[error("Serialization error: {0}")]
    Json(#[from] serde_json::Error),
}
```

---

## 7. Auth Manager (Unified Interface)

```rust
// client/src/auth/mod.rs

mod discord;
mod steam;
mod store;

pub use discord::{DiscordAuth, DiscordAuthError};
pub use steam::{SteamAuth, SteamAuthError, SteamTicket};
pub use store::{AuthStore, StoredSession, AuthProvider};

use mello_core::nakama::Session;

pub struct AuthManager {
    store: AuthStore,
    steam: Option<SteamAuth>,
}

impl AuthManager {
    pub fn new() -> Result<Self, AuthError> {
        let store = AuthStore::new()?;
        
        // Try to init Steam (optional, won't fail if Steam not running)
        let steam = SteamAuth::init().ok();
        if steam.is_some() {
            log::info!("Steam SDK initialized");
        }
        
        Ok(Self { store, steam })
    }
    
    /// Check for saved session on startup
    pub async fn try_restore_session(&self, core: &mut MelloCore) -> Result<Option<Session>, AuthError> {
        let stored = match self.store.load()? {
            Some(s) => s,
            None => return Ok(None),
        };
        
        log::info!("Found stored session for {}", stored.username);
        
        // Try to restore/refresh with Nakama
        match core.nakama().restore_session(&stored.token, &stored.refresh_token).await {
            Ok(session) => {
                // Update stored session with new tokens
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
    
    /// Login with Discord
    pub async fn login_discord(&self, core: &mut MelloCore) -> Result<Session, AuthError> {
        let token = DiscordAuth::authenticate().await?;
        let session = core.nakama().authenticate_discord(&token, true, None).await?;
        
        // Always persist session
        self.store.save(&StoredSession {
            user_id: session.user_id.clone(),
            username: session.username.clone(),
            token: session.token.clone(),
            refresh_token: session.refresh_token.clone(),
            expires_at: session.expires_at,
            provider: AuthProvider::Discord,
        })?;
        
        Ok(session)
    }
    
    /// Login with Steam
    pub async fn login_steam(&self, core: &mut MelloCore) -> Result<Session, AuthError> {
        let steam = self.steam.as_ref().ok_or(AuthError::SteamNotAvailable)?;
        let ticket = steam.get_session_ticket()?;
        let session = core.nakama().authenticate_steam(&ticket.data, true, None).await?;
        
        // Always persist session
        self.store.save(&StoredSession {
            user_id: session.user_id.clone(),
            username: session.username.clone(),
            token: session.token.clone(),
            refresh_token: session.refresh_token.clone(),
            expires_at: session.expires_at,
            provider: AuthProvider::Steam,
        })?;
        
        Ok(session)
    }
    
    /// Login with email/password
    pub async fn login_email(
        &self,
        core: &mut MelloCore,
        email: &str,
        password: &str,
    ) -> Result<Session, AuthError> {
        let session = core.nakama().authenticate_email(email, password, false).await?;
        
        // Always persist session
        self.store.save(&StoredSession {
            user_id: session.user_id.clone(),
            username: session.username.clone(),
            token: session.token.clone(),
            refresh_token: session.refresh_token.clone(),
            expires_at: session.expires_at,
            provider: AuthProvider::Email,
        })?;
        
        Ok(session)
    }
    
    /// Logout
    pub fn logout(&self) -> Result<(), AuthError> {
        self.store.clear()?;
        Ok(())
    }
    
    /// Check if Steam is available
    pub fn steam_available(&self) -> bool {
        self.steam.is_some()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("Store error: {0}")]
    Store(#[from] store::AuthStoreError),
    
    #[error("Discord error: {0}")]
    Discord(#[from] DiscordAuthError),
    
    #[error("Steam error: {0}")]
    Steam(#[from] SteamAuthError),
    
    #[error("Steam not available")]
    SteamNotAvailable,
    
    #[error("Nakama error: {0}")]
    Nakama(#[from] mello_core::Error),
}
```

---

## 8. Dependencies

```toml
# client/Cargo.toml

[dependencies]
# Auth
keyring = "2"
webbrowser = "1.0"
tiny_http = "0.12"
urlencoding = "2"
hex = "0.4"

# Optional: Steam (feature-flagged)
steamworks = { version = "0.11", optional = true }

[features]
default = []
steam = ["steamworks"]
```

---

## 9. Environment Variables

```bash
# Required for Discord
DISCORD_CLIENT_ID=your_discord_app_id

# Required for Steam (production)
STEAM_PUBLISHER_KEY=your_steam_publisher_key
STEAM_APP_ID=your_steam_app_id

# Nakama
NAKAMA_URL=https://your-nakama.onrender.com
NAKAMA_SERVER_KEY=your_server_key
```

---

## 10. Security Considerations

| Concern | Mitigation |
|---------|------------|
| Token storage | OS-native secure storage (Credential Manager / Keychain) |
| Token in memory | Clear on logout, minimize lifetime |
| OAuth callback | Random high port, localhost only |
| Steam ticket replay | Nakama validates with Steam API |
| Discord token scope | `identify` only, no write access |

---

## 11. Testing Checklist

- [ ] Discord OAuth flow completes successfully
- [ ] Steam login works when Steam is running
- [ ] Steam login fails gracefully when Steam not running
- [ ] Email login still works
- [ ] Session automatically persists across restarts
- [ ] Logout clears stored credentials
- [ ] Session refresh works when token near expiry
- [ ] Invalid stored session is cleared
- [ ] Steam button hidden/disabled when Steam not available

---

*This spec covers authentication. For auto-updates, see [07-AUTO-UPDATER.md](./07-AUTO-UPDATER.md).*
