# Renders docs/images/src/*.html to docs/images/*.png at 2x using headless Chrome/Edge.
# Usage: powershell -ExecutionPolicy Bypass -File docs\images\render.ps1 [name ...]
param([string[]]$Names)

$ErrorActionPreference = 'Stop'
$Sizes = @{
    'hero'         = '1600,800'
    'session'      = '1600,960'
    'tools'        = '1600,850'
    'architecture' = '1600,820'
}

$browser = @(
    "$env:ProgramFiles\Google\Chrome\Application\chrome.exe",
    "${env:ProgramFiles(x86)}\Microsoft\Edge\Application\msedge.exe",
    "$env:ProgramFiles\Microsoft\Edge\Application\msedge.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $browser) { throw 'Chrome or Edge is required.' }

$src = Join-Path $PSScriptRoot 'src'
if (-not $Names) { $Names = $Sizes.Keys }
# Isolated throwaway profile, so the user's browser profile is never touched.
$profileDir = Join-Path $env:TEMP "impactor-mcp-render-$([guid]::NewGuid())"

# The browser logs harmless noise to stderr; don't let that abort the script.
$ErrorActionPreference = 'Continue'
try {
    foreach ($name in $Names) {
        $html = Join-Path $src "$name.html"
        $out = Join-Path $PSScriptRoot "$name.png"
        $url = 'file:///' + ($html -replace '\\', '/')
        & $browser --headless=new --disable-gpu --hide-scrollbars --force-device-scale-factor=2 `
            "--user-data-dir=$profileDir" "--window-size=$($Sizes[$name])" "--screenshot=$out" $url 2>&1 | Out-Null
        if (Test-Path $out) { Write-Host "rendered $out" } else { Write-Warning "failed to render $name" }
    }
} finally {
    Remove-Item -Recurse -Force $profileDir -ErrorAction SilentlyContinue
}
