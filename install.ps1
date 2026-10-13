<#
.SYNOPSIS
    Installs the Impactor MCP server (impactor-mcp) and registers it with Claude Code /
    Claude Desktop when present. Any other MCP client can point at the installed binary.

.DESCRIPTION
    Works alongside the stock Impactor app - nothing about the app is modified.

    Run straight from the network (nothing to clone):
        irm https://raw.githubusercontent.com/YeetDisDude/impactor-mcp/main/install.ps1 | iex
    With options:
        & ([scriptblock]::Create((irm https://raw.githubusercontent.com/YeetDisDude/impactor-mcp/main/install.ps1))) -AutoRefresh

    1. Downloads the prebuilt binary for this machine from the latest GitHub release.
       If there is none (or -FromSource), builds it with cargo (installing Rust via winget if needed).
       Source builds use the local checkout when run from one.
    2. Copies it to %LOCALAPPDATA%\impactor-mcp\impactor-mcp.exe.
    3. Registers it with Claude Code (`claude mcp add`) and Claude Desktop.
#>
param(
    [switch]$NoClaudeCode,   # Skip registering with Claude Code
    [switch]$NoDesktop,      # Skip registering with Claude Desktop
    [switch]$AutoRefresh,    # Let the server auto-refresh saved apps in the background
    [switch]$FromSource,     # Build with cargo even if a prebuilt binary is available
    [switch]$Uninstall       # Remove the binary and both registrations
)

$ErrorActionPreference = 'Stop'
$Repo = 'YeetDisDude/impactor-mcp'
$ServerName = 'impactor'
$InstallDir = Join-Path $env:LOCALAPPDATA 'impactor-mcp'
$BinPath = Join-Path $InstallDir 'impactor-mcp.exe'
$DesktopConfig = Join-Path $env:APPDATA 'Claude\claude_desktop_config.json'
# Empty when piped into `iex`; a local checkout has Cargo.toml next to this script.
$LocalRoot = if ($PSScriptRoot -and (Test-Path (Join-Path $PSScriptRoot 'Cargo.toml'))) { $PSScriptRoot } else { $null }

function Step($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }
function Warn($msg) { Write-Host "warning: $msg" -ForegroundColor Yellow }

function Write-Utf8NoBom($path, $text) {
    # Windows PowerShell 5.1's `Out-File -Encoding utf8` writes a BOM, which
    # JSON parsers may reject.
    [System.IO.File]::WriteAllText($path, $text, (New-Object System.Text.UTF8Encoding($false)))
}

function Invoke-Native([scriptblock]$Block, [switch]$Quiet) {
    # Native stderr under ErrorActionPreference=Stop throws in PowerShell 5.1.
    $old = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        if ($Quiet) { & $Block 2>&1 | Out-Null } else { & $Block | Out-Host }
        return $LASTEXITCODE
    } catch {
        return 1
    } finally {
        $ErrorActionPreference = $old
    }
}

function Read-DesktopConfig {
    if (Test-Path $DesktopConfig) {
        $raw = Get-Content $DesktopConfig -Raw
        if (-not [string]::IsNullOrWhiteSpace($raw)) { return ($raw | ConvertFrom-Json) }
    }
    return (New-Object PSObject)
}

function Install-Prebuilt {
    $arch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'aarch64' } else { 'x86_64' }
    $url = "https://github.com/$Repo/releases/latest/download/impactor-mcp-windows-$arch.exe"
    Step "Downloading prebuilt binary ($url)"
    $tmp = Join-Path $env:TEMP "impactor-mcp-$([guid]::NewGuid()).exe"
    try {
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
        $ProgressPreference = 'SilentlyContinue'
        Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $tmp
        Copy-Item -Force $tmp $BinPath
        return $true
    } catch {
        Warn "No prebuilt binary available ($($_.Exception.Message)); building from source instead."
        return $false
    } finally {
        Remove-Item -Force $tmp -ErrorAction SilentlyContinue
    }
}

function Install-FromSource {
    $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        Step 'Rust not found; installing rustup via winget'
        if (-not (Get-Command winget -ErrorAction SilentlyContinue)) {
            throw 'winget is not available. Install Rust from https://rustup.rs and re-run this script.'
        }
        winget install --id Rustlang.Rustup -e --silent --accept-package-agreements --accept-source-agreements
        Invoke-Native { & "$env:USERPROFILE\.cargo\bin\rustup.exe" default stable } | Out-Null
    }

    $hint = "cargo build failed. Windows builds need the MSVC C++ build tools: winget install Microsoft.VisualStudio.2022.BuildTools --override `"--add Microsoft.VisualStudio.Workload.VCTools --includeRecommended --passive`""
    Step 'Building impactor-mcp (release). The first build downloads dependencies and takes a few minutes...'
    if ($LocalRoot) {
        Push-Location $LocalRoot
        try { $code = Invoke-Native { cargo build --release --locked } } finally { Pop-Location }
        if ($code -ne 0) { throw $hint }
        Copy-Item -Force (Join-Path $LocalRoot 'target\release\impactor-mcp.exe') $BinPath
    } else {
        $root = Join-Path $env:TEMP "impactor-mcp-build-$([guid]::NewGuid())"
        try {
            $code = Invoke-Native { cargo install --locked --git "https://github.com/$Repo" impactor-mcp --root $root }
            if ($code -ne 0) { throw $hint }
            Copy-Item -Force (Join-Path $root 'bin\impactor-mcp.exe') $BinPath
        } finally {
            Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue
        }
    }
}

function Remove-Registration($Name) {
    if (Get-Command claude -ErrorAction SilentlyContinue) {
        Invoke-Native -Quiet { claude mcp remove --scope user $Name } | Out-Null
    }
    if (Test-Path $DesktopConfig) {
        $config = Read-DesktopConfig
        if ($config.mcpServers -and ($config.mcpServers.PSObject.Properties.Name -contains $Name)) {
            $config.mcpServers.PSObject.Properties.Remove($Name)
            Write-Utf8NoBom $DesktopConfig ($config | ConvertTo-Json -Depth 32)
        }
    }
}

# Earlier releases were named `plume-impactor` / `plumemcp`; clean those up.
function Remove-Legacy {
    Remove-Registration 'plume-impactor'
    $legacyDir = Join-Path $env:LOCALAPPDATA 'PlumeImpactor\mcp'
    if (Test-Path $legacyDir) { Remove-Item -Recurse -Force $legacyDir -ErrorAction SilentlyContinue }
}

# --- Uninstall ---------------------------------------------------------------
if ($Uninstall) {
    Remove-Registration $ServerName
    Remove-Legacy
    if (Test-Path $InstallDir) { Remove-Item -Recurse -Force $InstallDir }
    Step 'Uninstalled. (Impactor accounts in %APPDATA%\PlumeImpactor were left untouched.)'
    return
}

# --- 1. Get the binary -------------------------------------------------------
New-Item -ItemType Directory -Force $InstallDir | Out-Null
# Prefer the prebuilt binary (no compiling); build only when asked or when none exists.
if ($FromSource -or -not (Install-Prebuilt)) {
    Install-FromSource
}
Step "Installed $BinPath"
Remove-Legacy

# --- 2. Register -------------------------------------------------------------
if (-not $NoClaudeCode) {
    if (Get-Command claude -ErrorAction SilentlyContinue) {
        Step 'Registering with Claude Code (user scope)'
        Invoke-Native -Quiet { claude mcp remove --scope user $ServerName } | Out-Null
        if ($AutoRefresh) {
            Invoke-Native { claude mcp add --scope user -e IMPACTOR_MCP_AUTO_REFRESH=1 $ServerName -- $BinPath } | Out-Null
        } else {
            Invoke-Native { claude mcp add --scope user $ServerName -- $BinPath } | Out-Null
        }
    } else {
        Warn "Claude Code CLI not found; skipped. Later run: claude mcp add --scope user $ServerName -- `"$BinPath`""
    }
}

