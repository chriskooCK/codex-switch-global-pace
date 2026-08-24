# codex-switch-global-pace installer / uninstaller for Windows
# Usage:
#   irm https://github.com/chriskooCK/codex-switch-global-pace/releases/latest/download/install.ps1 | iex
#   $env:CS_DEV="1"; irm https://github.com/chriskooCK/codex-switch-global-pace/releases/download/dev/install.ps1 | iex
#   $env:CS_VERSION="20260712.1.0"; irm .../install.ps1 | iex # install specific version
#   $env:CS_UNINSTALL="1"; irm .../install.ps1 | iex         # uninstall this program

$ErrorActionPreference = "Stop"
$Repo = "chriskooCK/codex-switch-global-pace"
$PackagedReleaseVersion = ""
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

function Get-CheckedDaemonStatus {
    param([Parameter(Mandatory = $true)][string]$BinPath)

    $StatusText = (& $BinPath --json daemon status 2>&1 | Out-String)
    if ($LASTEXITCODE -ne 0) {
        throw "daemon status exited with code ${LASTEXITCODE}: $StatusText"
    }
    try {
        $Status = $StatusText | ConvertFrom-Json
    } catch {
        throw "daemon status was not valid JSON: $_"
    }
    $RunningProperty = $Status.PSObject.Properties["running"]
    if ($null -eq $RunningProperty -or $RunningProperty.Value -isnot [bool]) {
        throw "daemon status did not contain a Boolean 'running' field"
    }
    $PlatformProperty = $Status.PSObject.Properties["platform"]
    if ($null -eq $PlatformProperty -or $null -eq $PlatformProperty.Value) {
        throw "daemon status did not contain a 'platform' object"
    }
    $ServiceProperty = $PlatformProperty.Value.PSObject.Properties["service_installed"]
    if ($null -eq $ServiceProperty -or $ServiceProperty.Value -isnot [bool]) {
        throw "daemon status did not contain a Boolean 'platform.service_installed' field"
    }
    return $Status
}

