# MELLO Backend Hosting Specification

> **Component:** Backend Infrastructure (Render.com)  
> **Version:** 0.2  
> **Status:** Planned  
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)

---

## 1. Overview

Mello's backend is hosted on Render.com for simplicity and cost-effectiveness during early testing phases (<100 users).

### Why Render?

| Factor | Render | GCP Cloud Run | Self-Hosted |
|--------|--------|---------------|-------------|
| **Complexity** | Very Low | Medium | High |
| **Cost (test)** | $7-14/mo | $9-14/mo | $5-20/mo |
| **Auto-deploy** | ✅ Built-in | ✅ With setup | ❌ Manual |
| **Managed DB** | ✅ PostgreSQL | ✅ Cloud SQL | ❌ Self-managed |
| **SSL** | ✅ Automatic | ✅ Automatic | ❌ Manual |
| **Scaling** | ✅ Easy | ✅ Auto | ❌ Manual |

---

## 2. Architecture

```
┌─────────────────────────────────────────────────────────────────────────┐
│                           RENDER.COM                                    │
│                                                                         │
│   ┌─────────────────────────────────────────────────────────────────┐   │
│   │                     Render Load Balancer                        │   │
│   │                          (SSL/TLS)                              │   │
│   │                                                                 │   │
│   │              https://mello-api.onrender.com                     │   │
│   └───────────────────────────┬─────────────────────────────────────┘   │
│                               │                                         │
│               ┌───────────────┴───────────────┐                         │
│               │                               │                         │
│               ▼                               ▼                         │
│   ┌───────────────────────┐       ┌───────────────────────┐            │
│   │   Nakama Web Service  │       │   PostgreSQL          │            │
│   │                       │       │   (Managed)           │            │
│   │   - Docker container  │──────▶│                       │            │
│   │   - 512MB RAM         │       │   - 256MB RAM         │            │
│   │   - $7/mo             │       │   - Free (90 days)    │            │
│   │                       │       │   - Then $7/mo        │            │
│   └───────────────────────┘       └───────────────────────┘            │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
                               │
                               │ WSS (WebSocket Secure)
                               │ HTTPS (REST API)
                               ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                          MELLO CLIENTS                                  │
│                                                                         │
│              Windows │ macOS │ Linux │ iOS │ Android                    │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

---

## 3. Render Services

### 3.1 Service Overview

| Service | Type | Plan | Cost |
|---------|------|------|------|
| **mello-nakama** | Web Service | Starter | $7/mo |
| **mello-db** | PostgreSQL | Free → Starter | $0 → $7/mo |

**Total:** $7/mo (first 90 days) → $14/mo after

### 3.2 Nakama Web Service

```yaml
# render.yaml (Infrastructure as Code)

services:
  - type: web
    name: mello-nakama
    runtime: docker
    repo: https://github.com/mollohq/mello
    branch: main
    rootDir: backend
    dockerfilePath: ./Dockerfile
    plan: starter  # $7/mo, 512MB RAM
    healthCheckPath: /healthcheck
    envVars:
      - key: NAKAMA_CONSOLE_USERNAME
        value: admin
      - key: NAKAMA_CONSOLE_PASSWORD
        generateValue: true  # Auto-generate secure password
      - key: NAKAMA_SERVER_KEY
        generateValue: true
      - key: NAKAMA_HTTP_KEY
        generateValue: true
      - key: DATABASE_URL
        fromDatabase:
          name: mello-db
          property: connectionString
      - key: DISCORD_CLIENT_ID
        sync: false  # Set manually
      - key: DISCORD_CLIENT_SECRET
        sync: false  # Set manually
      - key: STEAM_PUBLISHER_KEY
        sync: false  # Set manually (optional)
      - key: STEAM_APP_ID
        sync: false  # Set manually (optional)

databases:
  - name: mello-db
    plan: free  # 256MB, 90 days free
    databaseName: nakama
    user: nakama
```

---

## 4. Dockerfile

```dockerfile
# backend/Dockerfile

FROM heroiclabs/nakama:3.21.0

