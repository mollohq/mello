# Shared helpers for stream-host / stream-viewer probe scripts.
# Dot-source from run-stream-host.ps1 and run-stream-viewer.ps1.

$script:StreamProbeStateFile = Join-Path $env:TEMP "mello-stream-probe.state.json"

$script:StreamProbeProfiles = @{
    local = @{
        NakamaHttpBase   = "http://127.0.0.1:7350"
        NakamaServerKey  = "mello_dev_key"
        ConsoleBase      = "http://127.0.0.1:7351"
        ConsoleUser      = "admin"
        ConsolePassword  = "adminadmin"
        DefaultCrewName  = "Devs"
        SeedEmail        = "alice@test.com"
        SeedPassword     = "password123"
        SeedUsername     = "alice"
        DeviceId         = "mello-stream-probe"
        SfuHealthUrl     = "http://127.0.0.1:8080/health"
    }
    remote = @{
        NakamaHttpBase   = "https://mello-api-1iiv.onrender.com"
        NakamaServerKey  = $null
        ConsoleBase      = $null
        DefaultCrewName  = $null
        SeedEmail        = $null
        SeedPassword     = $null
        SeedUsername     = $null
        DeviceId         = "mello-stream-probe"
        SfuHealthUrl     = $null
    }
}

function Get-StreamProbeProfileName([bool]$Remote) {
    if ($Remote) { return "remote" }
    return "local"
}

function Get-StreamProbeProfile([bool]$Remote, [string]$NakamaHttpBase, [string]$ServerKey) {
    $name = Get-StreamProbeProfileName $Remote
    $profile = @{} + $script:StreamProbeProfiles[$name]

    if (-not [string]::IsNullOrWhiteSpace($NakamaHttpBase)) {
        $profile.NakamaHttpBase = $NakamaHttpBase.TrimEnd("/")
    }

    if (-not [string]::IsNullOrWhiteSpace($ServerKey)) {
        $profile.NakamaServerKey = $ServerKey
    }
    elseif ($Remote -and -not [string]::IsNullOrWhiteSpace($env:NAKAMA_SERVER_KEY)) {
        $profile.NakamaServerKey = $env:NAKAMA_SERVER_KEY
    }

    if ($Remote -and [string]::IsNullOrWhiteSpace($profile.NakamaServerKey)) {
        throw "Remote profile requires NAKAMA_SERVER_KEY env var or -ServerKey."
    }

    return [pscustomobject]$profile
}

function Get-BasicAuthHeader([string]$ServerKey) {
    $encoded = [Convert]::ToBase64String([Text.Encoding]::ASCII.GetBytes("${ServerKey}:"))
    return @{ Authorization = "Basic $encoded" }
}

function Test-NakamaReachable([string]$Base) {
    try {
        Invoke-RestMethod -Uri "$Base/healthcheck" -Method Get -TimeoutSec 5 | Out-Null
        return $true
    }
    catch {
        return $false
    }
}

function Test-LocalSfuReachable([string]$HealthUrl) {
    if ([string]::IsNullOrWhiteSpace($HealthUrl)) {
        return $true
    }
    try {
        Invoke-RestMethod -Uri $HealthUrl -Method Get -TimeoutSec 3 | Out-Null
        return $true
    }
    catch {
        return $false
    }
}

function Get-NakamaEmailToken(
    [string]$Base,
    [string]$ServerKey,
    [string]$Email,
    [string]$Password,
    [string]$Username
) {
    $headers = Get-BasicAuthHeader $ServerKey
    $headers["Content-Type"] = "application/json"
    $resp = Invoke-RestMethod `
        -Uri "$Base/v2/account/authenticate/email?create=true&username=$Username" `
        -Method Post `
        -Headers $headers `
        -Body (@{ email = $Email; password = $Password } | ConvertTo-Json)
    return $resp.token
}

function Get-NakamaDeviceToken(
    [string]$Base,
    [string]$ServerKey,
    [string]$DeviceId
) {
    $headers = Get-BasicAuthHeader $ServerKey
    $headers["Content-Type"] = "application/json"
    $resp = Invoke-RestMethod `
        -Uri "$Base/v2/account/authenticate/device?create=true" `
        -Method Post `
        -Headers $headers `
        -Body (@{ id = $DeviceId } | ConvertTo-Json)
    return $resp.token
}