function Stop-And-ConfirmDaemonAbsent {
    param([Parameter(Mandatory = $true)][string]$BinPath)

    $Before = Get-CheckedDaemonStatus -BinPath $BinPath
    if ($Before.running -or $Before.platform.service_installed) {
        & $BinPath daemon stop
        if ($LASTEXITCODE -ne 0) {
            throw "daemon stop exited with code $LASTEXITCODE"
        }
    }
    $After = Get-CheckedDaemonStatus -BinPath $BinPath
    if ($After.running) {
        throw "daemon still reports running after the stop boundary"
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

$ExpectedReleaseVersion = if ($Version -notin @("latest", "dev")) {
    $Version
} else {
    if ([string]::IsNullOrWhiteSpace($PackagedReleaseVersion)) {
        throw "This installer is not bound to a GitHub Release. Download install.ps1 from the stable or dev Release assets instead of running the repository copy directly."
    }
    Assert-SupportedVersion $PackagedReleaseVersion
    $PackagedReleaseVersion
}
if ($UseDev -and $ExpectedReleaseVersion -notmatch '-dev(?:\.|$)') {
    throw "Development installer expected a -dev release, got '$ExpectedReleaseVersion'."
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
    $CandidateVersionLine = (($CandidateVersionOutput | Select-Object -First 1) -as [string]).Trim()
    $ExpectedVersionLine = "codex-switch-global-pace $ExpectedReleaseVersion"
    if ($CandidateVersionLine -cne $ExpectedVersionLine) {
        throw "candidate reported '$CandidateVersionLine', expected '$ExpectedVersionLine'"
    }
} catch {
    Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
    Write-Host "[error] Downloaded binary failed its pre-install check; the existing installation was not changed: $_" -ForegroundColor Red
    exit 1
}

# Stage the verified candidate beside the installed executable before stopping a
# working daemon. The same-directory rename below is the only publication step,
# so a download drive and the install drive can never turn this into a partial
# cross-volume move.
$InstalledBin = Join-Path $InstallDir $BinaryName
$InstallDirWasPresent = Test-Path -LiteralPath $InstallDir -PathType Container
if ((Test-Path -LiteralPath $InstallDir) -and -not $InstallDirWasPresent) {
    Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
    Write-Host "[error] Install path exists but is not a directory: $InstallDir" -ForegroundColor Red
    exit 1
}
$ExistingBinaryWasPresent = Test-Path -LiteralPath $InstalledBin -PathType Leaf
if ((Test-Path -LiteralPath $InstalledBin) -and -not $ExistingBinaryWasPresent) {
    Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
    Write-Host "[error] Installed binary path exists but is not a regular file: $InstalledBin" -ForegroundColor Red
    exit 1
}

$TransactionId = [Guid]::NewGuid().ToString("N")
$BinaryStem = [System.IO.Path]::GetFileNameWithoutExtension($BinaryName)
$StagedBin = Join-Path $InstallDir ".$BinaryStem.install-$TransactionId.exe"
$BackupBin = Join-Path $InstallDir ".$BinaryStem.rollback-$TransactionId.exe"
$FailedBin = Join-Path $InstallDir ".$BinaryStem.failed-$TransactionId.exe"
$OriginalUserPath = [Environment]::GetEnvironmentVariable("Path", "User")

try {
    if (-not $InstallDirWasPresent) {
        New-Item -ItemType Directory -Path $InstallDir | Out-Null
    }
    Copy-Item -LiteralPath $CandidateBin -Destination $StagedBin
    $StagedVersionOutput = & $StagedBin --version 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "staged candidate version check exited with code ${LASTEXITCODE}: $StagedVersionOutput"
    }
    $StagedVersionLine = (($StagedVersionOutput | Select-Object -First 1) -as [string]).Trim()
    if ($StagedVersionLine -cne "codex-switch-global-pace $ExpectedReleaseVersion") {
        throw "staged candidate reported '$StagedVersionLine', expected 'codex-switch-global-pace $ExpectedReleaseVersion'"
    }
} catch {
    $StageError = $_
    Remove-Item -LiteralPath $StagedBin -Force -ErrorAction SilentlyContinue
    if (-not $InstallDirWasPresent -and (Test-Path -LiteralPath $InstallDir) -and @(Get-ChildItem -LiteralPath $InstallDir).Count -eq 0) {
        Remove-Item -LiteralPath $InstallDir -Force -ErrorAction SilentlyContinue
    }
    Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
    Write-Host "[error] Could not stage the verified binary; the existing installation was not changed: $StageError" -ForegroundColor Red
    exit 1
}

# A running daemon holds the executable open. Its state is part of this
# transaction, so an unreadable or malformed status is an error rather than a
# guess that it is stopped.
$DaemonWasRunning = $false
$DaemonServiceInstalled = $false
if ($ExistingBinaryWasPresent) {
    try {
        $DaemonStatus = Get-CheckedDaemonStatus -BinPath $InstalledBin
        $DaemonWasRunning = $DaemonStatus.running
        $DaemonServiceInstalled = $DaemonStatus.platform.service_installed
    } catch {
        $StatusError = $_
        Remove-Item -LiteralPath $StagedBin -Force -ErrorAction SilentlyContinue
        if (-not $InstallDirWasPresent -and @(Get-ChildItem -LiteralPath $InstallDir).Count -eq 0) {
            Remove-Item -LiteralPath $InstallDir -Force -ErrorAction SilentlyContinue
        }
        Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
        Write-Host "[error] Could not determine the existing daemon state; nothing was replaced: $StatusError" -ForegroundColor Red
        exit 1
    }
}

if ($DaemonWasRunning -or $DaemonServiceInstalled) {
    Write-Host "[info]  Stopping the existing daemon task before upgrade..." -ForegroundColor Blue
    & $InstalledBin daemon stop
    if ($LASTEXITCODE -ne 0) {
        Remove-Item -LiteralPath $StagedBin -Force -ErrorAction SilentlyContinue
        Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
        Write-Host "[error] The running daemon could not be stopped safely. The installed binary was not replaced." -ForegroundColor Red
        exit 1
    }
}

$InstallError = $null
$VersionOutput = $null
$OldBinaryBackedUp = $false
$NewBinaryPublished = $false
$PathMutationAttempted = $false
$DaemonRestarted = $false
$DaemonRestartAttempted = $false
$PreviousBinaryRestored = $ExistingBinaryWasPresent
try {
    if ($ExistingBinaryWasPresent) {
        Move-Item -LiteralPath $InstalledBin -Destination $BackupBin
        $OldBinaryBackedUp = $true
        $PreviousBinaryRestored = $false
    }
    Move-Item -LiteralPath $StagedBin -Destination $InstalledBin
    $NewBinaryPublished = $true

    # Preserve the exact original User PATH for rollback. Empty entries are
    # excluded only from the successful new value because Windows interprets
    # them as the current working directory.
    $PathEntries = @($OriginalUserPath -split ";" | Where-Object { $_.Trim() -ne "" })
    if ($PathEntries -notcontains $InstallDir) {
        $NewPath = ($PathEntries + $InstallDir) -join ";"
        $PathMutationAttempted = $true
        [Environment]::SetEnvironmentVariable("Path", $NewPath, "User")
        Write-Host "[info]  Added $InstallDir to user PATH (restart terminal to take effect)" -ForegroundColor Blue
    }

    $VersionOutput = & $InstalledBin --version 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "installed binary version check exited with code ${LASTEXITCODE}: $VersionOutput"
    }
    $InstalledVersionLine = (($VersionOutput | Select-Object -First 1) -as [string]).Trim()
    if ($InstalledVersionLine -cne "codex-switch-global-pace $ExpectedReleaseVersion") {
        throw "installed binary reported '$InstalledVersionLine', expected 'codex-switch-global-pace $ExpectedReleaseVersion'"
    }

    if ($DaemonWasRunning) {
        Write-Host "[info]  Restoring the previously running daemon..." -ForegroundColor Blue
        $DaemonRestartAttempted = $true
        & $InstalledBin daemon start
        if ($LASTEXITCODE -ne 0) {
            throw "daemon start exited with code $LASTEXITCODE"
        }
        $DaemonRestarted = $true
    }

    if ($OldBinaryBackedUp) {
        Remove-Item -LiteralPath $BackupBin -Force
        $OldBinaryBackedUp = $false
    }
} catch {
    $InstallError = $_
}