# Copy Nakama configuration
COPY nakama/data/nakama.yml /nakama/data/nakama.yml

# Copy custom server modules
COPY nakama/data/modules /nakama/data/modules

# Expose ports
# Render automatically routes to the PORT env var
EXPOSE 7350

# Health check
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -f http://localhost:7350/healthcheck || exit 1

# Start Nakama
# Render provides DATABASE_URL, we need to parse it
ENTRYPOINT ["/bin/sh", "-c", "\
    /nakama/nakama migrate up \
        --database.address=${DATABASE_URL} && \
    exec /nakama/nakama \
        --config /nakama/data/nakama.yml \
        --database.address=${DATABASE_URL} \
        --socket.server_key=${NAKAMA_SERVER_KEY:-defaultkey} \
        --runtime.http_key=${NAKAMA_HTTP_KEY:-defaulthttpkey} \
        --console.username=${NAKAMA_CONSOLE_USERNAME:-admin} \
        --console.password=${NAKAMA_CONSOLE_PASSWORD:-admin} \
        --console.port=7351 \
        --socket.port=7350 \
    "]
```

---

## 5. Nakama Configuration

```yaml
# backend/nakama/data/nakama.yml

name: mello

logger:
  level: INFO
  stdout: true
  format: json

session:
  token_expiry_sec: 86400        # 24 hours
  refresh_token_expiry_sec: 604800  # 7 days

socket:
  # Port is set via CLI args
  max_message_size_bytes: 4096
  max_request_size_bytes: 131072
  read_buffer_size_bytes: 4096
  write_buffer_size_bytes: 4096
  read_timeout_ms: 10000
  write_timeout_ms: 10000
  pong_wait_ms: 25000
  ping_period_ms: 15000
  ping_backoff_threshold: 20

runtime:
  path: /nakama/data/modules
  
match:
  input_queue_size: 128
  call_queue_size: 128
  join_attempt_queue_size: 128
  deferred_queue_size: 128

tracker:
  event_queue_size: 1024
```

---

## 6. Deployment

### 6.1 Initial Setup (One-time)

1. **Create Render account:** https://render.com

2. **Connect GitHub:**
   - Go to Dashboard → Account Settings → Git Providers
   - Connect GitHub, authorize mollohq/mello repo

3. **Create Blueprint:**
   - Dashboard → New → Blueprint
   - Select repository: mollohq/mello
   - Render reads `render.yaml` automatically

4. **Set Environment Variables:**
   - Go to mello-nakama service → Environment
   - Add required secrets:
     - `DISCORD_CLIENT_ID`
     - `DISCORD_CLIENT_SECRET`
     - `STEAM_PUBLISHER_KEY` (optional)
     - `STEAM_APP_ID` (optional)

5. **Deploy:**
   - Click "Manual Deploy" or push to main branch

### 6.2 Auto-Deploy (Continuous)

After initial setup, every push to `main` triggers:

```
┌─────────────┐     ┌──────────────┐     ┌─────────────┐
│  Git Push   │────▶│ Render Build │────▶│   Deploy    │
│  (main)     │     │  (Docker)    │     │  (Rolling)  │
└─────────────┘     └──────────────┘     └─────────────┘
                          │
                          ▼
                    Health Check
                          │
                    ┌─────┴─────┐
                    │           │
                    ▼           ▼
                 Success     Failure
                    │           │
                    ▼           ▼
               Live Traffic  Rollback
```

### 6.3 Manual Deploy

```bash
# Trigger deploy via Render CLI (optional)
render deploy --service mello-nakama

# Or via API
curl -X POST https://api.render.com/v1/services/{service_id}/deploys \
  -H "Authorization: Bearer ${RENDER_API_KEY}"
```

---

## 7. Environment Configuration

### 7.1 Development (Local)

```bash
# backend/.env.local

# Database
DATABASE_URL=postgres://nakama:localdev@localhost:5432/nakama

# Nakama
NAKAMA_SERVER_KEY=mello_dev_key
NAKAMA_HTTP_KEY=mello_http_key
NAKAMA_CONSOLE_USERNAME=admin
NAKAMA_CONSOLE_PASSWORD=admin

