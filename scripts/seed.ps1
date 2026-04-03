#!/usr/bin/env pwsh
# Seed Mello backend with test users, crews, and memberships.
# Idempotent — safe to run multiple times.
#
# Usage:
#   .\seed.ps1                                       # local dev (default)
#   .\seed.ps1 -Base https://mello-nakama.onrender.com -ServerKey "YOUR_KEY"

param(
    [string]$Base      = "http://127.0.0.1:7350",
    [string]$ServerKey = "mello_dev_key",
    [string]$Password  = "password123"
)

$ErrorActionPreference = "Stop"

$BASE       = $Base
$SERVER_KEY = $ServerKey
$PASSWORD   = $Password

# ── helpers ──────────────────────────────────────────────────────

function Seed-Auth([string]$Email, [string]$Password, [string]$Username) {
    $b64 = [Convert]::ToBase64String([Text.Encoding]::ASCII.GetBytes("${SERVER_KEY}:"))
    $resp = Invoke-RestMethod `
        -Uri "$BASE/v2/account/authenticate/email?create=true&username=$Username" `
        -Method Post `
        -Headers @{ Authorization = "Basic $b64" } `
        -Body (@{ email = $Email; password = $Password } | ConvertTo-Json) `
        -ContentType "application/json"
    return $resp.token
}

function Seed-GetUserId([string]$Token) {
    $resp = Invoke-RestMethod -Uri "$BASE/v2/account" -Method Get `
        -Headers @{ Authorization = "Bearer $Token" }
    return $resp.user.id
}

function Seed-EnsureGroup([string]$Token, [string]$Name, [string]$Desc, [bool]$Open = $true) {
    $h = @{ Authorization = "Bearer $Token" }
    try {
        $g = Invoke-RestMethod -Uri "$BASE/v2/group" -Method Post -Headers $h `
            -Body (@{ name = $Name; description = $Desc; open = $Open; max_count = 6 } | ConvertTo-Json) `
            -ContentType "application/json"
        return @{ id = $g.id; created = $true }
    } catch {
        $r = Invoke-RestMethod -Uri "$BASE/v2/group?name=$Name&limit=100" -Method Get -Headers $h
        $match = $r.groups | Where-Object { $_.name -eq $Name } | Select-Object -First 1
        if ($match) { return @{ id = $match.id; created = $false } }
        throw "Cannot create or find group '$Name': $_"
    }
}

function Seed-JoinGroup([string]$Token, [string]$GroupId) {
    try {
        Invoke-RestMethod -Uri "$BASE/v2/group/$GroupId/join" `
            -Method Post -Headers @{ Authorization = "Bearer $Token" } `
            -ContentType "application/json" | Out-Null
        return $true
    } catch {
        return $false
    }
}

# ── preflight ────────────────────────────────────────────────────

Write-Host ""
Write-Host "  mello seed" -ForegroundColor Cyan
Write-Host "  ---------"
Write-Host ""

try {
    Invoke-RestMethod -Uri "$BASE/healthcheck" -Method Get -ErrorAction Stop | Out-Null
} catch {
    Write-Host "  [!] Nakama not reachable at $BASE" -ForegroundColor Red
    Write-Host "      Run: docker compose up -d  (from backend/)" -ForegroundColor DarkGray
    exit 1
}

# ── nuke existing accounts ────────────────────────────────────────
# Uses the Nakama console API (port 7351) to delete stale accounts
# so re-seeding works even if passwords changed.

$CONSOLE_BASE = $BASE -replace ':\d+$', ':7351'

Write-Host "  nuke" -ForegroundColor Yellow

try {
    $consoleAuth = Invoke-RestMethod -Uri "$CONSOLE_BASE/v2/console/authenticate" `
        -Method Post -ContentType "application/json" `
        -Body (@{ username = "admin"; password = "adminadmin" } | ConvertTo-Json)
    $ch = @{ Authorization = "Bearer $($consoleAuth.token)" }

    $accounts = Invoke-RestMethod -Uri "$CONSOLE_BASE/v2/console/account" `
        -Method Get -Headers $ch
    $list = @()
    if ($accounts.users) { $list = @($accounts.users) }
    $nuked = 0
    $list | ForEach-Object {
        $id   = $_.id
        $name = $_.username
        if ($id -eq "00000000-0000-0000-0000-000000000000") { return }
        Invoke-RestMethod -Uri "$CONSOLE_BASE/v2/console/account/$id" `
            -Method Delete -Headers $ch | Out-Null
        Write-Host "    deleted $name ($id)" -ForegroundColor DarkGray
        $nuked++
    }
    if ($nuked -eq 0) { Write-Host "    (no accounts to nuke)" -ForegroundColor DarkGray }
} catch {
    Write-Host "    [!] console nuke failed (non-fatal): $_" -ForegroundColor DarkGray
}

# ── users ────────────────────────────────────────────────────────

Write-Host ""
Write-Host "  users" -ForegroundColor Yellow