if ($null -ne $InstallError) {
    $RollbackErrors = @()

    $DaemonSafeForBinaryRollback = $true
    if ($DaemonRestarted -or $DaemonRestartAttempted) {
        try {
            Stop-And-ConfirmDaemonAbsent -BinPath $InstalledBin
            $DaemonRestarted = $false
        } catch {
            $DaemonSafeForBinaryRollback = $false
            $RollbackErrors += "could not prove the new daemon/task was stopped: $_"
        }
    }

    $NewBinaryMovedAside = $false
    if ($NewBinaryPublished -and $DaemonSafeForBinaryRollback) {
        try {
            if (Test-Path -LiteralPath $InstalledBin) {
                Move-Item -LiteralPath $InstalledBin -Destination $FailedBin
                $NewBinaryMovedAside = $true
            }
            if ($OldBinaryBackedUp) {
                Move-Item -LiteralPath $BackupBin -Destination $InstalledBin
                $OldBinaryBackedUp = $false
                $PreviousBinaryRestored = $true
            }
            if ($NewBinaryMovedAside) {
                Remove-Item -LiteralPath $FailedBin -Force
                $NewBinaryMovedAside = $false
            }
            $NewBinaryPublished = $false
        } catch {
            $BinaryRollbackError = $_
            if ($NewBinaryMovedAside -and -not (Test-Path -LiteralPath $InstalledBin)) {
                try {
                    Move-Item -LiteralPath $FailedBin -Destination $InstalledBin
                    $NewBinaryMovedAside = $false
                } catch {
                    $BinaryRollbackError = "$BinaryRollbackError; executable path recovery also failed: $_"
                }
            }
            $RollbackErrors += "could not restore the previous binary: $BinaryRollbackError"
        }
    } elseif (-not $DaemonSafeForBinaryRollback) {
        $RollbackErrors += "the previous binary remains preserved at $BackupBin; automatic binary rollback was refused"
    } elseif ($OldBinaryBackedUp) {
        try {
            Move-Item -LiteralPath $BackupBin -Destination $InstalledBin
            $OldBinaryBackedUp = $false
            $PreviousBinaryRestored = $true
        } catch {
            $RollbackErrors += "could not restore the previous binary: $_"
        }
    }

    if ($PathMutationAttempted) {
        try {
            [Environment]::SetEnvironmentVariable("Path", $OriginalUserPath, "User")
            $PathMutationAttempted = $false
        } catch {
            $RollbackErrors += "could not restore the exact previous User PATH: $_"
        }
    }

    if ($DaemonWasRunning -and $DaemonSafeForBinaryRollback -and $PreviousBinaryRestored -and (Test-Path -LiteralPath $InstalledBin)) {
        try {
            Write-Host "[info]  Restarting the previous daemon after rollback..." -ForegroundColor Blue
            & $InstalledBin daemon start
            if ($LASTEXITCODE -ne 0) {
                throw "daemon start exited with code $LASTEXITCODE"
            }
        } catch {
            $RollbackErrors += "could not restart the previous daemon: $_"
        }
    }

    Remove-Item -LiteralPath $StagedBin -Force -ErrorAction SilentlyContinue
    if ($NewBinaryMovedAside) {
        $RollbackErrors += "failed candidate remains at $FailedBin"
    }
    if (-not $InstallDirWasPresent -and (Test-Path -LiteralPath $InstallDir) -and @(Get-ChildItem -LiteralPath $InstallDir).Count -eq 0) {
        try {
            Remove-Item -LiteralPath $InstallDir -Force
        } catch {
            $RollbackErrors += "could not remove the newly created empty install directory: $_"
        }
    }
    Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue

    if ($RollbackErrors.Count -eq 0) {
        Write-Host "[error] Installation failed and the previous binary, User PATH, and daemon state were restored: $InstallError" -ForegroundColor Red
    } else {
        Write-Host "[error] Installation failed: $InstallError. Rollback was incomplete: $($RollbackErrors -join '; ')" -ForegroundColor Red
    }
    exit 1
}

Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue

Write-Host "[info]  Installed: $VersionOutput" -ForegroundColor Blue
Write-Host "[info]  Run 'codex-switch-global-pace --help' to get started" -ForegroundColor Blue
