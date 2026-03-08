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

function Seed-EnsureGroup([string]$Token, [string]$Name, [string]$Desc) {
    $h = @{ Authorization = "Bearer $Token" }
    try {
        $g = Invoke-RestMethod -Uri "$BASE/v2/group" -Method Post -Headers $h `
            -Body (@{ name = $Name; description = $Desc; open = $true; max_count = 6 } | ConvertTo-Json) `
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

# ── users ────────────────────────────────────────────────────────

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
    @{ name = "Devs";   desc = "Development crew" },
    @{ name = "Gamers"; desc = "Gaming nights" },
    @{ name = "Music";  desc = "Music production" }
)

$crewIds = @{}
$crews | ForEach-Object {
    $c = $_
    $result = Seed-EnsureGroup $tok["alice"] $c.name $c.desc
    $crewIds[$c.name] = $result.id
    $tag = if ($result.created) { "new" } else { "exists" }
    Write-Host "    $($c.name)  $($result.id)  ($tag)" -ForegroundColor DarkGray
}

# ── memberships ──────────────────────────────────────────────────
#
#   Devs   : alice*, bob, charlie
#   Gamers : alice*, bob, diana
#   Music  : alice*, charlie, diana        (* = creator)

Write-Host ""
Write-Host "  memberships" -ForegroundColor Yellow

$joins = @(
    @{ user = "bob";     crew = "Devs" },
    @{ user = "charlie"; crew = "Devs" },
    @{ user = "bob";     crew = "Gamers" },
    @{ user = "diana";   crew = "Gamers" },
    @{ user = "charlie"; crew = "Music" },
    @{ user = "diana";   crew = "Music" }
)

$joins | ForEach-Object {
    $j = $_
    $ok = Seed-JoinGroup $tok[$j.user] $crewIds[$j.crew]
    $tag = if ($ok) { "joined" } else { "already in" }
    Write-Host "    $($j.user) -> $($j.crew)  ($tag)" -ForegroundColor DarkGray
}

# ── summary ──────────────────────────────────────────────────────

Write-Host ""
Write-Host "  done!" -ForegroundColor Green
Write-Host ""
Write-Host "  Credentials     <name>@test.com / $PASSWORD"
Write-Host "  Devs            alice, bob, charlie"
Write-Host "  Gamers          alice, bob, diana"
Write-Host "  Music           alice, charlie, diana"
Write-Host ""
