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
$DevVersionPattern = '\A[0-9]+\.[0-9]+\.[0-9]+-dev(?:\.|(?=\+|\z))'

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

function Get-DirectPathItem {
    param([Parameter(Mandatory = $true)][string]$Path)

    try {
        return Get-Item -LiteralPath $Path -Force -ErrorAction Stop
    } catch [System.Management.Automation.ItemNotFoundException] {
        return $null
    }
}

function Test-DirectInstallDirectory {
    param([Parameter(Mandatory = $true)][string]$Path)

    $Item = Get-DirectPathItem -Path $Path
    if ($null -eq $Item) {
        return $false
    }
    if (($Item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "Install path is a reparse point; refusing a direct install transaction: $Path"
    }
    if (-not $Item.PSIsContainer) {
        throw "Install path exists but is not a directory: $Path"
    }
    return $true
}

function Test-DirectInstalledBinary {
    param([Parameter(Mandatory = $true)][string]$Path)

    $Item = Get-DirectPathItem -Path $Path
    if ($null -eq $Item) {
        return $false
    }
    if (($Item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "Installed binary path is a reparse point; refusing a direct install transaction: $Path"
    }
    if ($Item.PSIsContainer) {
        throw "Installed binary path is not a regular file: $Path"
    }
    return $true
}

function Assert-NoInstallTransactionResidue {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Binary,
        [string]$ExpectedStagedPath
    )

    if (-not (Test-DirectInstallDirectory -Path $Path)) {
        return
    }
    $Stem = [System.IO.Path]::GetFileNameWithoutExtension($Binary)
    $ReservedNames = @(
        ".$Stem.install.exe",
        ".$Stem.rollback.exe",
        ".$Stem.failed.exe",
        ".$Stem.uninstall.exe"
    )
    $LegacyTransactionPattern = '^\.' + [regex]::Escape($Stem) + '\.(install|rollback|failed)-[0-9A-Fa-f]{32}\.exe$'
    $Residues = @(
        Get-ChildItem -LiteralPath $Path -Force |
            Where-Object {
                $IsExpectedStagedPath = -not [string]::IsNullOrEmpty($ExpectedStagedPath) -and
                    [System.StringComparer]::OrdinalIgnoreCase.Equals($_.FullName, $ExpectedStagedPath)
                -not $IsExpectedStagedPath -and (
                    $ReservedNames -contains $_.Name -or
                    $_.Name -cmatch $LegacyTransactionPattern
                )
            } |
            ForEach-Object { $_.FullName }
    )
    if ($Residues.Count -gt 0) {
        throw "An incomplete previous installer transaction was found. Refusing to overwrite or remove its recovery files: $($Residues -join ', ')"
    }
}

function Get-DirectFileSha256 {
    param([Parameter(Mandatory = $true)][string]$Path)

    $Item = Get-DirectPathItem -Path $Path
    if ($null -eq $Item) {
        return $null
    }
    if (($Item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "Transaction path is a reparse point: $Path"
    }
    if ($Item.PSIsContainer) {
        throw "Transaction path is not a regular file: $Path"
    }
    $Stream = [System.IO.File]::Open(
        $Path,
        [System.IO.FileMode]::Open,
        [System.IO.FileAccess]::Read,
        [System.IO.FileShare]::Read
    )
    try {
        $Hasher = [System.Security.Cryptography.SHA256]::Create()
        try {
            return [System.BitConverter]::ToString($Hasher.ComputeHash($Stream)).Replace("-", "").ToLowerInvariant()
        } finally {
            $Hasher.Dispose()
        }
    } finally {
        $Stream.Dispose()
    }
}

function Remove-StagedCandidate {
    param([Parameter(Mandatory = $true)][string]$Path)

    try {
        if ($null -eq (Get-DirectFileSha256 -Path $Path)) {
            return $null
        }
        Remove-Item -LiteralPath $Path -Force
        if ($null -ne (Get-DirectFileSha256 -Path $Path)) {
            return "the staged candidate still exists after removal"
        }
        return $null
    } catch {
        return "$_"
    }
}

function Invoke-AtomicUpgradePublication {
    param(
        [Parameter(Mandatory = $true)][string]$StagedPath,
        [Parameter(Mandatory = $true)][string]$InstalledPath,
        [Parameter(Mandatory = $true)][string]$BackupPath,
        [Parameter(Mandatory = $true)][string]$FailedPath,
        [Parameter(Mandatory = $true)][string]$StagedSha256,
        [Parameter(Mandatory = $true)][string]$PreviousSha256
    )

    try {
        $InstalledSha256 = Get-DirectFileSha256 -Path $InstalledPath
        $StagedSha256Before = Get-DirectFileSha256 -Path $StagedPath
        $BackupSha256 = Get-DirectFileSha256 -Path $BackupPath
        $FailedSha256 = Get-DirectFileSha256 -Path $FailedPath
    } catch {
        return [pscustomobject]@{
            State           = "Ambiguous"
            OperationError  = $null
            InspectionError = "$_"
        }
    }
    if ($InstalledSha256 -cne $PreviousSha256 -or
        $StagedSha256Before -cne $StagedSha256 -or
        $null -ne $BackupSha256 -or
        $null -ne $FailedSha256) {
        return [pscustomobject]@{
            State           = "Ambiguous"
            OperationError  = $null
            InspectionError = "publication inputs did not match the exact pre-transaction state"
        }
    }

    $OperationError = $null
    try {
        [System.IO.File]::Replace($StagedPath, $InstalledPath, $BackupPath, $true)
    } catch {
        $OperationError = "$_"
    }

    try {
        $InstalledSha256 = Get-DirectFileSha256 -Path $InstalledPath
        $StagedSha256After = Get-DirectFileSha256 -Path $StagedPath
        $BackupSha256 = Get-DirectFileSha256 -Path $BackupPath
    } catch {
        return [pscustomobject]@{
            State           = "Ambiguous"
            OperationError  = $OperationError
            InspectionError = "$_"
        }
    }

    $State = if ($InstalledSha256 -ceq $StagedSha256 -and
        $null -eq $StagedSha256After -and
        $BackupSha256 -ceq $PreviousSha256) {
        "Published"
    } elseif ($InstalledSha256 -ceq $PreviousSha256 -and
        $StagedSha256After -ceq $StagedSha256 -and
        $null -eq $BackupSha256) {
        "Unchanged"
    } else {
        "Ambiguous"
    }

    return [pscustomobject]@{
        State           = $State
        OperationError  = $OperationError
        InspectionError = $null
    }
}

function Invoke-AtomicUpgradeRollback {
    param(
        [Parameter(Mandatory = $true)][string]$InstalledPath,
        [Parameter(Mandatory = $true)][string]$BackupPath,
        [Parameter(Mandatory = $true)][string]$FailedPath,
        [Parameter(Mandatory = $true)][string]$StagedSha256,
        [Parameter(Mandatory = $true)][string]$PreviousSha256
    )

    try {
        $InstalledSha256 = Get-DirectFileSha256 -Path $InstalledPath
        $BackupSha256 = Get-DirectFileSha256 -Path $BackupPath
        $FailedSha256 = Get-DirectFileSha256 -Path $FailedPath
    } catch {
        return [pscustomobject]@{
            State           = "Ambiguous"
            OperationError  = $null
            InspectionError = "$_"
        }
    }
    if ($InstalledSha256 -cne $StagedSha256 -or
        $BackupSha256 -cne $PreviousSha256 -or
        $null -ne $FailedSha256) {
        return [pscustomobject]@{
            State           = "Ambiguous"
            OperationError  = $null
            InspectionError = "rollback inputs did not match the exact published state"
        }
    }

    $OperationError = $null
    try {
        [System.IO.File]::Replace($BackupPath, $InstalledPath, $FailedPath, $true)
    } catch {
        $OperationError = "$_"
    }

    try {
        $InstalledSha256 = Get-DirectFileSha256 -Path $InstalledPath
        $BackupSha256 = Get-DirectFileSha256 -Path $BackupPath
        $FailedSha256 = Get-DirectFileSha256 -Path $FailedPath
    } catch {
        return [pscustomobject]@{
            State           = "Ambiguous"
            OperationError  = $OperationError
            InspectionError = "$_"
        }
    }

    $State = if ($InstalledSha256 -ceq $PreviousSha256 -and
        $null -eq $BackupSha256 -and
        $FailedSha256 -ceq $StagedSha256) {
        "Restored"
    } elseif ($InstalledSha256 -ceq $StagedSha256 -and
        $BackupSha256 -ceq $PreviousSha256 -and
        $null -eq $FailedSha256) {
        "Unchanged"
    } else {
        "Ambiguous"
    }

    return [pscustomobject]@{
        State           = $State
        OperationError  = $OperationError
        InspectionError = $null
    }
}

function Invoke-AtomicUninstallStaging {
    param(
        [Parameter(Mandatory = $true)][string]$InstalledPath,
        [Parameter(Mandatory = $true)][string]$BackupPath,
        [Parameter(Mandatory = $true)][string]$InstalledSha256
    )

    try {
        $InstalledSha256Before = Get-DirectFileSha256 -Path $InstalledPath
        $BackupSha256Before = Get-DirectFileSha256 -Path $BackupPath
    } catch {
        return [pscustomobject]@{
            State           = "Ambiguous"
            OperationError  = $null
            InspectionError = "$_"
        }
    }
    if ($InstalledSha256Before -cne $InstalledSha256 -or $null -ne $BackupSha256Before) {
        return [pscustomobject]@{
            State           = "Ambiguous"
            OperationError  = $null
            InspectionError = "uninstall staging inputs did not match the exact pre-transaction state"
        }
    }

    $OperationError = $null
    try {
        [System.IO.File]::Move($InstalledPath, $BackupPath)
    } catch {
        $OperationError = "$_"
    }

    try {
        $InstalledSha256After = Get-DirectFileSha256 -Path $InstalledPath
        $BackupSha256After = Get-DirectFileSha256 -Path $BackupPath
    } catch {
        return [pscustomobject]@{
            State           = "Ambiguous"
            OperationError  = $OperationError
            InspectionError = "$_"
        }
    }

    $State = if ($null -eq $InstalledSha256After -and $BackupSha256After -ceq $InstalledSha256) {
        "Staged"
    } elseif ($InstalledSha256After -ceq $InstalledSha256 -and $null -eq $BackupSha256After) {
        "Unchanged"
    } else {
        "Ambiguous"
    }

    return [pscustomobject]@{
        State           = $State
        OperationError  = $OperationError
        InspectionError = $null
    }
}

function Invoke-AtomicUninstallRestore {
    param(
        [Parameter(Mandatory = $true)][string]$InstalledPath,
        [Parameter(Mandatory = $true)][string]$BackupPath,
        [Parameter(Mandatory = $true)][string]$InstalledSha256
    )

    try {
        $InstalledSha256Before = Get-DirectFileSha256 -Path $InstalledPath
        $BackupSha256Before = Get-DirectFileSha256 -Path $BackupPath
    } catch {
        return [pscustomobject]@{
            State           = "Ambiguous"
            OperationError  = $null
            InspectionError = "$_"
        }
    }
    if ($null -ne $InstalledSha256Before -or $BackupSha256Before -cne $InstalledSha256) {
        return [pscustomobject]@{
            State           = "Ambiguous"
            OperationError  = $null
            InspectionError = "uninstall restore inputs did not match the exact staged state"
        }
    }

    $OperationError = $null
    try {
        [System.IO.File]::Move($BackupPath, $InstalledPath)
    } catch {
        $OperationError = "$_"
    }

    try {
        $InstalledSha256After = Get-DirectFileSha256 -Path $InstalledPath
        $BackupSha256After = Get-DirectFileSha256 -Path $BackupPath
    } catch {
        return [pscustomobject]@{
            State           = "Ambiguous"
            OperationError  = $OperationError
            InspectionError = "$_"
        }
    }

    $State = if ($InstalledSha256After -ceq $InstalledSha256 -and $null -eq $BackupSha256After) {
        "Restored"
    } elseif ($null -eq $InstalledSha256After -and $BackupSha256After -ceq $InstalledSha256) {
        "Unchanged"
    } else {
        "Ambiguous"
    }

    return [pscustomobject]@{
        State           = $State
        OperationError  = $OperationError
        InspectionError = $null
    }
}

function Invoke-AtomicUninstallCommit {
    param(
        [Parameter(Mandatory = $true)][string]$InstalledPath,
        [Parameter(Mandatory = $true)][string]$BackupPath,
        [Parameter(Mandatory = $true)][string]$InstalledSha256
    )

    try {
        $InstalledSha256Before = Get-DirectFileSha256 -Path $InstalledPath
        $BackupSha256Before = Get-DirectFileSha256 -Path $BackupPath
    } catch {
        return [pscustomobject]@{
            State           = "Ambiguous"
            OperationError  = $null
            InspectionError = "$_"
        }
    }
    if ($null -ne $InstalledSha256Before -or $BackupSha256Before -cne $InstalledSha256) {
        return [pscustomobject]@{
            State           = "Ambiguous"
            OperationError  = $null
            InspectionError = "uninstall commit inputs did not match the exact staged state"
        }
    }

    $OperationError = $null
    try {
        [System.IO.File]::Delete($BackupPath)
    } catch {
        $OperationError = "$_"
    }

    try {
        $InstalledSha256After = Get-DirectFileSha256 -Path $InstalledPath
        $BackupSha256After = Get-DirectFileSha256 -Path $BackupPath
    } catch {
        return [pscustomobject]@{
            State           = "Ambiguous"
            OperationError  = $OperationError
            InspectionError = "$_"
        }
    }

    $State = if ($null -eq $InstalledSha256After -and $null -eq $BackupSha256After) {
        "Committed"
    } elseif ($null -eq $InstalledSha256After -and $BackupSha256After -ceq $InstalledSha256) {
        "Unchanged"
    } else {
        "Ambiguous"
    }

    return [pscustomobject]@{
        State           = $State
        OperationError  = $OperationError
        InspectionError = $null
    }
}

function Start-UpdateLockHolder {
    param(
        [Parameter(Mandatory = $true)][string]$CandidatePath,
        [Parameter(Mandatory = $true)][string]$DestinationPath
    )

    $StartInfo = New-Object System.Diagnostics.ProcessStartInfo
    $StartInfo.FileName = $CandidatePath
    $StartInfo.Arguments = "__hold-update-lock"
    $StartInfo.UseShellExecute = $false
    $StartInfo.CreateNoWindow = $true
    $StartInfo.RedirectStandardInput = $true
    $StartInfo.RedirectStandardOutput = $true
    $StartInfo.RedirectStandardError = $true
    $StartInfo.EnvironmentVariables["CS_UPDATE_LOCK_TARGET"] = $DestinationPath

    $LockProcess = New-Object System.Diagnostics.Process
    $LockProcess.StartInfo = $StartInfo
    try {
        [void]$LockProcess.Start()
        $ReadyLine = $LockProcess.StandardOutput.ReadLine()
        if ($ReadyLine -cne "codex-switch-global-pace update lock ready") {
            try { $LockProcess.StandardInput.Close() } catch {}
            $Exited = $LockProcess.WaitForExit(5000)
            $ExitDescription = if ($Exited) { "exit code $($LockProcess.ExitCode)" } else { "PID $($LockProcess.Id) did not exit after stdin closed" }
            $ErrorText = if ($Exited) { $LockProcess.StandardError.ReadToEnd().Trim() } else { "" }
            throw "Downloaded binary does not support the required installer transaction lock ($ExitDescription): $ErrorText"
        }
        return $LockProcess
    } catch {
        $StartError = $_
        try { $LockProcess.StandardInput.Close() } catch {}
        try { [void]$LockProcess.WaitForExit(5000) } catch {}
        try { $LockProcess.Dispose() } catch {}
        throw "Could not acquire the exclusive installer transaction lock; the existing installation was not changed: $StartError"
    }
}

function Complete-UpdateLockHolder {
    param([Parameter(Mandatory = $true)][System.Diagnostics.Process]$LockProcess)

    $ReleaseErrors = @()
    try {
        $LockProcess.StandardInput.Close()
    } catch {
        $ReleaseErrors += "could not close the lock-holder stdin pipe: $_"
    }

    $Exited = $false
    try {
        $Exited = $LockProcess.WaitForExit(10000)
        if (-not $Exited) {
            $ReleaseErrors += "lock-holder PID $($LockProcess.Id) did not exit after stdin EOF"
        }
    } catch {
        $ReleaseErrors += "could not wait for the lock-holder process: $_"
    }

    if ($Exited) {
        $RemainingOutput = $LockProcess.StandardOutput.ReadToEnd()
        $ErrorOutput = $LockProcess.StandardError.ReadToEnd()
        if ($LockProcess.ExitCode -ne 0) {
            $ReleaseErrors += "lock-holder exited with code $($LockProcess.ExitCode): $($ErrorOutput.Trim())"
        } elseif (-not [string]::IsNullOrEmpty($RemainingOutput) -or -not [string]::IsNullOrEmpty($ErrorOutput)) {
            $ReleaseErrors += "lock-holder emitted unexpected output while releasing the transaction"
        }
    }

    try { $LockProcess.Dispose() } catch {}
    if ($ReleaseErrors.Count -gt 0) {
        throw ($ReleaseErrors -join "; ")
    }
}

function Get-CheckedDaemonStatus {
    param([Parameter(Mandatory = $true)][string]$CandidatePath)

    $StatusLines = @(& $CandidatePath daemon status --installer-state 2>&1 | ForEach-Object { [string]$_ })
    $StatusExitCode = $LASTEXITCODE
    if ($StatusExitCode -ne 0) {
        throw "release-verified daemon state probe exited with code ${StatusExitCode}: $($StatusLines -join '; ')"
    }
    if ($StatusLines.Count -ne 1) {
        throw "release-verified daemon state probe returned $($StatusLines.Count) lines instead of one exact state tuple"
    }

    switch -CaseSensitive ($StatusLines[0]) {
        "running=true service_installed=true" {
            $Running = $true
            $ServiceInstalled = $true
        }
        "running=true service_installed=false" {
            $Running = $true
            $ServiceInstalled = $false
        }
        "running=false service_installed=true" {
            $Running = $false
            $ServiceInstalled = $true
        }
        "running=false service_installed=false" {
            $Running = $false
            $ServiceInstalled = $false
        }
        default {
            throw "release-verified daemon state probe returned an unsupported tuple: $($StatusLines[0])"
        }
    }

    return [pscustomobject]@{
        running = $Running
        platform = [pscustomobject]@{ service_installed = $ServiceInstalled }
    }
}

function Assert-CandidateServiceOwner {
    param(
        [Parameter(Mandatory = $true)][string]$CandidatePath,
        [Parameter(Mandatory = $true)][string]$ExpectedExecutable
    )

    $OwnerLines = @(& $CandidatePath daemon uninstall --expected-executable $ExpectedExecutable --check-owner 2>&1 | ForEach-Object { [string]$_ })
    $OwnerExitCode = $LASTEXITCODE
    if ($OwnerExitCode -ne 0) {
        throw "release-verified service ownership probe exited with code ${OwnerExitCode}: $($OwnerLines -join '; ')"
    }
    if ($OwnerLines.Count -ne 0) {
        throw "release-verified service ownership probe emitted unexpected output: $($OwnerLines -join '; ')"
    }
}

function Restore-UninstallRunningState {
    param(
        [Parameter(Mandatory = $true)][string]$BinPath,
        [Parameter(Mandatory = $true)][string]$CandidatePath,
        [Parameter(Mandatory = $true)][bool]$WasRunning,
        [Parameter(Mandatory = $true)][bool]$ServiceWasInstalled
    )

    $Current = Get-CheckedDaemonStatus -CandidatePath $CandidatePath
    if ($Current.platform.service_installed -ne $ServiceWasInstalled) {
        throw "service installation state did not match its exact pre-uninstall value"
    }
    if ($ServiceWasInstalled) {
        Assert-CandidateServiceOwner `
            -CandidatePath $CandidatePath `
            -ExpectedExecutable $BinPath
    }
    if ($WasRunning -and -not $Current.running) {
        $StartOutput = (& $CandidatePath daemon start --expected-executable $BinPath 2>&1 | Out-String)
        $StartExitCode = $LASTEXITCODE
        $Current = Get-CheckedDaemonStatus -CandidatePath $CandidatePath
        if ($StartExitCode -ne 0 -or -not $Current.running) {
            throw "daemon start did not restore the previously running daemon (exit code ${StartExitCode}): $StartOutput"
        }
    }
    if ($Current.running -ne $WasRunning) {
        throw "daemon running state did not match its exact pre-uninstall value after rollback"
    }
}

function Stop-And-ConfirmDaemonAbsent {
    param(
        [Parameter(Mandatory = $true)][string]$BinPath,
        [Parameter(Mandatory = $true)][string]$CandidatePath
    )

    $Before = Get-CheckedDaemonStatus -CandidatePath $CandidatePath
    if ($Before.platform.service_installed) {
        Assert-CandidateServiceOwner `
            -CandidatePath $CandidatePath `
            -ExpectedExecutable $BinPath
    }
    if ($Before.running -or $Before.platform.service_installed) {
        & $CandidatePath daemon stop --expected-service-executable $BinPath
        if ($LASTEXITCODE -ne 0) {
            throw "daemon stop exited with code $LASTEXITCODE"
        }
    }
    $After = Get-CheckedDaemonStatus -CandidatePath $CandidatePath
    if ($After.running) {
        throw "daemon still reports running after the stop boundary"
    }
    if ($After.platform.service_installed -ne $Before.platform.service_installed) {
        throw "daemon stop changed the service installation state"
    }
    if ($After.platform.service_installed) {
        Assert-CandidateServiceOwner `
            -CandidatePath $CandidatePath `
            -ExpectedExecutable $BinPath
    }
}

# ── Installer entrypoint: resolve and verify the release binary ──

# Detect architecture
$Arch = if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -eq "Arm64") { "arm64" } else { "amd64" }
$AssetName = "codex-switch-global-pace-windows-${Arch}.zip"

# Determine version / channel. Uninstall uses the release that packaged this
# script, so its lock helper cannot drift to another moving channel.
$Uninstall = $env:CS_UNINSTALL -eq "1"
$UseDev = $env:CS_DEV -eq "1"
if ($Uninstall) {
    if ([string]::IsNullOrWhiteSpace($PackagedReleaseVersion)) {
        throw "This uninstaller is not bound to a GitHub Release. Download install.ps1 from that Release before uninstalling."
    }
    Assert-SupportedVersion $PackagedReleaseVersion
    if ($PackagedReleaseVersion -cmatch $DevVersionPattern) {
        $UseDev = $true
        $Version = "dev"
    } else {
        $UseDev = $false
        $Version = $PackagedReleaseVersion
    }
} elseif ($UseDev) {
    $Version = "dev"
} else {
    $Version = if ($env:CS_VERSION) { $env:CS_VERSION } else { "latest" }
}

if ($Version -notin @("latest", "dev")) {
    Assert-SupportedVersion $Version
}
$DownloadUrl = if ($Version -eq "dev") {
    "https://github.com/$Repo/releases/download/dev/$AssetName"
} elseif ($Version -eq "latest") {
    "https://github.com/$Repo/releases/latest/download/$AssetName"
} else {
    "https://github.com/$Repo/releases/download/v$Version/$AssetName"
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
if ($UseDev -and $ExpectedReleaseVersion -cnotmatch $DevVersionPattern) {
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
$ActualSha256 = (Get-DirectFileSha256 -Path $ZipPath).ToUpperInvariant()
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

# ── Uninstall ────────────────────────────────────────────
$BinaryStem = [System.IO.Path]::GetFileNameWithoutExtension($BinaryName)
$StagedBin = Join-Path $InstallDir ".$BinaryStem.install.exe"
$BackupBin = Join-Path $InstallDir ".$BinaryStem.rollback.exe"
$FailedBin = Join-Path $InstallDir ".$BinaryStem.failed.exe"

if ($Uninstall) {
    Write-Host "[info]  Uninstalling codex-switch-global-pace..." -ForegroundColor Blue

    $InstalledBin = Join-Path $InstallDir $BinaryName
    $UninstallBackupBin = Join-Path $InstallDir ".$BinaryStem.uninstall.exe"
    try {
        $InstallDirWasPresent = Test-DirectInstallDirectory -Path $InstallDir
        $PreflightUserPath = [Environment]::GetEnvironmentVariable("Path", "User")
        $PreflightPathEntries = @($PreflightUserPath -split ";" | Where-Object { $_.Trim() -ne "" })
        $PreflightDaemonStatus = Get-CheckedDaemonStatus -CandidatePath $CandidateBin
    } catch {
        Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
        Write-Host "[error] Could not inspect the existing uninstall state; nothing was changed: $_" -ForegroundColor Red
        exit 1
    }

    # An absent install directory is the only state in which taking the normal
    # sibling lock would itself create persistent installation state. Confirm
    # the complete no-op state twice, then linearize the no-op at the final
    # absence check. An installer that starts afterwards is a later transaction.
    if (-not $InstallDirWasPresent -and
        $PreflightPathEntries -notcontains $InstallDir -and
        -not $PreflightDaemonStatus.running -and
        -not $PreflightDaemonStatus.platform.service_installed) {
        try {
            $ConfirmedUserPath = [Environment]::GetEnvironmentVariable("Path", "User")
            $ConfirmedPathEntries = @($ConfirmedUserPath -split ";" | Where-Object { $_.Trim() -ne "" })
            $ConfirmedDaemonStatus = Get-CheckedDaemonStatus -CandidatePath $CandidateBin
            $ConfirmedInstallDirPresent = Test-DirectInstallDirectory -Path $InstallDir
            $UninstallIsNoOp = -not $ConfirmedInstallDirPresent -and
                $ConfirmedPathEntries -notcontains $InstallDir -and
                -not $ConfirmedDaemonStatus.running -and
                -not $ConfirmedDaemonStatus.platform.service_installed
        } catch {
            Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
            Write-Host "[error] Could not confirm the no-op uninstall state; nothing was changed: $_" -ForegroundColor Red
            exit 1
        }
        if ($UninstallIsNoOp) {
            Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
            if (Test-Path -LiteralPath $DataDir) {
                Write-Host "[info]  Kept shared profile data: $DataDir" -ForegroundColor Blue
            }
            Write-Host "[info]  codex-switch-global-pace is already uninstalled." -ForegroundColor Blue
            exit 0
        }
    }

    if (-not $InstallDirWasPresent) {
        try {
            [void][System.IO.Directory]::CreateDirectory($InstallDir)
            if (-not (Test-DirectInstallDirectory -Path $InstallDir)) {
                throw "Install path was not created as a direct directory: $InstallDir"
            }
        } catch {
            Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
            Write-Host "[error] Could not create the direct install directory; nothing was changed: $_" -ForegroundColor Red
            exit 1
        }
    }

    $UninstallLockHolder = $null
    $UninstallError = $null
    $PostCommitCleanupError = $null
    $LockReleaseError = $null
    $InstalledBinaryWasPresent = $false
    $OriginalBinarySha256 = $null
    $OriginalUserPath = $null
    $RequestedUserPath = $null
    $DaemonWasRunning = $false
    $DaemonServiceInstalled = $false
    $PathMutationAttempted = $false
    $DaemonStopAttempted = $false
    $UninstallMutationAttempted = $false
    $UninstallCommitted = $false
    try {
        # A release-verified temporary executable holds the same destination
        # lease as install and self-update. The installed executable can then be
        # removed without asking a process loaded from that path to stay alive.
        $UninstallLockHolder = Start-UpdateLockHolder `
            -CandidatePath $CandidateBin `
            -DestinationPath $InstalledBin

        if (-not (Test-DirectInstallDirectory -Path $InstallDir)) {
            throw "Install directory disappeared after acquiring the shared update lock: $InstallDir"
        }
        Assert-NoInstallTransactionResidue -Path $InstallDir -Binary $BinaryName
        $InstalledBinaryWasPresent = Test-DirectInstalledBinary -Path $InstalledBin
        $OriginalBinarySha256 = if ($InstalledBinaryWasPresent) {
            Get-DirectFileSha256 -Path $InstalledBin
        } else {
            $null
        }
        if ($InstalledBinaryWasPresent -and [string]::IsNullOrEmpty($OriginalBinarySha256)) {
            throw "The installed binary disappeared before the uninstall transaction began."
        }

        # Ownership is a read-only precondition. The actual uninstall repeats
        # it at the deletion boundary, after the daemon has been stopped.
        Assert-CandidateServiceOwner `
            -CandidatePath $CandidateBin `
            -ExpectedExecutable $InstalledBin

        $OriginalUserPath = [Environment]::GetEnvironmentVariable("Path", "User")
        $OriginalPathEntries = @($OriginalUserPath -split ";" | Where-Object { $_.Trim() -ne "" })
        $InstallDirWasOnPath = $OriginalPathEntries -contains $InstallDir

        $DaemonStatus = Get-CheckedDaemonStatus -CandidatePath $CandidateBin
        $DaemonWasRunning = $DaemonStatus.running
        $DaemonServiceInstalled = $DaemonStatus.platform.service_installed

        # A loaded executable cannot be renamed on Windows. Only the stable
        # installed binary may stop its own running generation before staging;
        # a binary-missing daemon is handled as the final commit command below.
        if ($InstalledBinaryWasPresent -and $DaemonWasRunning) {
            $DaemonStopAttempted = $true
            $UninstallMutationAttempted = $true
            Stop-And-ConfirmDaemonAbsent -BinPath $InstalledBin -CandidatePath $CandidateBin
        }

        # PATH is reversible without touching the service or binary, so commit
        # it first. If any later step fails, restore the exact original string.
        if ($InstallDirWasOnPath) {
            $NewPath = ($OriginalPathEntries | Where-Object { $_ -ne $InstallDir }) -join ";"
            $RequestedUserPath = if ([string]::IsNullOrEmpty($NewPath)) { $null } else { $NewPath }
            $UserPathBeforeMutation = [Environment]::GetEnvironmentVariable("Path", "User")
            if ($UserPathBeforeMutation -cne $OriginalUserPath) {
                throw "User PATH changed after uninstall preflight; refusing to overwrite it."
            }
            $PathMutationAttempted = $true
            $UninstallMutationAttempted = $true
            [Environment]::SetEnvironmentVariable("Path", $RequestedUserPath, "User")
            $ObservedUserPath = [Environment]::GetEnvironmentVariable("Path", "User")
            if ($ObservedUserPath -cne $RequestedUserPath) {
                throw "User PATH did not match the exact requested value after removal."
            }
            Write-Host "[info]  Removed $InstallDir from user PATH" -ForegroundColor Blue
        }

        if ($InstalledBinaryWasPresent) {
            $UninstallMutationAttempted = $true
            $Staging = Invoke-AtomicUninstallStaging `
                -InstalledPath $InstalledBin `
                -BackupPath $UninstallBackupBin `
                -InstalledSha256 $OriginalBinarySha256
            if ($Staging.State -cne "Staged") {
                throw "Installed binary could not be staged for reversible removal ($($Staging.State)): operation=$($Staging.OperationError); inspection=$($Staging.InspectionError)"
            }
        } elseif (Test-DirectInstalledBinary -Path $InstalledBin) {
            throw "An installed binary appeared after the locked uninstall preflight; refusing to commit against changed state."
        }

        # Service removal is the last meaningful commit boundary. When no
        # installed binary and no service exist, a detached daemon is stopped as
        # that boundary instead; the temporary candidate is never installed.
        if ($InstalledBinaryWasPresent -or $DaemonServiceInstalled) {
            $UninstallMutationAttempted = $true
            $DaemonCleanupOutput = (& $CandidateBin daemon uninstall --expected-executable $InstalledBin 2>&1 | Out-String)
            if ($LASTEXITCODE -ne 0) {
                throw "daemon service cleanup exited with code ${LASTEXITCODE}: $DaemonCleanupOutput"
            }
        } elseif ($DaemonWasRunning) {
            $UninstallMutationAttempted = $true
            $DaemonCleanupOutput = (& $CandidateBin daemon stop 2>&1 | Out-String)
            if ($LASTEXITCODE -ne 0) {
                throw "detached daemon cleanup exited with code ${LASTEXITCODE}: $DaemonCleanupOutput"
            }
        }
        $CommittedDaemonStatus = Get-CheckedDaemonStatus -CandidatePath $CandidateBin
        if ($CommittedDaemonStatus.running -or
            $CommittedDaemonStatus.platform.service_installed) {
            throw "daemon cleanup returned without removing the exact running/service state"
        }
        $UninstallCommitted = $true
        if ($InstalledBinaryWasPresent -or $DaemonServiceInstalled) {
            Write-Host "[info]  Daemon scheduled task cleanup completed." -ForegroundColor Blue
        }

        # The official executable path, PATH, and service are now absent. The
        # recovery backup is post-commit cleanup: failure must not recreate a
        # potentially different Task XML definition.
        if ($InstalledBinaryWasPresent) {
            try {
                $Commit = Invoke-AtomicUninstallCommit `
                    -InstalledPath $InstalledBin `
                    -BackupPath $UninstallBackupBin `
                    -InstalledSha256 $OriginalBinarySha256
                if ($Commit.State -cne "Committed") {
                    $PostCommitCleanupError = "verified binary backup cleanup was $($Commit.State): operation=$($Commit.OperationError); inspection=$($Commit.InspectionError)"
                } else {
                    Write-Host "[info]  Removed $InstalledBin" -ForegroundColor Blue
                }
            } catch {
                $PostCommitCleanupError = "verified binary backup cleanup raised an exception: $_"
            }
        }
    } catch {
        $UninstallFailure = $_
        if ($UninstallCommitted) {
            $PostCommitCleanupError = "post-commit cleanup or reporting failed: $UninstallFailure"
        } elseif (-not $UninstallMutationAttempted) {
            $UninstallError = "Uninstall preflight failed before any binary, PATH, or daemon mutation: $UninstallFailure"
        } else {
            $RollbackErrors = @()

            # Restore exact binary bytes before restarting a daemon generation.
            # The Rust service transaction restores its original Task XML on
            # failure; the script never recreates that definition.
            $StableRollbackBinary = $false
            if ($InstalledBinaryWasPresent) {
                try {
                    $InstalledSha256AfterFailure = Get-DirectFileSha256 -Path $InstalledBin
                    $BackupSha256AfterFailure = Get-DirectFileSha256 -Path $UninstallBackupBin
                    if ($InstalledSha256AfterFailure -ceq $OriginalBinarySha256 -and
                        $null -eq $BackupSha256AfterFailure) {
                        $StableRollbackBinary = $true
                    } elseif ($null -eq $InstalledSha256AfterFailure -and
                        $BackupSha256AfterFailure -ceq $OriginalBinarySha256) {
                        $Restore = Invoke-AtomicUninstallRestore `
                            -InstalledPath $InstalledBin `
                            -BackupPath $UninstallBackupBin `
                            -InstalledSha256 $OriginalBinarySha256
                        if ($Restore.State -ceq "Restored") {
                            $StableRollbackBinary = $true
                        } else {
                            $RollbackErrors += "binary restoration was $($Restore.State): operation=$($Restore.OperationError); inspection=$($Restore.InspectionError)"
                        }
                    } else {
                        $RollbackErrors += "binary state was ambiguous; installed=$InstalledSha256AfterFailure backup=$BackupSha256AfterFailure"
                    }
                } catch {
                    $RollbackErrors += "binary rollback inspection failed: $_"
                }
            } else {
                try {
                    $InstalledSha256AfterFailure = Get-DirectFileSha256 -Path $InstalledBin
                    $BackupSha256AfterFailure = Get-DirectFileSha256 -Path $UninstallBackupBin
                    if ($null -eq $InstalledSha256AfterFailure -and
                        $null -eq $BackupSha256AfterFailure) {
                        $StableRollbackBinary = $true
                    } else {
                        $RollbackErrors += "binary paths changed after an absent-binary preflight; installed=$InstalledSha256AfterFailure backup=$BackupSha256AfterFailure"
                    }
                } catch {
                    $RollbackErrors += "absent-binary rollback inspection failed: $_"
                }
            }

            if ($PathMutationAttempted) {
                try {
                    $RestoredUserPath = [Environment]::GetEnvironmentVariable("Path", "User")
                    if ($RestoredUserPath -ceq $RequestedUserPath) {
                        [Environment]::SetEnvironmentVariable("Path", $OriginalUserPath, "User")
                        $RestoredUserPath = [Environment]::GetEnvironmentVariable("Path", "User")
                    } elseif ($RestoredUserPath -cne $OriginalUserPath) {
                        throw "User PATH changed independently after uninstall mutation; refusing to overwrite it during rollback."
                    }
                    if ($RestoredUserPath -cne $OriginalUserPath) {
                        throw "User PATH did not match its exact pre-uninstall value after restoration."
                    }
                } catch {
                    $RollbackErrors += "user PATH restoration failed: $_"
                }
            }

            if ($StableRollbackBinary) {
                try {
                    Restore-UninstallRunningState `
                        -BinPath $InstalledBin `
                        -CandidatePath $CandidateBin `
                        -WasRunning $DaemonWasRunning `
                        -ServiceWasInstalled $DaemonServiceInstalled
                } catch {
                    $RollbackErrors += "daemon running/service-state restoration failed: $_"
                }
            } elseif ($DaemonWasRunning -or $DaemonServiceInstalled) {
                $RollbackErrors += "daemon running/service state could not be restored without the exact installed binary state"
            }

            if ($RollbackErrors.Count -eq 0) {
                $UninstallError = "The uninstall did not commit, and the exact pre-uninstall binary, PATH, and running state were restored: $UninstallFailure"
            } else {
                $UninstallError = "The uninstall did not commit and rollback was incomplete: $UninstallFailure. $($RollbackErrors -join '; ')"
            }
        }
    } finally {
        if ($null -ne $UninstallLockHolder) {
            try {
                Complete-UpdateLockHolder -LockProcess $UninstallLockHolder
            } catch {
                $LockReleaseError = $_
            }
        }
        Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
    }

    if ($null -ne $UninstallError) {
        $Suffix = if ($null -ne $LockReleaseError) {
            " Additionally, the exclusive update lock did not close cleanly: $LockReleaseError"
        } else {
            ""
        }
        Write-Host "[error] Uninstall did not complete: $UninstallError$Suffix" -ForegroundColor Red
        exit 1
    }
    if ($null -ne $PostCommitCleanupError) {
        $Suffix = if ($null -ne $LockReleaseError) {
            " Additionally, the exclusive update lock did not close cleanly: $LockReleaseError"
        } else {
            ""
        }
        Write-Host "[error] Uninstall committed, but post-commit cleanup could not be confirmed. Official executable path: $InstalledBin. Recovery residue path: $UninstallBackupBin. $PostCommitCleanupError$Suffix" -ForegroundColor Red
        exit 1
    }
    if ($null -ne $LockReleaseError) {
        Write-Host "[error] Uninstall completed, but the exclusive update lock did not close cleanly: $LockReleaseError" -ForegroundColor Red
        exit 1
    }

    # This directory is deliberately shared with codex-switch so existing
    # profiles work without another login. Never remove it from this uninstaller.
    if (Test-Path -LiteralPath $DataDir) {
        Write-Host "[info]  Kept shared profile data: $DataDir" -ForegroundColor Blue
    }
    Write-Host "[info]  codex-switch-global-pace has been uninstalled." -ForegroundColor Blue
    exit 0
}

# Stage the verified candidate beside the installed executable before stopping a
# working daemon. Publication is a same-directory atomic move or replacement,
# so a download drive and the install drive cannot turn it into a partial
# cross-volume operation.
$InstalledBin = Join-Path $InstallDir $BinaryName
try {
    $InstallDirWasPresent = Test-DirectInstallDirectory -Path $InstallDir
} catch {
    Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
    Write-Host "[error] $_" -ForegroundColor Red
    exit 1
}

$UpdateLockHolder = $null

try {
    if (-not $InstallDirWasPresent) {
        [void][System.IO.Directory]::CreateDirectory($InstallDir)
        if (-not (Test-DirectInstallDirectory -Path $InstallDir)) {
            throw "Install path was not created as a directory: $InstallDir"
        }
    }
    $UpdateLockHolder = Start-UpdateLockHolder `
        -CandidatePath $CandidateBin `
        -DestinationPath $InstalledBin
} catch {
    $LockError = $_
    if (-not $InstallDirWasPresent -and (Test-Path -LiteralPath $InstallDir) -and @(Get-ChildItem -LiteralPath $InstallDir).Count -eq 0) {
        Remove-Item -LiteralPath $InstallDir -Force -ErrorAction SilentlyContinue
    }
    Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
    Write-Host "[error] $LockError" -ForegroundColor Red
    exit 1
}

$TransactionSucceeded = $false
try {
    # Re-read destination state only after acquiring the shared lease. A
    # concurrent first install may have published the binary while this process
    # was waiting.
    if (-not (Test-DirectInstallDirectory -Path $InstallDir)) {
        throw "Install directory disappeared after acquiring the shared update lock: $InstallDir"
    }
    Assert-NoInstallTransactionResidue -Path $InstallDir -Binary $BinaryName
    $ExistingBinaryWasPresent = Test-DirectInstalledBinary -Path $InstalledBin
    $OriginalUserPath = [Environment]::GetEnvironmentVariable("Path", "User")

    try {
    Copy-Item -LiteralPath $CandidateBin -Destination $StagedBin
    $StagedVersionOutput = & $StagedBin --version 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "staged candidate version check exited with code ${LASTEXITCODE}: $StagedVersionOutput"
    }
    $StagedVersionLine = (($StagedVersionOutput | Select-Object -First 1) -as [string]).Trim()
    if ($StagedVersionLine -cne "codex-switch-global-pace $ExpectedReleaseVersion") {
        throw "staged candidate reported '$StagedVersionLine', expected 'codex-switch-global-pace $ExpectedReleaseVersion'"
    }
    $StagedBinarySha256 = Get-DirectFileSha256 -Path $StagedBin
    $PreviousBinarySha256 = if ($ExistingBinaryWasPresent) {
        Get-DirectFileSha256 -Path $InstalledBin
    } else {
        $null
    }
    if ([string]::IsNullOrEmpty($StagedBinarySha256) -or
        ($ExistingBinaryWasPresent -and [string]::IsNullOrEmpty($PreviousBinarySha256))) {
        throw "A staged or existing binary disappeared before atomic replacement."
    }
    } catch {
        $StageError = $_
        $StageCleanupError = Remove-StagedCandidate -Path $StagedBin
        if (-not $InstallDirWasPresent -and (Test-Path -LiteralPath $InstallDir) -and @(Get-ChildItem -LiteralPath $InstallDir).Count -eq 0) {
            Remove-Item -LiteralPath $InstallDir -Force -ErrorAction SilentlyContinue
        }
        $CleanupSuffix = if ($null -ne $StageCleanupError) {
            " The staged candidate was preserved at ${StagedBin}: $StageCleanupError"
        } else {
            ""
        }
        Write-Host "[error] Could not stage the verified binary; the existing installation was not changed: $StageError.$CleanupSuffix" -ForegroundColor Red
        exit 1
    }

# A running daemon holds the executable open. Its state is part of this
# transaction, so an unreadable or malformed status is an error rather than a
# guess that it is stopped.
$DaemonWasRunning = $false
$DaemonServiceInstalled = $false
try {
    $DaemonStatus = Get-CheckedDaemonStatus -CandidatePath $CandidateBin
    $DaemonWasRunning = $DaemonStatus.running
    $DaemonServiceInstalled = $DaemonStatus.platform.service_installed
    if (-not $ExistingBinaryWasPresent -and
        ($DaemonWasRunning -or $DaemonServiceInstalled)) {
        throw "The installed binary is absent while daemon/service state still exists. Restore or explicitly uninstall that exact installation before retrying."
    }
    if ($DaemonServiceInstalled) {
        Assert-CandidateServiceOwner `
            -CandidatePath $CandidateBin `
            -ExpectedExecutable $InstalledBin
    }
} catch {
    $StatusError = $_
    $StageCleanupError = Remove-StagedCandidate -Path $StagedBin
    if (-not $InstallDirWasPresent -and @(Get-ChildItem -LiteralPath $InstallDir).Count -eq 0) {
        Remove-Item -LiteralPath $InstallDir -Force -ErrorAction SilentlyContinue
    }
    $CleanupSuffix = if ($null -ne $StageCleanupError) {
        " The staged candidate was preserved at ${StagedBin}: $StageCleanupError"
    } else {
        ""
    }
    Write-Host "[error] Could not validate the existing daemon/service state; nothing was replaced: $StatusError.$CleanupSuffix" -ForegroundColor Red
    exit 1
}

if ($DaemonWasRunning -or $DaemonServiceInstalled) {
    Write-Host "[info]  Stopping the existing daemon task before upgrade..." -ForegroundColor Blue
    try {
        Stop-And-ConfirmDaemonAbsent -BinPath $InstalledBin -CandidatePath $CandidateBin
    } catch {
        $StageCleanupError = Remove-StagedCandidate -Path $StagedBin
        $CleanupSuffix = if ($null -ne $StageCleanupError) {
            " The staged candidate was preserved at ${StagedBin}: $StageCleanupError"
        } else {
            ""
        }
        Write-Host "[error] The existing daemon could not be stopped safely: $_. The installed binary was not replaced.$CleanupSuffix" -ForegroundColor Red
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
$AmbiguousBinaryState = $false
try {
    try {
        if (-not (Test-DirectInstallDirectory -Path $InstallDir)) {
            throw "Install directory disappeared before binary publication."
        }
        Assert-NoInstallTransactionResidue `
            -Path $InstallDir `
            -Binary $BinaryName `
            -ExpectedStagedPath $StagedBin
        if ($DaemonServiceInstalled) {
            Assert-CandidateServiceOwner `
                -CandidatePath $CandidateBin `
                -ExpectedExecutable $InstalledBin
        }
        if (-not $ExistingBinaryWasPresent -and
            ((Test-DirectInstalledBinary -Path $InstalledBin) -or
                (Get-DirectFileSha256 -Path $StagedBin) -cne $StagedBinarySha256)) {
            throw "First-install transaction files changed before binary publication."
        }
    } catch {
        $AmbiguousBinaryState = $true
        throw
    }

    if ($ExistingBinaryWasPresent) {
        $Publication = Invoke-AtomicUpgradePublication `
            -StagedPath $StagedBin `
            -InstalledPath $InstalledBin `
            -BackupPath $BackupBin `
            -FailedPath $FailedBin `
            -StagedSha256 $StagedBinarySha256 `
            -PreviousSha256 $PreviousBinarySha256

        if ($Publication.State -ceq "Published") {
            $OldBinaryBackedUp = $true
            $PreviousBinaryRestored = $false
            $NewBinaryPublished = $true
            if ($null -ne $Publication.OperationError) {
                throw "Atomic binary replacement reported an error after publishing exact bytes; rolling back: $($Publication.OperationError)"
            }
        } elseif ($Publication.State -ceq "Unchanged") {
            $Reason = if ($null -ne $Publication.OperationError) {
                $Publication.OperationError
            } else {
                "the operation returned without publishing the candidate"
            }
            throw "Atomic binary replacement failed before publication: $Reason"
        } else {
            $AmbiguousBinaryState = $true
            $Details = @()
            if ($null -ne $Publication.OperationError) {
                $Details += "operation error: $($Publication.OperationError)"
            }
            if ($null -ne $Publication.InspectionError) {
                $Details += "inspection error: $($Publication.InspectionError)"
            }
            $DetailSuffix = if ($Details.Count -gt 0) { " ($($Details -join '; '))" } else { "" }
            throw "Atomic binary replacement produced an ambiguous file state$DetailSuffix. The installed, staged, and rollback paths were preserved for explicit recovery."
        }
    } else {
        Move-Item -LiteralPath $StagedBin -Destination $InstalledBin
        $NewBinaryPublished = $true
        try {
            if ((Get-DirectFileSha256 -Path $InstalledBin) -cne $StagedBinarySha256 -or
                $null -ne (Get-DirectFileSha256 -Path $StagedBin)) {
                throw "First-install publication did not produce the exact expected executable."
            }
        } catch {
            $AmbiguousBinaryState = $true
            throw
        }
    }

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

    # If this final proof fails, rollback must first prove that no process from
    # the replacement generation still owns the executable.
    $DaemonRestartAttempted = $true
    $FinalDaemonStatus = Get-CheckedDaemonStatus -CandidatePath $CandidateBin
    if ($FinalDaemonStatus.running -ne $DaemonWasRunning -or
        $FinalDaemonStatus.platform.service_installed -ne $DaemonServiceInstalled) {
        throw "daemon running/service state did not match its exact pre-upgrade value"
    }
    if ($DaemonServiceInstalled) {
        Assert-CandidateServiceOwner `
            -CandidatePath $CandidateBin `
            -ExpectedExecutable $InstalledBin
    }

    if ($OldBinaryBackedUp) {
        try {
            if ((Get-DirectFileSha256 -Path $InstalledBin) -cne $StagedBinarySha256 -or
                (Get-DirectFileSha256 -Path $BackupBin) -cne $PreviousBinarySha256) {
                throw "Binary state changed before the rollback backup commit boundary."
            }
        } catch {
            $AmbiguousBinaryState = $true
            throw
        }
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
            Stop-And-ConfirmDaemonAbsent -BinPath $InstalledBin -CandidatePath $CandidateBin
            $DaemonRestarted = $false
        } catch {
            $DaemonSafeForBinaryRollback = $false
            $RollbackErrors += "could not prove the new daemon/task was stopped: $_"
        }
    }

    $NewBinaryMovedAside = $false
    if ($AmbiguousBinaryState) {
        $RollbackErrors += "binary publication state is ambiguous; the installed, staged, rollback, and failed paths were preserved for explicit recovery"
    } elseif (-not $DaemonSafeForBinaryRollback) {
        $RollbackErrors += "the previous binary remains preserved at $BackupBin; automatic binary rollback was refused"
    } elseif ($NewBinaryPublished -and $OldBinaryBackedUp) {
        $Rollback = Invoke-AtomicUpgradeRollback `
            -InstalledPath $InstalledBin `
            -BackupPath $BackupBin `
            -FailedPath $FailedBin `
            -StagedSha256 $StagedBinarySha256 `
            -PreviousSha256 $PreviousBinarySha256

        if ($Rollback.State -ceq "Restored") {
            $OldBinaryBackedUp = $false
            $PreviousBinaryRestored = $true
            $NewBinaryPublished = $false
            $NewBinaryMovedAside = $true

            if ($null -ne $Rollback.OperationError) {
                $RollbackErrors += "atomic rollback reported an error after restoring the exact previous bytes; the failed candidate was preserved at ${FailedBin}: $($Rollback.OperationError)"
            } else {
                try {
                    Remove-Item -LiteralPath $FailedBin -Force
                    $NewBinaryMovedAside = $false
                } catch {
                    $RollbackErrors += "the previous binary was restored, but the failed candidate could not be removed from ${FailedBin}: $_"
                }
            }
        } elseif ($Rollback.State -ceq "Unchanged") {
            $Reason = if ($null -ne $Rollback.OperationError) {
                $Rollback.OperationError
            } else {
                "the operation returned without restoring the previous binary"
            }
            $RollbackErrors += "atomic rollback failed before restoring the previous binary: $Reason"
        } else {
            $AmbiguousBinaryState = $true
            $Details = @()
            if ($null -ne $Rollback.OperationError) {
                $Details += "operation error: $($Rollback.OperationError)"
            }
            if ($null -ne $Rollback.InspectionError) {
                $Details += "inspection error: $($Rollback.InspectionError)"
            }
            $DetailSuffix = if ($Details.Count -gt 0) { ": $($Details -join '; ')" } else { "" }
            $RollbackErrors += "atomic rollback produced an ambiguous file state; all transaction paths were preserved$DetailSuffix"
        }
    } elseif ($NewBinaryPublished) {
        try {
            Move-Item -LiteralPath $InstalledBin -Destination $FailedBin
            $NewBinaryMovedAside = $true
            if ($null -ne (Get-DirectFileSha256 -Path $InstalledBin) -or
                (Get-DirectFileSha256 -Path $FailedBin) -cne $StagedBinarySha256) {
                throw "First-install rollback did not isolate the exact failed candidate."
            }
            Remove-Item -LiteralPath $FailedBin -Force
            $NewBinaryMovedAside = $false
            $NewBinaryPublished = $false
            $PreviousBinaryRestored = $true
        } catch {
            $RollbackErrors += "could not remove the failed first-install candidate safely: $_"
        }
    } elseif ($OldBinaryBackedUp) {
        $RollbackErrors += "rollback backup exists without a classified published binary; it was preserved at $BackupBin"
        $AmbiguousBinaryState = $true
    }

    if ($AmbiguousBinaryState) {
        $PreviousBinaryRestored = $false
    }

    if ($PathMutationAttempted) {
        try {
            [Environment]::SetEnvironmentVariable("Path", $OriginalUserPath, "User")
            $PathMutationAttempted = $false
        } catch {
            $RollbackErrors += "could not restore the exact previous User PATH: $_"
        }
    }

    if (-not $AmbiguousBinaryState -and $DaemonWasRunning -and $DaemonSafeForBinaryRollback -and $PreviousBinaryRestored -and (Test-Path -LiteralPath $InstalledBin)) {
        try {
            Write-Host "[info]  Restarting the previous daemon after rollback..." -ForegroundColor Blue
            & $CandidateBin daemon start --expected-executable $InstalledBin
            if ($LASTEXITCODE -ne 0) {
                throw "daemon start exited with code $LASTEXITCODE"
            }
        } catch {
            $RollbackErrors += "could not restart the previous daemon: $_"
        }
    }

    if (-not $AmbiguousBinaryState -and $DaemonSafeForBinaryRollback -and
        $PreviousBinaryRestored) {
        try {
            $RestoredDaemonStatus = Get-CheckedDaemonStatus -CandidatePath $CandidateBin
            if ($RestoredDaemonStatus.running -ne $DaemonWasRunning -or
                $RestoredDaemonStatus.platform.service_installed -ne $DaemonServiceInstalled) {
                throw "daemon running/service state did not match its exact pre-upgrade value after rollback"
            }
            if ($DaemonServiceInstalled) {
                Assert-CandidateServiceOwner `
                    -CandidatePath $CandidateBin `
                    -ExpectedExecutable $InstalledBin
            }
        } catch {
            $RollbackErrors += "could not verify the exact daemon state after rollback: $_"
        }
    }

    if (-not $AmbiguousBinaryState) {
        $StageCleanupError = Remove-StagedCandidate -Path $StagedBin
        if ($null -ne $StageCleanupError) {
            $RollbackErrors += "staged candidate remains preserved at ${StagedBin}: $StageCleanupError"
        }
    }
    if ($NewBinaryMovedAside) {
        $RollbackErrors += "failed candidate remains at $FailedBin"
    }
    if (-not $AmbiguousBinaryState -and -not $InstallDirWasPresent -and (Test-Path -LiteralPath $InstallDir) -and @(Get-ChildItem -LiteralPath $InstallDir).Count -eq 0) {
        try {
            Remove-Item -LiteralPath $InstallDir -Force
        } catch {
            $RollbackErrors += "could not remove the newly created empty install directory: $_"
        }
    }
    if ($RollbackErrors.Count -eq 0) {
        Write-Host "[error] Installation failed and the previous binary, User PATH, and daemon state were restored: $InstallError" -ForegroundColor Red
    } else {
        Write-Host "[error] Installation failed: $InstallError. Rollback was incomplete: $($RollbackErrors -join '; ')" -ForegroundColor Red
    }
    exit 1
}
$TransactionSucceeded = $true
} finally {
    $LockReleaseError = $null
    try {
        Complete-UpdateLockHolder -LockProcess $UpdateLockHolder
    } catch {
        $LockReleaseError = $_
    }
    Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
    if ($null -ne $LockReleaseError) {
        $LockMessage = "The exclusive update lock did not close cleanly: $LockReleaseError"
        if ($TransactionSucceeded) {
            throw "Installer transaction completed, but $LockMessage"
        }
        Write-Host "[error] Additionally, $LockMessage" -ForegroundColor Red
    }
}

Write-Host "[info]  Installed: $VersionOutput" -ForegroundColor Blue
Write-Host "[info]  Run 'codex-switch-global-pace --help' to get started" -ForegroundColor Blue
