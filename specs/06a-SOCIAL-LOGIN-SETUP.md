# MELLO Social Login — Developer Setup Guide

> **Purpose:** Step-by-step instructions for obtaining OAuth credentials from each provider  
> **Parent:** [06-SOCIAL-LOGIN.md](./06-SOCIAL-LOGIN.md)

---

## Table of Contents

1. [Steam](#1-steam)
2. [Google](#2-google)
3. [Twitch](#3-twitch)
4. [Discord](#4-discord)
5. [Apple](#5-apple)
6. [.env Reference](#6-env-reference)

---

## 1. Steam

### What you need

| Credential | Where it goes |
|------------|---------------|
| App ID | `STEAM_APP_ID` env var + `steam_appid.txt` in client working dir |
| Publisher Web API Key | `STEAM_PUBLISHER_KEY` env var (Nakama server-side) |

### Steps

1. **Create a Steamworks account**
   - Go to https://partner.steamgames.com
   - Pay the $100 app credit fee
   - Complete company/individual verification

2. **Create an application**
   - Steamworks → "Create New App"
   - Note the **App ID** (e.g. `480` for testing with Spacewar)

3. **Get the Publisher Web API Key**
   - Steamworks → Users & Permissions → "Manage Group" → "Generate Web API Key"
   - This is a secret — never ship it in the client

4. **Client setup**
   - Create `steam_appid.txt` in the client working directory containing just the App ID
   - For development, you can use Spacewar's App ID: `480`
   - Steam client must be running on the dev machine

### Dev shortcut

For local development without a real Steamworks account, use Spacewar (App ID `480`). Create `client/steam_appid.txt`:

```
480
```

This lets you test the Steam SDK integration. Nakama won't validate the ticket against the real Steam API in local mode.

---

## 2. Google

### What you need

| Credential | Where it goes |
|------------|---------------|
| OAuth 2.0 Client ID | `GOOGLE_CLIENT_ID` env var (client + backend) |
| OAuth 2.0 Client Secret | `GOOGLE_CLIENT_SECRET` env var (client only, compile-time) |

Google requires the client secret even for Desktop app clients. It is not truly secret (ships in the binary) — PKCE provides the actual security.

### Steps

1. **Go to Google Cloud Console**
   - https://console.cloud.google.com

2. **Create a project** (or select existing)
   - Click "Select a project" → "New Project"
   - Name: `mello` (or similar)

3. **Configure OAuth consent screen**
   - APIs & Services → OAuth consent screen
   - User type: **External**
   - App name: `Mello`
   - User support email: your email
   - Authorized domains: `localhost` (for dev)
   - Scopes: add `openid`, `profile`, `email`
   - Save

4. **Create OAuth 2.0 Client ID**
   - APIs & Services → Credentials → "Create Credentials" → "OAuth client ID"
   - Application type: **Desktop app**
   - Name: `Mello Desktop`
   - Click Create

5. **Copy the Client ID and Client Secret**
   - Client ID looks like: `123456789-abcdef.apps.googleusercontent.com`
   - Set as `GOOGLE_CLIENT_ID` in your `backend/.env` and `.cargo/config.toml`
   - Copy the Client Secret and set as `GOOGLE_CLIENT_SECRET` in `.cargo/config.toml`

### Notes

- In development, Google shows a "This app isn't verified" warning — that's normal
- For production, submit the app for verification through the consent screen settings
- The client secret is required by Google even for Desktop apps — it ships in the binary (PKCE provides the real security)

---

## 3. Twitch

### What you need

| Credential | Where it goes |
|------------|---------------|
| Client ID | `TWITCH_CLIENT_ID` env var (client + backend) |

No client secret needed — we use PKCE.

### Steps

1. **Go to Twitch Developer Console**
   - https://dev.twitch.tv/console

2. **Log in with your Twitch account**

3. **Register a new application**
   - "Register Your Application"
   - Name: `Mello`
   - OAuth Redirect URLs: `http://localhost:29405/callback`
   - Category: **Game Integration**
   - Click Create

4. **Copy the Client ID**
   - Click "Manage" on your app
   - Copy the **Client ID**
   - Set as `TWITCH_CLIENT_ID` in your `.env`

### Notes

- Twitch PKCE flow does not require a client secret
- The `Client-Id` header is required on all Twitch API calls (including token validation on the backend)
- For production, update the redirect URI to your production callback if needed

---

## 4. Discord

### What you need

| Credential | Where it goes |
|------------|---------------|
| Client ID (Application ID) | `DISCORD_CLIENT_ID` env var (client) |

### Steps

1. **Go to Discord Developer Portal**
   - https://discord.com/developers/applications

2. **Create a new application**
   - Click "New Application"
   - Name: `Mello`

3. **Configure OAuth2**
   - Left sidebar → OAuth2
   - Add redirect: `http://localhost:29405/callback`
   - Note the **Application ID** (same as Client ID) from the General Information page

4. **Copy the Client ID**
   - General Information → Application ID
   - Set as `DISCORD_CLIENT_ID` in your `.env`

### Notes

- We use the **implicit flow** (`response_type=token`), so no client secret is needed in the client
- Only the `identify` scope is requested — no access to user's servers, messages, etc.
- Discord is P1 (competitor) — don't give it prominent placement in the UI

---

## 5. Apple

### What you need

| Credential | Where it goes |
|------------|---------------|
| Services ID (Client ID) | `APPLE_CLIENT_ID` env var |
| Team ID | `APPLE_TEAM_ID` env var |
| Key ID | `APPLE_KEY_ID` env var |
| Private key (.p8) | Stored securely on the server |

### Prerequisites

- Apple Developer account ($99/year): https://developer.apple.com

### Steps

1. **Create an App ID**
   - Certificates, Identifiers & Profiles → Identifiers
   - Click "+" → "App IDs" → Continue
   - Platform: any
   - Description: `Mello`
   - Bundle ID: `dev.mollo.mello`
   - Capabilities: check "Sign in with Apple"
   - Click Continue → Register

2. **Create a Services ID**
   - Identifiers → "+" → "Services IDs" → Continue
   - Description: `Mello Web Auth`
   - Identifier: `dev.mollo.mello.auth`
   - Click Continue → Register
   - Click on the newly created Services ID
   - Enable "Sign in with Apple"
   - Click Configure:
     - Primary App ID: select your App ID from step 1
     - Domains: `localhost`
     - Return URLs: `http://localhost:29405/callback`
   - Save

3. **Create a Sign in with Apple key**
   - Keys → "+" → Name: `Mello Auth Key`
   - Check "Sign in with Apple" → Configure → select your Primary App ID
   - Continue → Register
   - **Download the .p8 file** (you can only download it once!)
   - Note the **Key ID**

4. **Get your Team ID**
   - Top right of the developer portal, or Membership → Team ID

5. **Set environment variables**
   ```
   APPLE_CLIENT_ID=dev.mollo.mello.auth    # Services ID
   APPLE_TEAM_ID=ABC123DEF4                 # Your team ID
   APPLE_KEY_ID=XYZ789KEY0                  # Key ID from step 3
   ```

6. **Deploy the .p8 key**
   - Store the private key securely on your server (never commit to git)
   - Nakama needs it for server-side token validation

### Notes

- Apple Sign In is **P1** — only needed for future App Store distribution
- Apple requires it when you offer other social logins on their platform
- The private key (.p8) must be kept secret and secure
- For local dev, skip Apple unless you're specifically testing it

---

## 6. .env Reference

Complete `.env` file with all provider credentials:

```bash
# =============================================================================
# Mello Backend Environment Variables
# =============================================================================

# --- Nakama (always required) ---
NAKAMA_HTTP_KEY=mello_http_key_dev

# --- Steam (P0, cloud only) ---
STEAM_PUBLISHER_KEY=                    # From Steamworks → Web API Key
STEAM_APP_ID=480                        # 480 = Spacewar (dev testing)

# --- Google (P0, cloud only) ---
GOOGLE_CLIENT_ID=                       # From Google Cloud Console

# --- Twitch (P0, cloud only) ---
TWITCH_CLIENT_ID=                       # From Twitch Developer Console

# --- Discord (P1, cloud only) ---
DISCORD_CLIENT_ID=                      # From Discord Developer Portal

# --- Apple (P1, cloud only) ---
APPLE_CLIENT_ID=                        # Services ID from Apple Developer
APPLE_TEAM_ID=                          # Team ID
APPLE_KEY_ID=                           # Key ID for the .p8 key

# --- TURN server (optional for local dev) ---
TURN_HOST=
TURN_USERNAME=
TURN_PASSWORD=
```

### What's required for local development?

Only `NAKAMA_HTTP_KEY`. All social login credentials are optional — the backend's `auth/providers` RPC will only advertise providers whose env vars are set. Without them, the client falls back to email/password only (same as self-hosted).

### What's required for production (cloud)?

All of the above, plus:
- The Apple `.p8` private key deployed to the server
- All `CLIENT_ID` values filled in
- `STEAM_APP_ID` set to your real app (not Spacewar)

---

*For the technical specification of each auth flow, see [06-SOCIAL-LOGIN.md](./06-SOCIAL-LOGIN.md).*
