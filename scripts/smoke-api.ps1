<#
  Приёмка бэкенда Murmur: вход -> канал -> токен LiveKit.
  Запуск:  powershell -ExecutionPolicy Bypass -File scripts\smoke-api.ps1 -Name <ник> [-Api https://api.example.com] [-Invite]
  Пароль спрашивается в консоли (или из $env:MURMUR_PASSWORD). Токен LiveKit копируется в буфер обмена.
#>
param(
    [Parameter(Mandatory = $true)][string]$Name,
    [string]$Api = 'https://api.murmurchat.ru',
    [string]$Channel = 'General',
    [switch]$Invite
)
$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

function Call([string]$Method, [string]$Path, $Body = $null, [string]$Token = $null) {
    $h = @{}
    if ($Token) { $h['Authorization'] = "Bearer $Token" }
    $p = @{ Method = $Method; Uri = "$Api$Path"; Headers = $h; UseBasicParsing = $true }
    if ($null -ne $Body) {
        $p['ContentType'] = 'application/json; charset=utf-8'
        $p['Body'] = [Text.Encoding]::UTF8.GetBytes(($Body | ConvertTo-Json -Compress))
    }
    try { Invoke-RestMethod @p }
    catch {
        $msg = $_.ErrorDetails.Message; if (-not $msg) { $msg = $_.Exception.Message }
        throw "$Method $Path -> $msg"
    }
}

$health = Call GET '/health'
Write-Host "1. /health: ok=$($health.ok) version=$($health.version)" -ForegroundColor Green

$pw = $env:MURMUR_PASSWORD
if (-not $pw) {
    $sec = Read-Host "Пароль для $Name" -AsSecureString
    $pw = [Runtime.InteropServices.Marshal]::PtrToStringAuto([Runtime.InteropServices.Marshal]::SecureStringToBSTR($sec))
}
$auth = Call POST '/auth/login' @{ name = $Name; password = $pw }
$pw = $null
$t = $auth.token
Write-Host "2. Вход: $($auth.user.name) (id=$($auth.user.id), admin=$($auth.user.is_admin))" -ForegroundColor Green

$me = Call GET '/me' $null $t
Write-Host "3. /me: $($me.name)" -ForegroundColor Green

$chans = @(Call GET '/channels' $null $t)
$ch = $chans | Where-Object { $_.kind -eq 'voice' } | Select-Object -First 1
if (-not $ch) {
    $ch = Call POST '/channels' @{ name = $Channel; kind = 'voice' } $t
    Write-Host "4. Создан голосовой канал '$($ch.name)' (id=$($ch.id))" -ForegroundColor Green
} else {
    Write-Host "4. Голосовой канал '$($ch.name)' (id=$($ch.id)) уже есть" -ForegroundColor Green
}

$join = Call POST "/channels/$($ch.id)/join" $null $t
Set-Clipboard -Value $join.livekit_token
Write-Host "5. Токен LiveKit получен (комната $($join.room)) и скопирован в буфер обмена." -ForegroundColor Green
Write-Host "   https://meet.livekit.io -> вкладка Custom -> LiveKit URL: $($join.livekit_url) -> Token: Ctrl+V -> Connect"

if ($Invite) {
    $inv = Call POST '/invites' @{} $t
    Write-Host "6. Инвайт для друга: $($inv.code)" -ForegroundColor Green
}