function Get-NakamaBearerHeaders([string]$Token) {
    return @{ Authorization = "Bearer $Token"; "Content-Type" = "application/json" }
}

function Get-ConsoleAuthHeaders([pscustomobject]$Profile) {
    if ([string]::IsNullOrWhiteSpace($Profile.ConsoleBase)) {
        return $null
    }
    $resp = Invoke-RestMethod `
        -Uri "$($Profile.ConsoleBase)/v2/console/authenticate" `
        -Method Post `
        -ContentType "application/json" `
        -Body (@{
            username = $Profile.ConsoleUser
            password = $Profile.ConsolePassword
        } | ConvertTo-Json)
    return @{ Authorization = "Bearer $($resp.token)"; "Content-Type" = "application/json" }
}

function Get-ConsoleGroupByName([hashtable]$ConsoleHeaders, [string]$ConsoleBase, [string]$Name) {
    $resp = Invoke-RestMethod `
        -Uri "$ConsoleBase/v2/console/group?limit=100" `
        -Method Get `
        -Headers $ConsoleHeaders
    return $resp.groups | Where-Object { $_.name -eq $Name } | Select-Object -First 1
}

function Invoke-NakamaRpc([string]$Base, [string]$Token, [string]$RpcId, [hashtable]$Payload) {
    $headers = Get-NakamaBearerHeaders $Token
    $inner = $Payload | ConvertTo-Json -Compress
    $body = $inner | ConvertTo-Json
    $resp = Invoke-RestMethod `
        -Uri "$Base/v2/rpc/$RpcId" `
        -Method Post `
        -Headers $headers `
        -Body $body
    if ($resp.payload) {
        return $resp.payload | ConvertFrom-Json
    }
    return $resp
}

function Enable-SfuCrewViaRpc([string]$Base, [string]$Token, [string]$CrewId) {
    try {
        Invoke-NakamaRpc $Base $Token "dev_enable_sfu_crew" @{ crew_id = $CrewId } | Out-Null
        return $true
    }
    catch {
        return $_
    }
}

function Test-CrewSfuEnabled([string]$Base, [string]$Token, [string]$CrewId) {
    $headers = Get-NakamaBearerHeaders $Token
    $resp = Invoke-RestMethod -Uri "$Base/v2/group?limit=100" -Method Get -Headers $headers
    $group = $resp.groups | Where-Object { $_.id -eq $CrewId } | Select-Object -First 1
    if (-not $group) {
        return $false
    }
    if ([string]::IsNullOrWhiteSpace($group.metadata) -or $group.metadata -eq "{}") {
        return $false
    }
    $meta = $group.metadata | ConvertFrom-Json
    return [bool]$meta.sfu_enabled
}

function Get-UserGroupByName([string]$Base, [string]$Token, [string]$Name) {
    $headers = Get-NakamaBearerHeaders $Token
    $resp = Invoke-RestMethod -Uri "$Base/v2/group?name=$Name&limit=100" -Method Get -Headers $headers
    return $resp.groups | Where-Object { $_.name -eq $Name } | Select-Object -First 1
}

function Join-UserGroup([string]$Base, [string]$Token, [string]$GroupId) {
    $headers = Get-NakamaBearerHeaders $Token
    try {
        Invoke-RestMethod -Uri "$Base/v2/group/$GroupId/join" -Method Post -Headers $headers -Body "{}" | Out-Null
        return $true
    }
    catch {
        return $false
    }
}

function New-UserGroup([string]$Base, [string]$Token, [string]$Name, [string]$Description) {
    $headers = Get-NakamaBearerHeaders $Token
    return Invoke-RestMethod `
        -Uri "$Base/v2/group" `
        -Method Post `
        -Headers $headers `
        -Body (@{
            name        = $Name
            description = $Description
            open        = $true
            max_count   = 6
        } | ConvertTo-Json)
}

function Resolve-StreamProbeAuthToken(
    [pscustomobject]$Profile,
    [string]$AuthToken,
    [switch]$RefreshAuth
) {
    if (-not $RefreshAuth -and -not [string]::IsNullOrWhiteSpace($AuthToken)) {
        return $AuthToken
    }
    if (-not $RefreshAuth -and -not [string]::IsNullOrWhiteSpace($env:MELLO_NAKAMA_AUTH_TOKEN)) {
        return $env:MELLO_NAKAMA_AUTH_TOKEN
    }

    if ($Profile.PSObject.Properties.Name -contains "SeedEmail" -and $Profile.SeedEmail) {
        try {
            Write-Host "  auth: $($Profile.SeedEmail) (local seed user)" -ForegroundColor DarkGray
            return Get-NakamaEmailToken `
                $Profile.NakamaHttpBase `
                $Profile.NakamaServerKey `
                $Profile.SeedEmail `
                $Profile.SeedPassword `
                $Profile.SeedUsername
        }
        catch {
            Write-Host "  auth: seed user unavailable ($($Profile.SeedEmail)); trying device auth" -ForegroundColor Yellow
        }
    }

    Write-Host "  auth: device $($Profile.DeviceId)" -ForegroundColor DarkGray
    return Get-NakamaDeviceToken $Profile.NakamaHttpBase $Profile.NakamaServerKey $Profile.DeviceId
}

