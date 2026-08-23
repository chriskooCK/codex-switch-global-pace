# codex-switch-global-pace installer / uninstaller for Windows
# Usage:
#   irm https://github.com/chriskooCK/codex-switch-global-pace/releases/latest/download/install.ps1 | iex
#   $env:CS_DEV="1"; irm https://github.com/chriskooCK/codex-switch-global-pace/releases/download/dev/install.ps1 | iex
#   $env:CS_VERSION="20260712.1.0"; irm .../install.ps1 | iex # install specific version
#   $env:CS_UNINSTALL="1"; irm .../install.ps1 | iex         # uninstall this program

$ErrorActionPreference = "Stop"
$Repo = "chriskooCK/codex-switch-global-pace"
$BinaryName = "codex-switch-global-pace.exe"
$InstallDir = Join-Path $env:LOCALAPPDATA "Programs\codex-switch-global-pace"
$DataDir = Join-Path $env:USERPROFILE ".codex-switch"

# ── Uninstall ────────────────────────────────────────────
if ($env:CS_UNINSTALL -eq "1") {
    Write-Host "[info]  Uninstalling codex-switch-global-pace..." -ForegroundColor Blue

    $BinPath = Join-Path $InstallDir $BinaryName
    $ServiceUninstallFailed = $false
    if (Test-Path $BinPath) {
        & $BinPath daemon uninstall
        if ($LASTEXITCODE -eq 0) {
            Write-Host "[info]  Removed daemon scheduled task." -ForegroundColor Blue
        } else {
            Write-Warning "Failed to remove daemon scheduled task with '$BinPath daemon uninstall'."
            $ServiceUninstallFailed = $true
        }
    } else {
        & schtasks.exe /Query /TN "\codex-switch-global-pace-daemon" 2>$null | Out-Null
        if ($LASTEXITCODE -eq 0) {
            & schtasks.exe /End /TN "\codex-switch-global-pace-daemon" 2>$null | Out-Null
            & schtasks.exe /Delete /TN "\codex-switch-global-pace-daemon" /F
            if ($LASTEXITCODE -ne 0) {
                Write-Warning "Failed to delete Windows scheduled task \codex-switch-global-pace-daemon."
                $ServiceUninstallFailed = $true
            } else {
                Write-Host "[info]  Removed daemon scheduled task." -ForegroundColor Blue
            }
        }
    }

    if ($ServiceUninstallFailed) {
        Write-Error "Daemon service cleanup failed; binary and data were kept. Resolve the service error and retry uninstall."
        exit 1
    }

    # Remove binary
    if (Test-Path $BinPath) {
        Remove-Item -Force $BinPath
        Write-Host "[info]  Removed $BinPath" -ForegroundColor Blue
    }

    # Remove install directory if empty
    if ((Test-Path $InstallDir) -and @(Get-ChildItem $InstallDir).Count -eq 0) {
        Remove-Item -Force $InstallDir
    }

    # Remove from PATH. Compared entry by entry rather than with -like: the
    # pattern operators treat [ and ] in a path (a username can contain them) as
    # wildcards, and a substring match would also fire on an unrelated directory
    # that merely starts with this one. Empty entries are dropped on the way out
    # because Windows resolves them to the current working directory.
    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $PathEntries = @($UserPath -split ";" | Where-Object { $_.Trim() -ne "" })
    if ($PathEntries -contains $InstallDir) {
        $NewPath = ($PathEntries | Where-Object { $_ -ne $InstallDir }) -join ";"
        [Environment]::SetEnvironmentVariable("Path", $NewPath, "User")
        Write-Host "[info]  Removed $InstallDir from user PATH" -ForegroundColor Blue
    }

    # This directory is deliberately shared with codex-switch so existing
    # profiles work without another login. Never remove it from this uninstaller.
    if (Test-Path $DataDir) {
        Write-Host "[info]  Kept shared profile data: $DataDir" -ForegroundColor Blue
    }

    Write-Host "[info]  codex-switch-global-pace has been uninstalled." -ForegroundColor Blue
    exit 0
}

# ── Install ──────────────────────────────────────────────

# Detect architecture
$Arch = if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -eq "Arm64") { "arm64" } else { "amd64" }
$AssetName = "codex-switch-global-pace-windows-${Arch}.zip"

# Determine version / channel
$UseDev = $env:CS_DEV -eq "1"
if ($UseDev) {
    $Version = "dev"
    $DownloadUrl = "https://github.com/$Repo/releases/download/dev/$AssetName"
} else {
    $Version = if ($env:CS_VERSION) { $env:CS_VERSION } else { "latest" }
    if ($Version -eq "latest") {
        $DownloadUrl = "https://github.com/$Repo/releases/latest/download/$AssetName"
    } else {
        $DownloadUrl = "https://github.com/$Repo/releases/download/v$Version/$AssetName"
    }
}

Write-Host "[info]  Detected: windows/$Arch" -ForegroundColor Blue
Write-Host "[info]  Downloading: $DownloadUrl" -ForegroundColor Blue

