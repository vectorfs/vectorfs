# install-cli.ps1 — install the `vf` client CLI from the repo's GitHub
# Releases (the Windows-native counterpart of install-cli.sh; the release
# pipeline ships a CLIENT-ONLY zip for x86_64-pc-windows-msvc — the server
# binary stays linux).
#
#   iwr -useb https://raw.githubusercontent.com/vectorfs/vectorfs/main/scripts/install-cli.ps1 | iex
#
# Or save + run with parameters (the iex form cannot bind params — use env
# vars there):
#   .\install-cli.ps1 -Version v0.1.0 -To C:\Users\me\bin
#   [env: VERSION / INSTALL_DIR / GITHUB_TOKEN are honored]
#
# Behavior notes (mirrors install-cli.sh):
#   * downloads the release zip AND sha256sums.txt — the asset is verified
#     BEFORE anything is installed (integrity is part of this project's
#     wire contract; the installer honors the same bar);
#   * PowerShell 5.1 compatible (no ternary / ?? / SkipHttpErrorCheck);
#   * `vf config init` then `vf mcp` are the next steps (endpoint/creds
#     resolve from flags > VF_* env > config.toml, same as every platform).
param(
    [string]$Version,
    [string]$To
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"   # Invoke-WebRequest progress is 10x slower than the download
# Windows PowerShell 5.1 may still default to TLS 1.0 for framework WebRequests
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$Repo = "vectorfs/vectorfs"
if (-not $Version) { $Version = if ($env:VERSION) { $env:VERSION } else { "latest" } }
if (-not $To) { $To = if ($env:INSTALL_DIR) { $env:INSTALL_DIR } else { Join-Path $env:USERPROFILE ".local\bin" } }
$Token = $env:GITHUB_TOKEN

function Fail($msg) { Write-Host "install-cli: error: $msg" -ForegroundColor Red; exit 1 }
function Note($msg) { Write-Host "install-cli: $msg" }

# --- resolve the version tag (no API: follow the /releases/latest redirect)
if ($Version -eq "latest") {
    $req = [System.Net.HttpWebRequest]::Create("https://github.com/$Repo/releases/latest")
    $req.AllowAutoRedirect = $false
    $req.UserAgent = "install-cli.ps1"
    if ($Token) { $req.Headers.Add("Authorization", "Bearer $Token") }
    try {
        $resp = $req.GetResponse()
        $redirect = $resp.Headers["Location"]
        $resp.Close()
    } catch { Fail "cannot reach github.com/$Repo/releases/latest : $_" }
    if (-not $redirect) { Fail "no redirect Location from /releases/latest" }
    $Version = Split-Path -Leaf $redirect        # .../tag/v0.1.0 -> v0.1.0
    if ($Version -notlike "v*") { Fail "unexpected latest redirect: $redirect" }
}
$Semver = $Version.TrimStart("v")
$Base = "https://github.com/$Repo/releases/download/$Version"
$Asset = "vectorfs-$Semver-x86_64-pc-windows-msvc.zip"

function Get-Asset($url, $dest) {
    $headers = @{}
    if ($Token) { $headers["Authorization"] = "Bearer $Token" }
    try {
        Invoke-WebRequest -Uri $url -OutFile $dest -Headers $headers -UseBasicParsing
    } catch { Fail "download failed: $url : $_" }
}

# --- download to a private temp dir ------------------------------------------
$Tmp = New-Item -ItemType Directory -Force -Path (Join-Path ([System.IO.Path]::GetTempPath()) ("vf-install-" + [guid]::NewGuid().ToString("N").Substring(0, 8)))
try {
    Note "downloading $Asset ($Version)"
    Get-Asset "$Base/$Asset" (Join-Path $Tmp $Asset)
    Get-Asset "$Base/sha256sums.txt" (Join-Path $Tmp "sha256sums.txt")

    # --- verify BEFORE installing (subset check of the release's own sums)
    # sha256sums.txt format: "<hex>  <filename>" (two-space separator)
    $sumsFile = Join-Path $Tmp "sha256sums.txt"
    $expected = $null
    foreach ($line in (Get-Content $sumsFile)) {
        $parts = $line -split '\s+', 2
        if ($parts.Count -eq 2 -and $parts[1].Trim() -eq $Asset) { $expected = $parts[0].ToLower() }
    }
    if (-not $expected) { Fail "$Asset missing from the release's sha256sums.txt" }
    $actual = (Get-FileHash -Algorithm SHA256 -Path (Join-Path $Tmp $Asset)).Hash.ToLower()
    if ($actual -ne $expected) { Fail "CHECKSUM MISMATCH for $Asset — not installing" }
    Note "sha256 verified"

    # --- install ----------------------------------------------------------------
    $extract = Join-Path $Tmp "extract"
    Expand-Archive -Path (Join-Path $Tmp $Asset) -DestinationPath $extract
    $src = Join-Path $extract "vf.exe"
    if (-not (Test-Path $src)) { Fail "vf.exe missing from the zip" }
    New-Item -ItemType Directory -Force -Path $To | Out-Null
    Copy-Item $src (Join-Path $To "vf.exe") -Force
    $bin = Join-Path $To "vf.exe"
} finally {
    Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
}

Note "installed: $bin"
$onPath = ($env:Path -split ';' | Where-Object { $_ -ne "" }) -contains $To
if (-not $onPath) {
    Note ("NOTE: {0} is not in PATH — add it via Settings > System > About > Advanced system settings >" +
          " Environment Variables, or for this session only:`n  `$env:Path += `";{0}`"" -f $To)
}
& $bin --version
Note "next: vf config init --endpoint http://<daemon>:8787 --access-key AK --secret-key SK"
Note 'MCP (agent remote mode): {"vf": {"command": "vf", "args": ["mcp"]}}'