function Ensure-SfuEnabledOnCrew([bool]$Local, [string]$Base, [string]$Token, [string]$CrewId, [string]$CrewLabel) {
    if (-not $Local) {
        if (-not (Test-CrewSfuEnabled $Base $Token $CrewId)) {
            Write-Host "  warning: crew may not have sfu_enabled (remote — set in Nakama console)" -ForegroundColor Yellow
        }
        return
    }

    if (Test-CrewSfuEnabled $Base $Token $CrewId) {
        Write-Host "  crew: $CrewLabel ($CrewId) sfu_enabled=true" -ForegroundColor DarkGray
        return
    }

    $result = Enable-SfuCrewViaRpc $Base $Token $CrewId
    if ($result -is [bool] -and $result) {
        if (Test-CrewSfuEnabled $Base $Token $CrewId) {
            Write-Host "  crew: $CrewLabel ($CrewId) sfu_enabled=true" -ForegroundColor DarkGray
            return
        }
    }

    $detail = if ($result -is [bool]) { "metadata still missing sfu_enabled" } else { $result.Exception.Message }
    throw @"
Could not enable SFU for crew '$CrewLabel' ($CrewId).
Rebuild Nakama so dev_enable_sfu_crew RPC is available, then retry:
  cd backend
  docker compose up -d --build nakama
Error: $detail
"@
}

function Resolve-StreamProbeCrewId(
    [pscustomobject]$Profile,
    [string]$Token,
    [string]$CrewId,
    [string]$CrewName,
    [bool]$Local
) {
    $resolvedId = $null
    $resolvedLabel = $null

    if (-not [string]::IsNullOrWhiteSpace($CrewId)) {
        $resolvedId = $CrewId
        $resolvedLabel = $CrewId
    }
    elseif (-not [string]::IsNullOrWhiteSpace($env:MELLO_CREW_ID)) {
        $resolvedId = $env:MELLO_CREW_ID
        $resolvedLabel = $env:MELLO_CREW_ID
    }
    elseif ($Profile.PSObject.Properties.Name -contains "DefaultCrewName" -and $Profile.DefaultCrewName) {
        $targetName = if ([string]::IsNullOrWhiteSpace($CrewName)) { $Profile.DefaultCrewName } else { $CrewName }
        $group = Get-UserGroupByName $Profile.NakamaHttpBase $Token $targetName
        if ($group) {
            $resolvedId = $group.id
            $resolvedLabel = $group.name
        }
        elseif ($Profile.ConsoleBase) {
            $consoleHeaders = Get-ConsoleAuthHeaders $Profile
            $existing = Get-ConsoleGroupByName $consoleHeaders $Profile.ConsoleBase $targetName
            if ($existing) {
                Join-UserGroup $Profile.NakamaHttpBase $Token $existing.id | Out-Null
                $resolvedId = $existing.id
                $resolvedLabel = $existing.name
            }
        }

        if (-not $resolvedId) {
            $created = New-UserGroup $Profile.NakamaHttpBase $Token $targetName "Stream probe crew"
            $resolvedId = $created.id
            $resolvedLabel = $targetName
        }
    }
    else {
        throw "Remote profile requires -CrewId or MELLO_CREW_ID."
    }

    Ensure-SfuEnabledOnCrew $Local $Profile.NakamaHttpBase $Token $resolvedId $resolvedLabel
    return $resolvedId
}