if (-not $NoDesktop) {
    Step "Registering with Claude Desktop ($DesktopConfig)"
    New-Item -ItemType Directory -Force (Split-Path $DesktopConfig) | Out-Null
    if (Test-Path $DesktopConfig) { Copy-Item -Force $DesktopConfig "$DesktopConfig.bak" }
    $config = Read-DesktopConfig
    if (-not ($config.PSObject.Properties.Name -contains 'mcpServers')) {
        $config | Add-Member -NotePropertyName mcpServers -NotePropertyValue (New-Object PSObject)
    }
    $entry = [ordered]@{ command = $BinPath; args = @() }
    if ($AutoRefresh) { $entry.env = @{ IMPACTOR_MCP_AUTO_REFRESH = '1' } }
    $config.mcpServers | Add-Member -Force -NotePropertyName $ServerName -NotePropertyValue ([PSCustomObject]$entry)
    Write-Utf8NoBom $DesktopConfig ($config | ConvertTo-Json -Depth 32)
    Write-Host '    Restart Claude Desktop to load the server.'
}

# --- 3. Prerequisite check ---------------------------------------------------
if (-not (Get-Service -Name 'Apple Mobile Device Service' -ErrorAction SilentlyContinue)) {
    Warn 'Apple Mobile Device Service not found. Install iTunes or "Apple Devices" (Microsoft Store) so the server can talk to iPhones/iPads.'
}

Step "Done. Other MCP clients: use command `"$BinPath`" (stdio). Try asking your agent: 'list my connected iOS devices'."
