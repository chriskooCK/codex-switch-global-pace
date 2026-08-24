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
$SemVerPattern = '\A(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-((0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)(\.(0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*))*))?(\+([0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*))?\z'

function Assert-SupportedVersion {
    param([Parameter(Mandatory = $true)][string]$Value)

    if (-not [regex]::IsMatch(
        $Value,
        $SemVerPattern,
        [System.Text.RegularExpressions.RegexOptions]::CultureInvariant
    )) {
        throw "Invalid CS_VERSION '$Value'; expected a SemVer version such as 20260824.6.0."
    }
}

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
            $Task = Get-ScheduledTask `
                -TaskName "codex-switch-global-pace-daemon" `
                -TaskPath "\" `
                -ErrorAction Stop
            $TaskState = [string]$Task.State
            if ($TaskState -notin @("Ready", "Disabled")) {
                Write-Warning "Scheduled daemon state is '$TaskState', and the installed binary is unavailable for a graceful stop. Restore the binary, stop the daemon safely, and retry uninstall."
                $ServiceUninstallFailed = $true
            } else {
                Unregister-ScheduledTask `
                    -TaskName "codex-switch-global-pace-daemon" `
                    -TaskPath "\" `
                    -Confirm:$false `
                    -ErrorAction Stop
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
        Assert-SupportedVersion $Version
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
$CandidateBin = Join-Path $TmpDir $BinaryName

# Prove that the downloaded executable can run on this host before stopping a
# working daemon. A valid checksum authenticates the release bytes, but it does
# not prove that the selected asset is executable on this Windows installation.
try {
    $CandidateVersionOutput = & $CandidateBin --version 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "candidate version check exited with code ${LASTEXITCODE}: $CandidateVersionOutput"
    }
} catch {
    Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
    Write-Host "[error] Downloaded binary failed its pre-install check; the existing installation was not changed: $_" -ForegroundColor Red
    exit 1
}

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

$InstallError = $null
$RestartError = $null
$VersionOutput = $null
try {
    # Install
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    Move-Item -LiteralPath $CandidateBin -Destination $InstalledBin -Force

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

    # Verify the replacement before restoring the daemon.
    $VersionOutput = & $InstalledBin --version 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "Installed binary failed its version check: $VersionOutput"
    }
} catch {
    $InstallError = $_
} finally {
    if ($DaemonWasRunning) {
        if (Test-Path -LiteralPath $InstalledBin) {
            Write-Host "[info]  Restoring the previously running daemon..." -ForegroundColor Blue
            try {
                & $InstalledBin daemon start
                if ($LASTEXITCODE -ne 0) {
                    throw "daemon start exited with code $LASTEXITCODE"
                }
            } catch {
                $RestartError = $_
            }
        } else {
            $RestartError = "The installed executable is missing after replacement."
        }
    }
}

# Temporary-file cleanup cannot be allowed to strand a daemon that was already
# restored in the finally block.
Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue

if ($null -ne $InstallError) {
    $RestartDetail = if ($null -ne $RestartError) {
        " The previously running daemon also could not be restored: $RestartError"
    } else {
        ""
    }
    Write-Error "Installation did not finish cleanly. Close any running codex-switch-global-pace process and retry. Details: $InstallError$RestartDetail"
    exit 1
}
if ($null -ne $RestartError) {
    Write-Error "The binary was installed, but the previously running daemon could not be restored: $RestartError. Run 'codex-switch-global-pace daemon start' after resolving the reported error."
    exit 1
}

Write-Host "[info]  Installed: $VersionOutput" -ForegroundColor Blue
Write-Host "[info]  Run 'codex-switch-global-pace --help' to get started" -ForegroundColor Blue