function Save-StreamProbeState(
    [string]$ProfileName,
    [pscustomobject]$Profile,
    [string]$AuthToken,
    [string]$CrewId
) {
    $state = @{
        profile        = $ProfileName
        nakamaHttpBase = $Profile.NakamaHttpBase
        authToken      = $AuthToken
        crewId         = $CrewId
        savedAt        = (Get-Date).ToString("o")
    }
    $state | ConvertTo-Json | Set-Content -Path $script:StreamProbeStateFile -Encoding UTF8
}

function Get-StreamProbeState() {
    if (-not (Test-Path $script:StreamProbeStateFile)) {
        return $null
    }
    try {
        return Get-Content -Path $script:StreamProbeStateFile -Raw | ConvertFrom-Json
    }
    catch {
        return $null
    }
}

function Initialize-StreamProbeContext {
    param(
        [switch]$Remote,
        [string]$NakamaHttpBase,
        [string]$ServerKey,
        [string]$AuthToken,
        [string]$CrewId,
        [string]$CrewName,
        [switch]$RefreshAuth,
        [switch]$SkipSfuCheck
    )

    $profileName = Get-StreamProbeProfileName $Remote
    $profile = Get-StreamProbeProfile $Remote $NakamaHttpBase $ServerKey

    Write-Host ""
    Write-Host "  stream probe ($profileName)" -ForegroundColor Cyan
    Write-Host "  nakama: $($profile.NakamaHttpBase)" -ForegroundColor DarkGray

    if (-not (Test-NakamaReachable $profile.NakamaHttpBase)) {
        if ($profileName -eq "local") {
            throw "Local Nakama not reachable at $($profile.NakamaHttpBase). Run: cd backend; docker compose up -d"
        }
        throw "Nakama not reachable at $($profile.NakamaHttpBase)."
    }

    if (-not $SkipSfuCheck -and $profileName -eq "local" -and -not (Test-LocalSfuReachable $profile.SfuHealthUrl)) {
        Write-Host "  warning: local SFU health check failed ($($profile.SfuHealthUrl))" -ForegroundColor Yellow
        Write-Host "           start SFU: `$env:SFU_PUBLIC_IP='127.0.0.1'; `$env:SFU_ADMIN_PASSWORD='devpass'; go run ./cmd/sfu" -ForegroundColor DarkGray
    }

    $token = Resolve-StreamProbeAuthToken $profile $AuthToken $RefreshAuth
    $resolvedCrewId = Resolve-StreamProbeCrewId $profile $token $CrewId $CrewName (-not $Remote)

    Save-StreamProbeState $profileName $profile $token $resolvedCrewId

    return [pscustomobject]@{
        ProfileName    = $profileName
        NakamaHttpBase = $profile.NakamaHttpBase
        AuthToken      = $token
        CrewId         = $resolvedCrewId
    }
}

function Initialize-StreamProbeViewerContext {
    param(
        [switch]$Remote,
        [string]$NakamaHttpBase,
        [string]$ServerKey,
        [string]$AuthToken,
        [switch]$RefreshAuth
    )

    $profileName = Get-StreamProbeProfileName $Remote
    $profile = Get-StreamProbeProfile $Remote $NakamaHttpBase $ServerKey
    $state = Get-StreamProbeState

    Write-Host ""
    Write-Host "  stream probe viewer ($profileName)" -ForegroundColor Cyan
    Write-Host "  nakama: $($profile.NakamaHttpBase)" -ForegroundColor DarkGray

    if (-not $RefreshAuth -and $state -and $state.profile -eq $profileName -and $state.authToken) {
        if ([string]::IsNullOrWhiteSpace($NakamaHttpBase)) {
            $profile.NakamaHttpBase = $state.nakamaHttpBase
        }
        Write-Host "  auth: reused saved token from host run" -ForegroundColor DarkGray
        return [pscustomobject]@{
            ProfileName    = $profileName
            NakamaHttpBase = $profile.NakamaHttpBase
            AuthToken      = $state.authToken
        }
    }

    if (-not (Test-NakamaReachable $profile.NakamaHttpBase)) {
        if ($profileName -eq "local") {
            throw "Local Nakama not reachable at $($profile.NakamaHttpBase). Run host first, or: cd backend; docker compose up -d"
        }
        throw "Nakama not reachable at $($profile.NakamaHttpBase)."
    }

    $token = Resolve-StreamProbeAuthToken $profile $AuthToken $RefreshAuth
    return [pscustomobject]@{
        ProfileName    = $profileName
        NakamaHttpBase = $profile.NakamaHttpBase
        AuthToken      = $token
    }
}