# Download
$TmpDir = Join-Path $env:TEMP "codex-switch-global-pace-install-$(Get-Random)"
New-Item -ItemType Directory -Path $TmpDir -Force | Out-Null
$ZipPath = Join-Path $TmpDir $AssetName
$ChecksumUrl = "$DownloadUrl.sha256"
$ChecksumPath = "$ZipPath.sha256"

try {
    Invoke-WebRequest -Uri $DownloadUrl -OutFile $ZipPath -UseBasicParsing
    Invoke-WebRequest -Uri $ChecksumUrl -OutFile $ChecksumPath -UseBasicParsing
} catch {
    Write-Host "[error] Archive or checksum download failed: $_" -ForegroundColor Red
    Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
    exit 1
}

# Verify checksum before extracting any downloaded content
$ChecksumText = (Get-Content -LiteralPath $ChecksumPath -Raw).Trim()
$ChecksumPattern = '^(?<hash>[0-9A-Fa-f]{64})\s+\*?(?<file>\S+)$'
if ($ChecksumText -notmatch $ChecksumPattern -or (Split-Path -Leaf $Matches.file) -ne $AssetName) {
    Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
    Write-Error "Invalid or empty checksum file for $AssetName."
    exit 1
}

$ExpectedSha256 = $Matches.hash.ToUpperInvariant()
$ActualSha256 = (Get-FileHash -LiteralPath $ZipPath -Algorithm SHA256).Hash.ToUpperInvariant()
if ($ActualSha256 -ne $ExpectedSha256) {
    Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
    Write-Error "Checksum mismatch for $AssetName; refusing to extract it."
    exit 1
}
Write-Host "[info]  Checksum verified: $AssetName" -ForegroundColor Blue

# Extract
Expand-Archive -Path $ZipPath -DestinationPath $TmpDir -Force

# A running daemon holds the installed executable open on Windows. Detect it
# through the old binary before replacement, stop it gracefully, and remember
# to restore the previous running state after the verified binary is installed.
$InstalledBin = Join-Path $InstallDir $BinaryName
$DaemonWasRunning = $false
if (Test-Path -LiteralPath $InstalledBin) {
    $DaemonStatusText = (& $InstalledBin --json daemon status 2>$null | Out-String)
    if ($LASTEXITCODE -eq 0) {
        try {
            $DaemonStatus = $DaemonStatusText | ConvertFrom-Json
            $DaemonWasRunning = [bool]$DaemonStatus.running
        } catch {
            Write-Warning "Could not parse the existing daemon status; replacement will continue only if the executable is not locked."
        }
    } else {
        Write-Warning "Could not query the existing daemon status; replacement will continue only if the executable is not locked."
    }
}

if ($DaemonWasRunning) {
    Write-Host "[info]  Stopping the running daemon before upgrade..." -ForegroundColor Blue
    & $InstalledBin daemon stop
    if ($LASTEXITCODE -ne 0) {
        Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
        Write-Error "The running daemon could not be stopped safely. Retry 'codex-switch-global-pace daemon stop', then run the installer again."
        exit 1
    }
}

# Install
New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
try {
    Move-Item -LiteralPath (Join-Path $TmpDir $BinaryName) -Destination $InstalledBin -Force
} catch {
    $InstallError = $_
    if ($DaemonWasRunning -and (Test-Path -LiteralPath $InstalledBin)) {
        & $InstalledBin daemon start 2>$null
        if ($LASTEXITCODE -ne 0) {
            Write-Warning "The previous daemon could not be restarted after the failed replacement."
        }
    }
    Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
    Write-Error "Could not replace $InstalledBin. Close any running codex-switch-global-pace process or run 'codex-switch-global-pace daemon stop', then retry. Details: $InstallError"
    exit 1
}

# Add to PATH if not already present.
#
# Rebuilt from entries instead of concatenating "$UserPath;$InstallDir": when the
# user has no User-scoped Path, or it ends with a separator, that concatenation
# produces an empty PATH element. Windows resolves an empty element to the
# current working directory when it searches for an executable, so the installer
# would leave every directory the user later cd's into on the search path for
# every command they run — a persistent change outliving the tool itself.
$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
$PathEntries = @($UserPath -split ";" | Where-Object { $_.Trim() -ne "" })
if ($PathEntries -notcontains $InstallDir) {
    $NewPath = ($PathEntries + $InstallDir) -join ";"
    [Environment]::SetEnvironmentVariable("Path", $NewPath, "User")
    Write-Host "[info]  Added $InstallDir to user PATH (restart terminal to take effect)" -ForegroundColor Blue
}

# Cleanup
Remove-Item -Recurse -Force $TmpDir

# Verify
$VersionOutput = & $InstalledBin --version 2>&1
Write-Host "[info]  Installed: $VersionOutput" -ForegroundColor Blue
if ($DaemonWasRunning) {
    Write-Host "[info]  Restarting the daemon after upgrade..." -ForegroundColor Blue
    & $InstalledBin daemon start
    if ($LASTEXITCODE -ne 0) {
        Write-Error "The binary was installed, but the previously running daemon could not be restarted. Run 'codex-switch-global-pace daemon start' after resolving the reported error."
        exit 1
    }
}
Write-Host "[info]  Run 'codex-switch-global-pace --help' to get started" -ForegroundColor Blue