# Social (optional for local)
DISCORD_CLIENT_ID=
DISCORD_CLIENT_SECRET=
```

### 7.2 Production (Render)

Set these in Render Dashboard → mello-nakama → Environment:

| Variable | Source | Notes |
|----------|--------|-------|
| `DATABASE_URL` | Auto (from mello-db) | Render injects this |
| `NAKAMA_SERVER_KEY` | Auto-generated | Used by client to connect |
| `NAKAMA_HTTP_KEY` | Auto-generated | Used for RPC calls |
| `NAKAMA_CONSOLE_USERNAME` | Manual | Admin console login |
| `NAKAMA_CONSOLE_PASSWORD` | Auto-generated | Admin console login |
| `DISCORD_CLIENT_ID` | Manual | From Discord Developer Portal |
| `DISCORD_CLIENT_SECRET` | Manual | From Discord Developer Portal |
| `STEAM_PUBLISHER_KEY` | Manual | From Steamworks (optional) |
| `STEAM_APP_ID` | Manual | From Steamworks (optional) |

---

## 8. Client Configuration

### 8.1 Config File

```rust
// mello-core/src/config.rs

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NakamaConfig {
    pub host: String,
    pub port: u16,
    pub server_key: String,
    pub use_ssl: bool,
}

impl NakamaConfig {
    /// Production configuration (Render)
    pub fn production() -> Self {
        Self {
            host: "mello-api.onrender.com".into(),
            port: 443,
            server_key: env!("NAKAMA_SERVER_KEY").into(),
            use_ssl: true,
        }
    }
    
    /// Development configuration (local Docker)
    pub fn development() -> Self {
        Self {
            host: "localhost".into(),
            port: 7350,
            server_key: "mello_dev_key".into(),
            use_ssl: false,
        }
    }
}
```

### 8.2 Build-time Configuration

```toml
# client/Cargo.toml

[features]
default = ["production"]
production = []
development = []
```

```rust
// client/src/main.rs

fn nakama_config() -> NakamaConfig {
    #[cfg(feature = "development")]
    return NakamaConfig::development();
    
    #[cfg(feature = "production")]
    return NakamaConfig::production();
}
```

```bash
# Development build
cargo run --features development

# Production build
cargo build --release --features production
```

---

## 9. URLs and Endpoints

### 9.1 Production URLs

| Endpoint | URL |
|----------|-----|
| WebSocket | `wss://mello-api.onrender.com/ws` |
| REST API | `https://mello-api.onrender.com/v2` |
| Admin Console | `https://mello-api.onrender.com` (port 7351 proxied) |
| Health Check | `https://mello-api.onrender.com/healthcheck` |

### 9.2 Custom Domain (Optional)

1. Add custom domain in Render Dashboard
2. Add CNAME record: `api.mello.app → mello-api.onrender.com`
3. SSL certificate auto-provisioned

---

## 10. Monitoring

### 10.1 Built-in (Render Dashboard)

- **Logs:** Real-time log streaming
- **Metrics:** CPU, Memory, Network
- **Events:** Deploy history, restarts
- **Alerts:** Email/Slack on failures

### 10.2 Health Check Endpoint

```go
// backend/nakama/data/modules/health.go

func HealthCheckRPC(ctx context.Context, logger runtime.Logger, db *sql.DB, nk runtime.NakamaModule, payload string) (string, error) {
    // Check database connectivity
    if err := db.PingContext(ctx); err != nil {
        return "", runtime.NewError("database unhealthy", 13)
    }
    
    return `{"status":"healthy","version":"0.2.0"}`, nil
}
```

### 10.3 External Monitoring (Optional)

For production, consider adding:

- **UptimeRobot:** Free uptime monitoring
- **Sentry:** Error tracking (Nakama has Sentry integration)

---

## 11. Backup Strategy

### 11.1 Automatic Backups (Render PostgreSQL)

- **Starter plan:** Daily snapshots, 7-day retention
- **Recovery:** Restore from dashboard or API

### 11.2 Manual Backup