$names = @("alice", "bob", "charlie", "diana")
$tok = @{}; $uid = @{}

$names | ForEach-Object {
    $n = $_
    $tok[$n] = Seed-Auth "$n@test.com" $PASSWORD $n
    $uid[$n] = Seed-GetUserId $tok[$n]
    Write-Host "    $n  $($uid[$n])" -ForegroundColor DarkGray
}

# ── crews ────────────────────────────────────────────────────────

Write-Host ""
Write-Host "  crews" -ForegroundColor Yellow

$crews = @(
    @{ name = "Devs";    desc = "Development crew";                          open = $true },
    @{ name = "Gamers";  desc = "Gaming nights";                             open = $true },
    @{ name = "Music";   desc = "Music production";                          open = $true },
    @{ name = "Design";  desc = "UI/UX design and prototyping";              open = $true },
    @{ name = "Ops";     desc = "Infrastructure and deployment ops";         open = $true },
    @{ name = "Retro";   desc = "Retro hardware tinkering and emulation";    open = $true },
    @{ name = "Vault";   desc = "Private archive - invite only";             open = $false },
    @{ name = "Phantom"; desc = "Stealth ops - closed crew";                 open = $false }
)

$crewIds = @{}
$crews | ForEach-Object {
    $c = $_
    $result = Seed-EnsureGroup $tok["alice"] $c.name $c.desc $c.open
    $crewIds[$c.name] = $result.id
    $tag = if ($result.created) { "new" } else { "exists" }
    Write-Host "    $($c.name)  $($result.id)  ($tag)" -ForegroundColor DarkGray
}

# ── memberships ──────────────────────────────────────────────────
#
#   Devs     : alice*, bob, charlie
#   Gamers   : alice*, bob, diana
#   Music    : alice*, charlie, diana
#   Design   : alice*, bob, diana
#   Ops      : alice*, charlie
#   Retro    : alice*, bob, charlie, diana
#   Vault    : alice* (closed)
#   Phantom  : alice* (closed)              (* = creator)

Write-Host ""
Write-Host "  memberships" -ForegroundColor Yellow

$joins = @(
    @{ user = "bob";     crew = "Devs" },
    @{ user = "charlie"; crew = "Devs" },
    @{ user = "bob";     crew = "Gamers" },
    @{ user = "diana";   crew = "Gamers" },
    @{ user = "charlie"; crew = "Music" },
    @{ user = "diana";   crew = "Music" },
    @{ user = "bob";     crew = "Design" },
    @{ user = "diana";   crew = "Design" },
    @{ user = "charlie"; crew = "Ops" },
    @{ user = "bob";     crew = "Retro" },
    @{ user = "charlie"; crew = "Retro" },
    @{ user = "diana";   crew = "Retro" }
)

$joins | ForEach-Object {
    $j = $_
    $ok = Seed-JoinGroup $tok[$j.user] $crewIds[$j.crew]
    $tag = if ($ok) { "joined" } else { "already in" }
    Write-Host "    $($j.user) -> $($j.crew)  ($tag)" -ForegroundColor DarkGray
}

# ── dev state (presence, voice, streams, chat previews, events) ──

Write-Host ""
Write-Host "  dev state" -ForegroundColor Yellow

try {
    $b64 = [Convert]::ToBase64String([Text.Encoding]::ASCII.GetBytes("${SERVER_KEY}:"))
    $seedResp = Invoke-RestMethod `
        -Uri "$BASE/v2/rpc/dev_seed_state" `
        -Method Post `
        -Headers @{ Authorization = "Bearer $($tok['alice'])" } `
        -Body '""' `
        -ContentType "application/json"
    Write-Host "    $seedResp" -ForegroundColor DarkGray
} catch {
    Write-Host "    [!] dev_seed_state failed (backend may need rebuild)" -ForegroundColor Red
    Write-Host "    $_" -ForegroundColor DarkGray
}

# ── summary ──────────────────────────────────────────────────────

Write-Host ""
Write-Host "  done!" -ForegroundColor Green
Write-Host ""
Write-Host "  Credentials     <name>@test.com / $PASSWORD"
Write-Host "  Devs            alice, bob, charlie"
Write-Host "  Gamers          alice, bob, diana"
Write-Host "  Music           alice, charlie, diana"
Write-Host "  Design          alice, bob, diana"
Write-Host "  Ops             alice, charlie"
Write-Host "  Retro           alice, bob, charlie, diana"
Write-Host "  Vault           alice (closed)"
Write-Host "  Phantom         alice (closed)"
Write-Host ""
Write-Host "  Dev state (transient - rerun after backend restart):"
Write-Host "    alice:   online, in voice (Gamers)"
Write-Host "    bob:     online, in voice (Gamers), speaking"
Write-Host "    charlie: online, streaming CS2 in voice (Devs)"
Write-Host "    diana:   idle"
Write-Host "    Chat previews in Devs, Gamers, Music, Design, Retro"
Write-Host "    Crew events + stale last_seen in Gamers, Devs"
Write-Host ""