```bash
# Export database (run locally)
pg_dump $DATABASE_URL > backup_$(date +%Y%m%d).sql

# Or via Render CLI
render db backup mello-db
```

---

## 12. Scaling

### 12.1 Vertical Scaling (Current)

Upgrade service plan in Render Dashboard:

| Plan | RAM | CPU | Cost |
|------|-----|-----|------|
| Starter | 512MB | 0.5 | $7/mo |
| Standard | 2GB | 1.0 | $25/mo |
| Pro | 4GB | 2.0 | $85/mo |

### 12.2 Horizontal Scaling (Future)

When >1000 concurrent users:

1. Upgrade to Pro plan
2. Enable auto-scaling (Pro feature)
3. Add Redis for session sharing (if needed)
4. Consider dedicated PostgreSQL

---

## 13. Cost Projection

| Users | Plan | DB Plan | Cost |
|-------|------|---------|------|
| 0-100 | Starter | Free | **$7/mo** |
| 0-100 | Starter | Starter | **$14/mo** |
| 100-500 | Standard | Starter | **$32/mo** |
| 500-1000 | Pro | Standard | **$110/mo** |
| 1000+ | Pro + Auto-scale | Pro | **$200+/mo** |

---

## 14. Migration Path

When ready to scale beyond Render:

### Option A: GCP (Recommended for scale)

```
Render Nakama       →  GCP Cloud Run (Nakama)
Render PostgreSQL   →  GCP Cloud SQL
                    +  GCP Memorystore (Redis)
```

### Option B: Heroic Cloud (Managed Nakama)

```
Render Nakama       →  Heroic Cloud (fully managed)
Render PostgreSQL   →  Heroic Cloud (included)
```

### Migration Steps

1. Export data from Render PostgreSQL
2. Import to new database
3. Update client config (new hostname)
4. Release client update
5. Monitor, then decommission Render

---

## 15. Security Checklist

- [x] HTTPS/WSS enforced (Render default)
- [x] Auto-managed SSL certificates
- [x] Secrets in environment variables (not code)
- [x] Server key rotatable
- [x] Console password auto-generated
- [ ] Rate limiting (configure in Nakama)
- [ ] IP allowlist (Render Pro feature)

---

## 16. Deployment Checklist

### Initial Deploy

- [ ] Create Render account
- [ ] Connect GitHub repo
- [ ] Create Blueprint from `render.yaml`
- [ ] Wait for PostgreSQL provisioning (~2 min)
- [ ] Set manual environment variables
- [ ] Trigger first deploy
- [ ] Verify health check passes
- [ ] Test WebSocket connection
- [ ] Test REST API
- [ ] Save admin console password

### Each Release

- [ ] Push to main branch
- [ ] Watch deploy logs in Render
- [ ] Verify health check
- [ ] Test one client connection
- [ ] Monitor for errors (10 min)

---

## 17. Troubleshooting

### Service Won't Start

```bash
# Check logs
render logs mello-nakama --tail 100

# Common issues:
# - DATABASE_URL not set → Check env vars
# - Port mismatch → Nakama must listen on $PORT
# - Migration failed → Check DB connectivity
```

### WebSocket Connection Fails

```bash
# Test WebSocket
wscat -c wss://mello-api.onrender.com/ws

# Common issues:
# - SSL not enabled in client
# - Wrong port (should be 443 for wss://)
# - Server key mismatch
```

### Database Connection Issues

```bash
# Check DB status in Render Dashboard
# Check DATABASE_URL format:
# postgres://user:pass@host:port/db?sslmode=require
```

---

## 18. Files to Create

```
backend/
├── Dockerfile                    # Nakama container
├── render.yaml                   # Render Blueprint
├── nakama/
│   └── data/
│       ├── nakama.yml            # Nakama config
│       └── modules/
│           ├── main.go           # Entry point
│           ├── auth.go           # Discord validation
│           └── health.go         # Health check RPC
```

---

*This spec covers backend hosting on Render.com. For complete architecture, see [00-ARCHITECTURE.md](./00-ARCHITECTURE.md).*
