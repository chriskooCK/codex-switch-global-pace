# codex-switch-global-pace installer / uninstaller for Windows
# Usage:
#   irm https://github.com/chriskooCK/codex-switch-global-pace/releases/latest/download/install.ps1 | iex
#   $env:CS_DEV="1"; irm https://github.com/chriskooCK/codex-switch-global-pace/releases/download/dev/install.ps1 | iex
#   $env:CS_VERSION="20260712.1.0"; irm .../install.ps1 | iex # install specific version
#   $env:CS_UNINSTALL="1"; irm .../install.ps1 | iex         # uninstall this program

& {
$ErrorActionPreference = "Stop"
$TmpDir = $null
$InstallerFailure = $null
$TempCleanupError = $null
$Repo = "chriskooCK/codex-switch-global-pace"
$PackagedReleaseVersion = ""
$BinaryName = "codex-switch-global-pace.exe"
$InstallDir = Join-Path $env:LOCALAPPDATA "Programs\codex-switch-global-pace"
$DataDir = Join-Path $env:USERPROFILE ".codex-switch"
$SemVerPattern = '\A(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-((0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)(\.(0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*))*))?(\+([0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*))?\z'
$DevVersionPattern = '\A[0-9]+\.[0-9]+\.[0-9]+-dev(?:\.|(?=\+|\z))'
$RecoveryNameCollisionLimit = 16
$UpdateLockStartupExitTimeoutMilliseconds = 5000
$UpdateLockReleaseExitTimeoutMilliseconds = 10000
$DaemonBoundaryPrefix = "codex-switch-global-pace daemon update boundary"
$DaemonBoundaryExitTimeoutMilliseconds = 10000

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

function Remove-NewEmptyInstallDirectory {
    param([Parameter(Mandatory = $true)][string]$Path)

    $Item = Get-DirectPathItem -Path $Path
    if ($null -eq $Item) {
        return
    }
    if (($Item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0 -or
        -not $Item.PSIsContainer) {
        throw "New install directory path changed type and was preserved: $Path"
    }
    $Children = @(Get-ChildItem -LiteralPath $Path -Force -ErrorAction Stop)
    if ($Children.Count -ne 0) {
        throw "New install directory is no longer empty and was preserved: $Path"
    }
    [System.IO.Directory]::Delete($Item.FullName, $false)
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

function New-InstallerRecoveryPath {
    param(
        [Parameter(Mandatory = $true)][string]$Directory,
        [Parameter(Mandatory = $true)][string]$Stem,
        [Parameter(Mandatory = $true)][ValidateSet("displaced", "failed")][string]$Role
    )

    $Generator = [System.Security.Cryptography.RandomNumberGenerator]::Create()
    try {
        for ($Attempt = 0; $Attempt -lt $RecoveryNameCollisionLimit; $Attempt++) {
            $Nonce = New-Object byte[] 16
            $Generator.GetBytes($Nonce)
            $Hex = [System.BitConverter]::ToString($Nonce).Replace("-", "").ToLowerInvariant()
            $Path = Join-Path $Directory ".$Stem.$Role-$Hex.exe"
            if ($null -eq (Get-DirectPathItem -Path $Path)) {
                return $Path
            }
        }
    } finally {
        $Generator.Dispose()
    }
    throw "Could not allocate a fresh random $Role recovery name after $RecoveryNameCollisionLimit collisions."
}

function Assert-NoInstallTransactionResidue {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Binary,
        [string]$ExpectedStagedPath,
        [string]$ExpectedBackupPath
    )

    if (-not (Test-DirectInstallDirectory -Path $Path)) {
        return
    }
    $Stem = [System.IO.Path]::GetFileNameWithoutExtension($Binary)
    $ReservedNames = @(
        ".$Stem.install.exe",
        ".$Stem.rollback.exe",
        ".$Stem.displaced.exe",
        ".$Stem.failed.exe",
        ".$Stem.uninstall.exe"
    )
    $LegacyTransactionPattern = '^\.' + [regex]::Escape($Stem) + '\.(install|rollback|failed)-[0-9A-Fa-f]{32}\.exe$'
    $CurrentRecoveryPattern = '^\.' + [regex]::Escape($Stem) + '\.(displaced|failed)-[0-9a-f]{32}\.exe$'
    $Residues = @(
        Get-ChildItem -LiteralPath $Path -Force |
            Where-Object {
                $IsExpectedStagedPath = -not [string]::IsNullOrEmpty($ExpectedStagedPath) -and
                    [System.StringComparer]::OrdinalIgnoreCase.Equals($_.FullName, $ExpectedStagedPath)
                $IsExpectedBackupPath = -not [string]::IsNullOrEmpty($ExpectedBackupPath) -and
                    [System.StringComparer]::OrdinalIgnoreCase.Equals($_.FullName, $ExpectedBackupPath)
                -not $IsExpectedStagedPath -and -not $IsExpectedBackupPath -and (
                    $ReservedNames -contains $_.Name -or
                    $_.Name -cmatch $LegacyTransactionPattern -or
                    $_.Name -cmatch $CurrentRecoveryPattern -or
                    $_.Name -cmatch '^\.codex-switch-global-pace\.installer-quarantine-[0-9a-f]{32}$'
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

function Invoke-InstallerFileOperation {
    param(
        [Parameter(Mandatory = $true)][string]$CandidatePath,
        [Parameter(Mandatory = $true)][string]$Operation,
        [string]$Source,
        [string]$Destination,
        [string]$Displaced,
        [string]$ExpectedToken,
        [string]$ExpectedDestinationToken
    )

    $Arguments = @("__installer-file-op", $Operation)
    if (-not [string]::IsNullOrEmpty($Source)) {
        $Arguments += @("--source", $Source)
    }
    if (-not [string]::IsNullOrEmpty($Destination)) {
        $Arguments += @("--destination", $Destination)
    }
    if (-not [string]::IsNullOrEmpty($Displaced)) {
        $Arguments += @("--displaced", $Displaced)
    }
    if (-not [string]::IsNullOrEmpty($ExpectedToken)) {
        $Arguments += @("--expected-token", $ExpectedToken)
    }
    if (-not [string]::IsNullOrEmpty($ExpectedDestinationToken)) {
        $Arguments += @("--expected-destination-token", $ExpectedDestinationToken)
    }

    try {
        $Lines = @(& $CandidatePath @Arguments 2>&1 | ForEach-Object { [string]$_ })
        $ExitCode = $LASTEXITCODE
    } catch {
        return [pscustomobject]@{
            Succeeded = $false
            Result = $null
            Error = "installer helper process could not be invoked: $_"
        }
    }
    if ($ExitCode -ne 0) {
        return [pscustomobject]@{
            Succeeded = $false
            Result = $null
            Error = "installer helper exited with code ${ExitCode}: $($Lines -join '; ')"
        }
    }
    if ($Lines.Count -ne 1 -or [string]::IsNullOrWhiteSpace($Lines[0])) {
        return [pscustomobject]@{
            Succeeded = $false
            Result = $null
            Error = "installer helper returned $($Lines.Count) lines instead of one machine-readable result"
        }
    }
    return [pscustomobject]@{
        Succeeded = $true
        Result = $Lines[0]
        Error = $null
    }
}

function Invoke-RequiredInstallerFileOperation {
    param(
        [Parameter(Mandatory = $true)][string]$CandidatePath,
        [Parameter(Mandatory = $true)][string]$Operation,
        [string]$Source,
        [string]$Destination,
        [string]$Displaced,
        [string]$ExpectedToken,
        [string]$ExpectedDestinationToken
    )

    $Result = Invoke-InstallerFileOperation @PSBoundParameters
    if (-not $Result.Succeeded) {
        throw $Result.Error
    }
    return $Result.Result
}

function Get-InstallerFileToken {
    param(
        [Parameter(Mandatory = $true)][string]$CandidatePath,
        [Parameter(Mandatory = $true)][string]$Path
    )

    $Token = Invoke-RequiredInstallerFileOperation `
        -CandidatePath $CandidatePath `
        -Operation "token" `
        -Source $Path
    if ($Token -cnotmatch '^[0-9]+:[0-9]+\|[0-9a-f]{64}$') {
        throw "installer helper returned an invalid file token for ${Path}: $Token"
    }
    return $Token
}

function Get-InstallerFileTokenIfPresent {
    param(
        [Parameter(Mandatory = $true)][string]$CandidatePath,
        [Parameter(Mandatory = $true)][string]$Path
    )

    if ($null -eq (Get-DirectPathItem -Path $Path)) {
        return $null
    }
    return Get-InstallerFileToken -CandidatePath $CandidatePath -Path $Path
}

function Remove-InstallerOwnedFile {
    param(
        [Parameter(Mandatory = $true)][string]$CandidatePath,
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$ExpectedToken
    )

    $Outcome = Invoke-RequiredInstallerFileOperation `
        -CandidatePath $CandidatePath `
        -Operation "remove-owned" `
        -Source $Path `
        -ExpectedToken $ExpectedToken
    switch -CaseSensitive ($Outcome) {
        "removed" { return }
        "removed-namespace-durability-unconfirmed" {
            Write-Host "[warn]  Removed exact owned file at $Path, but directory durability was not confirmed." -ForegroundColor Yellow
            return
        }
        default { throw "installer helper returned an unknown removal outcome for ${Path}: $Outcome" }
    }
}

function ConvertFrom-InstallerCreateOutcome {
    param(
        [Parameter(Mandatory = $true)][string]$CandidatePath,
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Outcome
    )

    if ($Outcome -cnotmatch '^(?<state>created|created-namespace-durability-unconfirmed)\|(?<token>[0-9]+:[0-9]+\|[0-9a-f]{64})$') {
        throw "installer helper returned an invalid creation outcome for ${Path}: $Outcome"
    }
    $State = $Matches.state
    $Token = $Matches.token
    if ($State -ceq "created-namespace-durability-unconfirmed") {
        $CleanupError = $null
        try {
            Remove-InstallerOwnedFile `
                -CandidatePath $CandidatePath `
                -Path $Path `
                -ExpectedToken $Token
        } catch {
            $CleanupError = $_
        }
        $Suffix = if ($null -eq $CleanupError) {
            " The exact staged file was removed."
        } else {
            " Exact cleanup failed and the token-bound residue was preserved at ${Path}: $CleanupError"
        }
        throw "Creation reached ${Path}, but directory durability was not confirmed.$Suffix"
    }
    return $Token
}

function Copy-InstallerFileExclusive {
    param(
        [Parameter(Mandatory = $true)][string]$CandidatePath,
        [Parameter(Mandatory = $true)][string]$Source,
        [Parameter(Mandatory = $true)][string]$Destination,
        [Parameter(Mandatory = $true)][string]$ExpectedToken
    )

    $Outcome = Invoke-RequiredInstallerFileOperation `
        -CandidatePath $CandidatePath `
        -Operation "copy-exclusive" `
        -Source $Source `
        -Destination $Destination `
        -ExpectedToken $ExpectedToken
    return ConvertFrom-InstallerCreateOutcome `
        -CandidatePath $CandidatePath `
        -Path $Destination `
        -Outcome $Outcome
}

function New-InstallerEmptyFileExclusive {
    param(
        [Parameter(Mandatory = $true)][string]$CandidatePath,
        [Parameter(Mandatory = $true)][string]$Destination
    )

    $Outcome = Invoke-RequiredInstallerFileOperation `
        -CandidatePath $CandidatePath `
        -Operation "create-empty-exclusive" `
        -Destination $Destination
    return ConvertFrom-InstallerCreateOutcome `
        -CandidatePath $CandidatePath `
        -Path $Destination `
        -Outcome $Outcome
}

function Remove-InstallerArtifactIfOwned {
    param(
        [Parameter(Mandatory = $true)][string]$CandidatePath,
        [Parameter(Mandatory = $true)][string]$Path,
        [AllowNull()][string]$ExpectedToken
    )

    if ([string]::IsNullOrEmpty($ExpectedToken)) {
        return $null
    }
    try {
        $ObservedToken = Get-InstallerFileTokenIfPresent `
            -CandidatePath $CandidatePath `
            -Path $Path
        if ($null -eq $ObservedToken) {
            return $null
        }
        if ($ObservedToken -cne $ExpectedToken) {
            return "${Path}: a foreign file identity was preserved"
        }
        Remove-InstallerOwnedFile `
            -CandidatePath $CandidatePath `
            -Path $Path `
            -ExpectedToken $ExpectedToken
        return $null
    } catch {
        return "${Path}: $_"
    }
}

function New-InstallerRegistryRequest {
    param(
        [Parameter(Mandatory = $true)][string]$TemporaryDirectory,
        [Parameter(Mandatory = $true)][hashtable]$Value
    )

    $Path = Join-Path $TemporaryDirectory "path-request-$([Guid]::NewGuid().ToString('N')).json"
    $Json = $Value | ConvertTo-Json -Compress
    $Stream = New-Object System.IO.FileStream(
        $Path,
        [System.IO.FileMode]::CreateNew,
        [System.IO.FileAccess]::Write,
        [System.IO.FileShare]::None
    )
    try {
        $Bytes = (New-Object System.Text.UTF8Encoding($false)).GetBytes($Json)
        $Stream.Write($Bytes, 0, $Bytes.Length)
        $Stream.Flush($true)
    } finally {
        $Stream.Dispose()
    }
    return $Path
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
            $Exited = $LockProcess.WaitForExit($UpdateLockStartupExitTimeoutMilliseconds)
            $ExitDescription = if ($Exited) { "exit code $($LockProcess.ExitCode)" } else { "PID $($LockProcess.Id) did not exit after stdin closed" }
            $ErrorText = if ($Exited) { $LockProcess.StandardError.ReadToEnd().Trim() } else { "" }
            throw "Downloaded binary does not support the required installer transaction lock ($ExitDescription): $ErrorText"
        }
        return $LockProcess
    } catch {
        $StartError = $_
        try { $LockProcess.StandardInput.Close() } catch {}
        try { [void]$LockProcess.WaitForExit($UpdateLockStartupExitTimeoutMilliseconds) } catch {}
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
        $Exited = $LockProcess.WaitForExit($UpdateLockReleaseExitTimeoutMilliseconds)
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

function ConvertTo-InstallerProcessArgument {
    param([Parameter(Mandatory = $true)][string]$Value)

    if ($Value.Contains([string][char]0) -or $Value.Contains('"')) {
        throw "Installer lifecycle path contains a character that cannot be passed to the verified helper."
    }
    return '"' + $Value + '"'
}

function Close-DaemonLifecycleHolder {
    param(
        [Parameter(Mandatory = $true)]$Holder,
        [Parameter(Mandatory = $true)][bool]$ExpectSuccess
    )

    if ($Holder.Phase -ceq "Closed") {
        return
    }
    if ($Holder.Phase -ceq "Retained") {
        throw "daemon lifecycle holder PID $($Holder.Process.Id) remains alive with its service/PID authority retained for inspection"
    }
    $CloseErrors = @()
    try {
        $Holder.Process.StandardInput.Close()
    } catch {
        $CloseErrors += "could not close daemon lifecycle stdin: $_"
    }
    $Exited = $false
    try {
        $Exited = $Holder.Process.WaitForExit($DaemonBoundaryExitTimeoutMilliseconds)
    } catch {
        $CloseErrors += "could not wait for daemon lifecycle holder: $_"
    }
    if (-not $Exited) {
        $Holder.Phase = "Retained"
        throw "daemon lifecycle holder PID $($Holder.Process.Id) did not exit after stdin EOF; it was not terminated, so its service/PID authority remains held for inspection"
    }
    if ($Exited) {
        $RemainingOutput = $Holder.Process.StandardOutput.ReadToEnd()
        if (-not [string]::IsNullOrEmpty($RemainingOutput)) {
            $CloseErrors += "daemon lifecycle holder emitted unexpected protocol output: $($RemainingOutput.Trim())"
        }
        if ($ExpectSuccess -and $Holder.Process.ExitCode -ne 0) {
            $CloseErrors += "daemon lifecycle holder exited with code $($Holder.Process.ExitCode)"
        } elseif (-not $ExpectSuccess -and $Holder.Process.ExitCode -eq 0) {
            $CloseErrors += "abandoned daemon lifecycle holder accepted an incomplete transaction"
        }
    }
    try {
        $Holder.Process.Dispose()
    } catch {
        $CloseErrors += "could not dispose daemon lifecycle process handle: $_"
    }
    $Holder.Phase = "Closed"
    if ($CloseErrors.Count -gt 0) {
        throw ($CloseErrors -join "; ")
    }
}

function Start-DaemonLifecycleHolder {
    param(
        [Parameter(Mandatory = $true)][string]$CandidatePath,
        [Parameter(Mandatory = $true)][string]$InitialExecutable,
        [Parameter(Mandatory = $true)][string]$ReplacementExecutable
    )

    $StartInfo = New-Object System.Diagnostics.ProcessStartInfo
    $StartInfo.FileName = $CandidatePath
    $StartInfo.Arguments = "__hold-daemon-update-boundary --initial-executable $(ConvertTo-InstallerProcessArgument -Value $InitialExecutable) --replacement-executable $(ConvertTo-InstallerProcessArgument -Value $ReplacementExecutable)"
    $StartInfo.UseShellExecute = $false
    $StartInfo.CreateNoWindow = $true
    $StartInfo.RedirectStandardInput = $true
    $StartInfo.RedirectStandardOutput = $true
    # Stderr is inherited instead of redirected: the protocol uses stdout
    # only, and an unread redirected error pipe could deadlock a verbose fatal
    # path before the holder releases its authorities.
    $StartInfo.RedirectStandardError = $false

    $Process = New-Object System.Diagnostics.Process
    $Process.StartInfo = $StartInfo
    $Holder = [pscustomobject]@{
        Process = $Process
        Phase = "Starting"
        Running = $false
        ServiceInstalled = $false
    }
    try {
        [void]$Process.Start()
        $ReadyLine = $Process.StandardOutput.ReadLine()
        switch -CaseSensitive ($ReadyLine) {
            "$DaemonBoundaryPrefix ready running=true service_installed=true" {
                $Holder.Running = $true
                $Holder.ServiceInstalled = $true
            }
            "$DaemonBoundaryPrefix ready running=true service_installed=false" {
                $Holder.Running = $true
            }
            "$DaemonBoundaryPrefix ready running=false service_installed=true" {
                $Holder.ServiceInstalled = $true
            }
            "$DaemonBoundaryPrefix ready running=false service_installed=false" {}
            default {
                throw "verified helper returned an invalid daemon lifecycle readiness marker: $ReadyLine"
            }
        }
        $Holder.Phase = "Stopped"
        return $Holder
    } catch {
        $StartError = $_
        $Holder.Phase = "Unknown"
        try {
            Close-DaemonLifecycleHolder -Holder $Holder -ExpectSuccess $false
        } catch {
            throw "Could not establish the daemon lifecycle boundary: $StartError. Holder cleanup also failed: $_"
        }
        throw "Could not establish the daemon lifecycle boundary: $StartError"
    }
}

function Invoke-DaemonLifecycleCommand {
    param(
        [Parameter(Mandatory = $true)]$Holder,
        [Parameter(Mandatory = $true)][ValidateSet("new", "uninstall", "rollback", "finish", "release")][string]$Command
    )

    $AllowedPhase = switch -CaseSensitive ($Command) {
        "finish" { @("NewReady", "UninstallReady") }
        "release" { @("FinalConfirmed") }
        default { @("Stopped") }
    }
    if ($AllowedPhase -cnotcontains $Holder.Phase) {
        throw "daemon lifecycle command '$Command' is invalid in phase '$($Holder.Phase)'"
    }
    try {
        $Holder.Process.StandardInput.WriteLine($Command)
        $Holder.Process.StandardInput.Flush()
        $Marker = $Holder.Process.StandardOutput.ReadLine()
    } catch {
        $Holder.Phase = "Unknown"
        throw "daemon lifecycle holder communication failed while sending '$Command': $_"
    }
    if ($null -eq $Marker) {
        $Holder.Phase = "Unknown"
        throw "daemon lifecycle holder exited before acknowledging '$Command'"
    }

    switch -CaseSensitive ("$Command`n$Marker") {
        "new`n$DaemonBoundaryPrefix new state ready" {
            $Holder.Phase = "NewReady"
            return
        }
        "new`n$DaemonBoundaryPrefix new state failed" {
            throw "replacement daemon state was rejected; exact daemon absence was retained for rollback"
        }
        "uninstall`n$DaemonBoundaryPrefix uninstall state ready" {
            $Holder.Phase = "UninstallReady"
            return
        }
        "uninstall`n$DaemonBoundaryPrefix uninstall state failed" {
            throw "daemon uninstall state was rejected; exact stopped state was retained for rollback"
        }
        "rollback`n$DaemonBoundaryPrefix old state restored" {
            $Holder.Phase = "Restored"
            Close-DaemonLifecycleHolder -Holder $Holder -ExpectSuccess $true
            return
        }
        "rollback`n$DaemonBoundaryPrefix old state failed" {
            $Holder.Phase = "Unknown"
            throw "the prior daemon state could not be restored; exact daemon absence was re-established"
        }
        "finish`n$DaemonBoundaryPrefix final state confirmed" {
            $Holder.Phase = "FinalConfirmed"
            return
        }
        "release`n$DaemonBoundaryPrefix lifecycle authority released" {
            $Holder.Phase = "Released"
            Close-DaemonLifecycleHolder -Holder $Holder -ExpectSuccess $true
            return
        }
        default {
            $Holder.Phase = "Unknown"
            throw "daemon lifecycle holder returned an invalid marker for '$Command': $Marker"
        }
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

function Set-ExactUserPathTransition {
    param(
        [Parameter(Mandatory = $true)][string]$CandidatePath,
        [Parameter(Mandatory = $true)][string]$TemporaryDirectory,
        [Parameter(Mandatory = $true)][ValidateSet("add", "remove")][string]$Action,
        [Parameter(Mandatory = $true)][string]$ExpectedSnapshot,
        [Parameter(Mandatory = $true)][string]$Entry
    )

    $RequestPath = New-InstallerRegistryRequest `
        -TemporaryDirectory $TemporaryDirectory `
        -Value @{ expected = $ExpectedSnapshot; entry = $Entry }
    $Outcome = Invoke-RequiredInstallerFileOperation `
        -CandidatePath $CandidatePath `
        -Operation "user-path-$Action" `
        -Source $RequestPath
    return ConvertFrom-InstallerPathTransitionOutcome -Outcome $Outcome
}

function Restore-ExactUserPathTransition {
    param(
        [Parameter(Mandatory = $true)][string]$CandidatePath,
        [Parameter(Mandatory = $true)][string]$TemporaryDirectory,
        [Parameter(Mandatory = $true)][string]$OriginalSnapshot,
        [Parameter(Mandatory = $true)][string]$RequestedSnapshot
    )

    $CurrentSnapshot = Invoke-RequiredInstallerFileOperation `
        -CandidatePath $CandidatePath `
        -Operation "user-path-snapshot"
    if ($CurrentSnapshot -ceq $OriginalSnapshot) {
        return [pscustomobject]@{
            Snapshot = $OriginalSnapshot
            Notification = "unchanged"
            NotificationSucceeded = $true
        }
    }
    if ($CurrentSnapshot -cne $RequestedSnapshot) {
        throw "User PATH changed independently; refusing to overwrite its raw value during rollback."
    }
    $RequestPath = New-InstallerRegistryRequest `
        -TemporaryDirectory $TemporaryDirectory `
        -Value @{ expected = $RequestedSnapshot; requested = $OriginalSnapshot }
    $Outcome = Invoke-RequiredInstallerFileOperation `
        -CandidatePath $CandidatePath `
        -Operation "user-path-restore" `
        -Source $RequestPath
    return ConvertFrom-InstallerPathTransitionOutcome -Outcome $Outcome
}

function ConvertFrom-InstallerPathTransitionOutcome {
    param([Parameter(Mandatory = $true)][string]$Outcome)

    if ($Outcome -cnotmatch '^path-transition\|(?<snapshot>absent|v1:[0-9]+:[0-9a-f]*)\|(?<notification>unchanged|broadcast-ok|broadcast-failed:[0-9]+)$') {
        throw "installer helper returned an invalid User PATH transition result: $Outcome"
    }
    $Notification = $Matches.notification
    return [pscustomobject]@{
        Snapshot = $Matches.snapshot
        Notification = $Notification
        NotificationSucceeded = $Notification -ceq "unchanged" -or $Notification -ceq "broadcast-ok"
    }
}

function Get-ProcessPathSnapshot {
    $Present = Test-Path -LiteralPath "Env:Path"
    return [pscustomobject]@{
        Present = $Present
        Value = if ($Present) { [Environment]::GetEnvironmentVariable("Path", "Process") } else { $null }
    }
}

function Test-ProcessPathSnapshotEqual {
    param(
        [Parameter(Mandatory = $true)]$Left,
        [Parameter(Mandatory = $true)]$Right
    )

    if ([bool]$Left.Present -ne [bool]$Right.Present) {
        return $false
    }
    if (-not $Left.Present) {
        return $true
    }
    return [System.StringComparer]::Ordinal.Equals([string]$Left.Value, [string]$Right.Value)
}

function Get-RequestedProcessPathSnapshot {
    param(
        [Parameter(Mandatory = $true)]$Current,
        [Parameter(Mandatory = $true)][ValidateSet("add", "remove")][string]$Action,
        [Parameter(Mandatory = $true)][string]$Entry
    )

    if ([string]::IsNullOrEmpty($Entry) -or $Entry.Contains(";") -or $Entry.Contains([char]0)) {
        throw "Process PATH entry is empty or contains a separator/NUL."
    }
    if (-not $Current.Present) {
        if ($Action -ceq "remove") {
            return [pscustomobject]@{ Present = $false; Value = $null }
        }
        return [pscustomobject]@{ Present = $true; Value = $Entry }
    }

    $CurrentValue = [string]$Current.Value
    $Segments = $CurrentValue.Split([char[]]@(';'), [System.StringSplitOptions]::None)
    $Matching = @($Segments | ForEach-Object {
        [System.StringComparer]::OrdinalIgnoreCase.Equals($_, $Entry)
    })
    if ($Action -ceq "add") {
        if ($Matching -contains $true) {
            return [pscustomobject]@{ Present = $true; Value = $CurrentValue }
        }
        # A fully empty value becomes the exact entry, so this installer does
        # not introduce a CWD-search segment. Existing empty segments in any
        # nonempty raw PATH, including a trailing one, remain byte-for-byte.
        $RequestedValue = if ($CurrentValue.Length -eq 0) { $Entry } else { "$CurrentValue;$Entry" }
        return [pscustomobject]@{ Present = $true; Value = $RequestedValue }
    }

    if ($Matching -notcontains $true) {
        return [pscustomobject]@{ Present = $true; Value = $CurrentValue }
    }
    $Kept = New-Object System.Collections.Generic.List[string]
    for ($Index = 0; $Index -lt $Segments.Count; $Index++) {
        if (-not $Matching[$Index]) {
            $Kept.Add($Segments[$Index])
        }
    }
    if ($Kept.Count -eq 0) {
        return [pscustomobject]@{ Present = $false; Value = $null }
    }
    return [pscustomobject]@{ Present = $true; Value = [string]::Join(";", $Kept) }
}

function Set-ExactProcessPathSnapshot {
    param(
        [Parameter(Mandatory = $true)]$Expected,
        [Parameter(Mandatory = $true)]$Requested
    )

    $Observed = Get-ProcessPathSnapshot
    if (-not (Test-ProcessPathSnapshotEqual -Left $Observed -Right $Expected)) {
        throw "Current PowerShell process PATH changed independently; refusing to overwrite it."
    }
    if (Test-ProcessPathSnapshotEqual -Left $Expected -Right $Requested) {
        return
    }
    $RequestedValue = if ($Requested.Present) { [string]$Requested.Value } else { $null }
    [Environment]::SetEnvironmentVariable("Path", $RequestedValue, "Process")
    $After = Get-ProcessPathSnapshot
    if (-not (Test-ProcessPathSnapshotEqual -Left $After -Right $Requested)) {
        throw "Current PowerShell process PATH did not reach the exact requested state."
    }
}

function Restore-ExactProcessPathTransition {
    param(
        [Parameter(Mandatory = $true)]$Original,
        [Parameter(Mandatory = $true)]$Requested
    )

    $Observed = Get-ProcessPathSnapshot
    if (Test-ProcessPathSnapshotEqual -Left $Observed -Right $Original) {
        return
    }
    if (-not (Test-ProcessPathSnapshotEqual -Left $Observed -Right $Requested)) {
        throw "Current PowerShell process PATH changed independently; its exact value was preserved during rollback."
    }
    Set-ExactProcessPathSnapshot -Expected $Requested -Requested $Original
}

function Invoke-ExactProcessPathTransition {
    param(
        [Parameter(Mandatory = $true)][ValidateSet("add", "remove")][string]$Action,
        [Parameter(Mandatory = $true)][string]$Entry
    )

    $Original = Get-ProcessPathSnapshot
    $Requested = Get-RequestedProcessPathSnapshot `
        -Current $Original `
        -Action $Action `
        -Entry $Entry
    $TransitionError = $null
    try {
        Set-ExactProcessPathSnapshot -Expected $Original -Requested $Requested
    } catch {
        $TransitionError = $_
    }
    $Observed = Get-ProcessPathSnapshot
    $AtOriginal = Test-ProcessPathSnapshotEqual -Left $Observed -Right $Original
    $AtRequested = Test-ProcessPathSnapshotEqual -Left $Observed -Right $Requested
    return [pscustomobject]@{
        Original = $Original
        Requested = $Requested
        Applied = -not (Test-ProcessPathSnapshotEqual -Left $Original -Right $Requested) -and $AtRequested
        Ambiguous = -not $AtOriginal -and -not $AtRequested
        Error = $TransitionError
    }
}

function Invoke-ClassifiedInstallerReplace {
    param(
        [Parameter(Mandatory = $true)][string]$CandidatePath,
        [Parameter(Mandatory = $true)][string]$ReplacementPath,
        [Parameter(Mandatory = $true)][string]$DestinationPath,
        [Parameter(Mandatory = $true)][string]$DisplacedPath,
        [Parameter(Mandatory = $true)][string]$ReplacementToken,
        [Parameter(Mandatory = $true)][string]$DestinationToken
    )

    $Boundary = Invoke-InstallerFileOperation `
        -CandidatePath $CandidatePath `
        -Operation "replace-with-displaced" `
        -Source $ReplacementPath `
        -Destination $DestinationPath `
        -Displaced $DisplacedPath `
        -ExpectedToken $ReplacementToken `
        -ExpectedDestinationToken $DestinationToken
    $InspectionError = $null
    try {
        $ReplacementAfter = Get-InstallerFileTokenIfPresent -CandidatePath $CandidatePath -Path $ReplacementPath
        $DestinationAfter = Get-InstallerFileTokenIfPresent -CandidatePath $CandidatePath -Path $DestinationPath
        $DisplacedAfter = Get-InstallerFileTokenIfPresent -CandidatePath $CandidatePath -Path $DisplacedPath
    } catch {
        $InspectionError = "$_"
    }
    $State = if ($null -eq $InspectionError -and
        $null -eq $ReplacementAfter -and
        $DestinationAfter -ceq $ReplacementToken -and
        $DisplacedAfter -ceq $DestinationToken) {
        "Published"
    } elseif ($null -eq $InspectionError -and
        $ReplacementAfter -ceq $ReplacementToken -and
        $DestinationAfter -ceq $DestinationToken -and
        $null -eq $DisplacedAfter) {
        "Unchanged"
    } else {
        "Ambiguous"
    }
    if ($Boundary.Succeeded -and $Boundary.Result -cne "replaced") {
        $Boundary = [pscustomobject]@{
            Succeeded = $false
            Result = $null
            Error = "installer helper returned an unknown replacement result: $($Boundary.Result)"
        }
    }
    return [pscustomobject]@{
        State = $State
        OperationError = if ($Boundary.Succeeded) { $null } else { $Boundary.Error }
        InspectionError = $InspectionError
    }
}

function Invoke-ClassifiedInstallerMoveNoReplace {
    param(
        [Parameter(Mandatory = $true)][string]$CandidatePath,
        [Parameter(Mandatory = $true)][string]$SourcePath,
        [Parameter(Mandatory = $true)][string]$DestinationPath,
        [Parameter(Mandatory = $true)][string]$SourceToken
    )

    $Boundary = Invoke-InstallerFileOperation `
        -CandidatePath $CandidatePath `
        -Operation "move-noreplace" `
        -Source $SourcePath `
        -Destination $DestinationPath `
        -ExpectedToken $SourceToken
    $InspectionError = $null
    try {
        $SourceAfter = Get-InstallerFileTokenIfPresent -CandidatePath $CandidatePath -Path $SourcePath
        $DestinationAfter = Get-InstallerFileTokenIfPresent -CandidatePath $CandidatePath -Path $DestinationPath
    } catch {
        $InspectionError = "$_"
    }
    $State = if ($null -eq $InspectionError -and
        $null -eq $SourceAfter -and
        $DestinationAfter -ceq $SourceToken) {
        "Published"
    } elseif ($null -eq $InspectionError -and
        $SourceAfter -ceq $SourceToken -and
        $null -eq $DestinationAfter) {
        "Unchanged"
    } else {
        "Ambiguous"
    }
    if ($Boundary.Succeeded -and $Boundary.Result -cne $SourceToken) {
        $Boundary = [pscustomobject]@{
            Succeeded = $false
            Result = $null
            Error = "installer helper returned an unexpected no-replace token"
        }
    }
    return [pscustomobject]@{
        State = $State
        OperationError = if ($Boundary.Succeeded) { $null } else { $Boundary.Error }
        InspectionError = $InspectionError
    }
}

# ── Installer entrypoint: resolve and verify the release binary ──

# Detect architecture
try {
$Arch = switch ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture) {
    ([System.Runtime.InteropServices.Architecture]::X64) { "amd64" }
    ([System.Runtime.InteropServices.Architecture]::Arm64) { "arm64" }
    default {
        throw "Unsupported Windows architecture: $([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture). Only X64 and Arm64 release assets are supported."
    }
}
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
$TmpDir = Join-Path ([System.IO.Path]::GetTempPath()) "codex-switch-global-pace-install-$([Guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path $TmpDir | Out-Null
$ZipPath = Join-Path $TmpDir $AssetName
$ChecksumUrl = "$DownloadUrl.sha256"
$ChecksumPath = "$ZipPath.sha256"

try {
    Invoke-WebRequest -Uri $DownloadUrl -OutFile $ZipPath -UseBasicParsing
    Invoke-WebRequest -Uri $ChecksumUrl -OutFile $ChecksumPath -UseBasicParsing
} catch {
    $DownloadError = $_
    throw "Archive or checksum download failed: $DownloadError"
}

# Verify checksum before extracting any downloaded content
$ChecksumText = (Get-Content -LiteralPath $ChecksumPath -Raw).Trim()
$ChecksumPattern = '^(?<hash>[0-9A-Fa-f]{64})\s+\*?(?<file>\S+)$'
if ($ChecksumText -notmatch $ChecksumPattern -or (Split-Path -Leaf $Matches.file) -ne $AssetName) {
    throw "Invalid or empty checksum file for $AssetName."
}

$ExpectedSha256 = $Matches.hash.ToUpperInvariant()
$ActualSha256 = (Get-DirectFileSha256 -Path $ZipPath).ToUpperInvariant()
if ($ActualSha256 -ne $ExpectedSha256) {
    throw "Checksum mismatch for $AssetName; refusing to extract it."
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
    $CandidateError = $_
    throw "Downloaded binary failed its pre-install check; the existing installation was not changed: $CandidateError"
}

# ── Uninstall ────────────────────────────────────────────
$BinaryStem = [System.IO.Path]::GetFileNameWithoutExtension($BinaryName)
$StagedBin = Join-Path $InstallDir ".$BinaryStem.install.exe"
$BackupBin = Join-Path $InstallDir ".$BinaryStem.rollback.exe"
$DisplacedBin = New-InstallerRecoveryPath -Directory $InstallDir -Stem $BinaryStem -Role "displaced"
$FailedBin = New-InstallerRecoveryPath -Directory $InstallDir -Stem $BinaryStem -Role "failed"

if ($Uninstall) {
    Write-Host "[info]  Uninstalling codex-switch-global-pace..." -ForegroundColor Blue

    $InstalledBin = Join-Path $InstallDir $BinaryName
    $UninstallBackupBin = Join-Path $InstallDir ".$BinaryStem.uninstall.exe"
    try {
        $InstallDirWasPresent = Test-DirectInstallDirectory -Path $InstallDir
    } catch {
        $PreflightError = $_
        throw "Could not inspect the existing uninstall state; nothing was changed: $PreflightError"
    }

    if (-not $InstallDirWasPresent) {
        try {
            [void][System.IO.Directory]::CreateDirectory($InstallDir)
            if (-not (Test-DirectInstallDirectory -Path $InstallDir)) {
                throw "Install path was not created as a direct directory: $InstallDir"
            }
        } catch {
            $DirectoryError = $_
            throw "Could not create the direct install directory; nothing was changed: $DirectoryError"
        }
    }

    $UninstallLockHolder = $null
    $UninstallLifecycleHolder = $null
    $UninstallError = $null
    $PostCommitCleanupError = $null
    $LockReleaseError = $null
    $InstalledBinaryWasPresent = $false
    $OriginalBinaryToken = $null
    $UninstallHoldToken = $null
    $UninstallPlaceholderToken = $null
    $OriginalUserPathSnapshot = $null
    $RequestedUserPathSnapshot = $null
    $OriginalProcessPathSnapshot = $null
    $RequestedProcessPathSnapshot = $null
    $DaemonWasRunning = $false
    $DaemonServiceInstalled = $false
    $PathMutationAttempted = $false
    $ProcessPathMutationAttempted = $false
    $ProcessPathStateAmbiguous = $false
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
        $OriginalBinaryToken = if ($InstalledBinaryWasPresent) {
            Get-InstallerFileToken -CandidatePath $CandidateBin -Path $InstalledBin
        } else {
            $null
        }
        if ($InstalledBinaryWasPresent) {
            $UninstallHoldToken = Copy-InstallerFileExclusive `
                -CandidatePath $CandidateBin `
                -Source $InstalledBin `
                -Destination $UninstallBackupBin `
                -ExpectedToken $OriginalBinaryToken
            try {
                $UninstallPlaceholderToken = New-InstallerEmptyFileExclusive `
                    -CandidatePath $CandidateBin `
                    -Destination $StagedBin
            } catch {
                $PlaceholderError = $_
                $HoldCleanupError = Remove-InstallerArtifactIfOwned `
                    -CandidatePath $CandidateBin `
                    -Path $UninstallBackupBin `
                    -ExpectedToken $UninstallHoldToken
                if ($null -ne $HoldCleanupError) {
                    throw "Could not create the uninstall placeholder ($PlaceholderError); independent recovery cleanup also failed: $HoldCleanupError"
                }
                throw $PlaceholderError
            }
            $UninstallMutationAttempted = $true
        }

        # Ownership is a read-only precondition. The actual uninstall repeats
        # it at the deletion boundary, after the daemon has been stopped.
        Assert-CandidateServiceOwner `
            -CandidatePath $CandidateBin `
            -ExpectedExecutable $InstalledBin

        $OriginalUserPathSnapshot = Invoke-RequiredInstallerFileOperation `
            -CandidatePath $CandidateBin `
            -Operation "user-path-snapshot"

        $UninstallLifecycleHolder = Start-DaemonLifecycleHolder `
            -CandidatePath $CandidateBin `
            -InitialExecutable $InstalledBin `
            -ReplacementExecutable $InstalledBin
        $DaemonWasRunning = $UninstallLifecycleHolder.Running
        $DaemonServiceInstalled = $UninstallLifecycleHolder.ServiceInstalled
        # The holder owns a PID-absence authority even when the daemon was
        # initially stopped, so every later failure must explicitly roll it
        # back or commit it before the update lock can be released.
        $UninstallMutationAttempted = $true

        # The candidate performs raw type/byte/absence CAS inside a Windows
        # registry transaction. Unrelated PATH segments are never reconstructed.
        $UserPathTransition = Set-ExactUserPathTransition `
            -CandidatePath $CandidateBin `
            -TemporaryDirectory $TmpDir `
            -Action "remove" `
            -ExpectedSnapshot $OriginalUserPathSnapshot `
            -Entry $InstallDir
        $RequestedUserPathSnapshot = $UserPathTransition.Snapshot
        $PathMutationAttempted = $RequestedUserPathSnapshot -cne $OriginalUserPathSnapshot
        if (-not $UserPathTransition.NotificationSucceeded) {
            Write-Host "[warn]  User PATH committed, but Windows environment notification failed ($($UserPathTransition.Notification)). New terminals may require sign-out/sign-in." -ForegroundColor Yellow
        }

        $ProcessPathTransition = Invoke-ExactProcessPathTransition -Action "remove" -Entry $InstallDir
        $OriginalProcessPathSnapshot = $ProcessPathTransition.Original
        $RequestedProcessPathSnapshot = $ProcessPathTransition.Requested
        $ProcessPathMutationAttempted = $ProcessPathTransition.Applied
        $ProcessPathStateAmbiguous = $ProcessPathTransition.Ambiguous
        if ($null -ne $ProcessPathTransition.Error -or $ProcessPathStateAmbiguous) {
            Write-Host "[warn]  User PATH was committed, but the current PowerShell process PATH update was not confirmed exactly: $($ProcessPathTransition.Error)" -ForegroundColor Yellow
        }
        if ($PathMutationAttempted -or $ProcessPathMutationAttempted) {
            $UninstallMutationAttempted = $true
            Write-Host "[info]  Removed $InstallDir from user PATH" -ForegroundColor Blue
        }

        if ($InstalledBinaryWasPresent) {
            $UninstallMutationAttempted = $true
            $Staging = Invoke-ClassifiedInstallerReplace `
                -CandidatePath $CandidateBin `
                -ReplacementPath $StagedBin `
                -DestinationPath $InstalledBin `
                -DisplacedPath $DisplacedBin `
                -ReplacementToken $UninstallPlaceholderToken `
                -DestinationToken $OriginalBinaryToken
            if ($Staging.State -ceq "Published") {
                if ($null -ne $Staging.OperationError) {
                    throw "Uninstall placeholder reached the public path, but the replacement boundary reported an error: $($Staging.OperationError)"
                }
            } elseif ($Staging.State -ceq "Unchanged") {
                throw "Installed binary could not be staged without replacement: $($Staging.OperationError)"
            } else {
                throw "Installed binary staging was ambiguous; public, placeholder, displaced, and independent hold paths were preserved: operation=$($Staging.OperationError); inspection=$($Staging.InspectionError)"
            }
        } elseif (Test-DirectInstalledBinary -Path $InstalledBin) {
            throw "An installed binary appeared after the locked uninstall preflight; refusing to commit against changed state."
        }

        # Service removal is the last meaningful commit boundary. The verified
        # child performs it while retaining the same service/PID authorities
        # used for the stop, then keeps them until final file cleanup is done.
        Invoke-DaemonLifecycleCommand -Holder $UninstallLifecycleHolder -Command "uninstall"
        $UninstallCommitted = $true
        Invoke-DaemonLifecycleCommand -Holder $UninstallLifecycleHolder -Command "finish"
        if ($InstalledBinaryWasPresent -or $DaemonServiceInstalled) {
            Write-Host "[info]  Daemon scheduled task cleanup completed." -ForegroundColor Blue
        }

        # The official executable path, PATH, and service are now absent. The
        # recovery backup is post-commit cleanup: failure must not recreate a
        # potentially different Task XML definition.
        if ($InstalledBinaryWasPresent) {
            try {
                if ((Get-InstallerFileToken -CandidatePath $CandidateBin -Path $InstalledBin) -cne $UninstallPlaceholderToken -or
                    (Get-InstallerFileToken -CandidatePath $CandidateBin -Path $DisplacedBin) -cne $OriginalBinaryToken -or
                    (Get-InstallerFileToken -CandidatePath $CandidateBin -Path $UninstallBackupBin) -cne $UninstallHoldToken) {
                    throw "uninstall file identities changed before post-commit cleanup"
                }
                Remove-InstallerOwnedFile `
                    -CandidatePath $CandidateBin `
                    -Path $InstalledBin `
                    -ExpectedToken $UninstallPlaceholderToken
                Remove-InstallerOwnedFile `
                    -CandidatePath $CandidateBin `
                    -Path $UninstallBackupBin `
                    -ExpectedToken $UninstallHoldToken
                Remove-InstallerOwnedFile `
                    -CandidatePath $CandidateBin `
                    -Path $DisplacedBin `
                    -ExpectedToken $OriginalBinaryToken
                Write-Host "[info]  Removed $InstalledBin" -ForegroundColor Blue
            } catch {
                $PostCommitCleanupError = "token-bound uninstall file cleanup raised an exception: $_"
            }
        }
        try {
            Invoke-DaemonLifecycleCommand -Holder $UninstallLifecycleHolder -Command "release"
        } catch {
            $LifecycleFinalizationError = "daemon lifecycle authority release failed after uninstall cleanup: $_"
            if ($null -eq $PostCommitCleanupError) {
                $PostCommitCleanupError = $LifecycleFinalizationError
            } else {
                $PostCommitCleanupError = "$PostCommitCleanupError; $LifecycleFinalizationError"
            }
        }
    } catch {
        $UninstallFailure = $_
        $UninstallMutationAttempted = $UninstallMutationAttempted -or
            $PathMutationAttempted -or $ProcessPathMutationAttempted
        if ($UninstallCommitted) {
            $PostCommitCleanupError = "post-commit cleanup or reporting failed: $UninstallFailure"
        } elseif (-not $UninstallMutationAttempted) {
            $UninstallError = "Uninstall preflight failed before any binary, PATH, or daemon mutation: $UninstallFailure"
        } else {
            $RollbackErrors = @()

            if ($null -ne $UninstallLifecycleHolder -and
                $UninstallLifecycleHolder.Phase -cne "Stopped") {
                $UninstallError = "The uninstall state is ambiguous after a lifecycle protocol failure; binary, PATH, service, and recovery paths were preserved without speculative rollback: $UninstallFailure"
            } else {

            # Restore exact binary bytes before restarting a daemon generation.
            # The Rust service transaction restores its original Task XML on
            # failure; the script never recreates that definition.
            $StableRollbackBinary = $false
            if ($InstalledBinaryWasPresent) {
                try {
                    $InstalledTokenAfterFailure = Get-InstallerFileTokenIfPresent -CandidatePath $CandidateBin -Path $InstalledBin
                    $DisplacedTokenAfterFailure = Get-InstallerFileTokenIfPresent -CandidatePath $CandidateBin -Path $DisplacedBin
                    $PlaceholderTokenAfterFailure = Get-InstallerFileTokenIfPresent -CandidatePath $CandidateBin -Path $StagedBin
                    $HoldTokenAfterFailure = Get-InstallerFileTokenIfPresent -CandidatePath $CandidateBin -Path $UninstallBackupBin
                    if ($InstalledTokenAfterFailure -ceq $OriginalBinaryToken -and
                        $null -eq $DisplacedTokenAfterFailure) {
                        $StableRollbackBinary = $true
                    } elseif ($InstalledTokenAfterFailure -ceq $UninstallPlaceholderToken -and
                        $DisplacedTokenAfterFailure -ceq $OriginalBinaryToken -and
                        $null -eq $PlaceholderTokenAfterFailure) {
                        $Restore = Invoke-ClassifiedInstallerReplace `
                            -CandidatePath $CandidateBin `
                            -ReplacementPath $DisplacedBin `
                            -DestinationPath $InstalledBin `
                            -DisplacedPath $FailedBin `
                            -ReplacementToken $OriginalBinaryToken `
                            -DestinationToken $UninstallPlaceholderToken
                        if ($Restore.State -ceq "Published") {
                            $StableRollbackBinary = $true
                            if ($null -ne $Restore.OperationError) {
                                $RollbackErrors += "binary was restored, but the replacement boundary reported: $($Restore.OperationError)"
                            }
                        } else {
                            $RollbackErrors += "binary restoration was $($Restore.State): operation=$($Restore.OperationError); inspection=$($Restore.InspectionError)"
                        }
                    } else {
                        $RollbackErrors += "binary state was ambiguous; installed=$InstalledTokenAfterFailure displaced=$DisplacedTokenAfterFailure placeholder=$PlaceholderTokenAfterFailure hold=$HoldTokenAfterFailure"
                    }
                    if ($StableRollbackBinary) {
                        foreach ($PlaceholderPath in @($StagedBin, $FailedBin)) {
                            $PlaceholderCleanup = Remove-InstallerArtifactIfOwned `
                                -CandidatePath $CandidateBin `
                                -Path $PlaceholderPath `
                                -ExpectedToken $UninstallPlaceholderToken
                            if ($null -ne $PlaceholderCleanup) {
                                $RollbackErrors += "placeholder cleanup failed: $PlaceholderCleanup"
                            }
                        }
                        $HoldCleanup = Remove-InstallerArtifactIfOwned `
                            -CandidatePath $CandidateBin `
                            -Path $UninstallBackupBin `
                            -ExpectedToken $UninstallHoldToken
                        if ($null -ne $HoldCleanup) {
                            $RollbackErrors += "independent hold cleanup failed: $HoldCleanup"
                        }
                    }
                } catch {
                    $RollbackErrors += "binary rollback inspection failed: $_"
                }
            } else {
                try {
                    $UnexpectedPaths = @($InstalledBin, $UninstallBackupBin, $StagedBin, $DisplacedBin) |
                        Where-Object { $null -ne (Get-InstallerFileTokenIfPresent -CandidatePath $CandidateBin -Path $_) }
                    if ($UnexpectedPaths.Count -eq 0) {
                        $StableRollbackBinary = $true
                    } else {
                        $RollbackErrors += "binary paths changed after an absent-binary preflight and were preserved: $($UnexpectedPaths -join ', ')"
                    }
                } catch {
                    $RollbackErrors += "absent-binary rollback inspection failed: $_"
                }
            }

            if ($PathMutationAttempted) {
                try {
                    $RestoredUserPath = Restore-ExactUserPathTransition `
                        -CandidatePath $CandidateBin `
                        -TemporaryDirectory $TmpDir `
                        -OriginalSnapshot $OriginalUserPathSnapshot `
                        -RequestedSnapshot $RequestedUserPathSnapshot
                    if (-not $RestoredUserPath.NotificationSucceeded) {
                        $RollbackErrors += "user PATH was restored, but Windows environment notification failed ($($RestoredUserPath.Notification))"
                    }
                    $PathMutationAttempted = $false
                } catch {
                    $RollbackErrors += "user PATH restoration failed: $_"
                }
            }

            if ($ProcessPathMutationAttempted) {
                try {
                    Restore-ExactProcessPathTransition `
                        -Original $OriginalProcessPathSnapshot `
                        -Requested $RequestedProcessPathSnapshot
                    $ProcessPathMutationAttempted = $false
                } catch {
                    $RollbackErrors += "current PowerShell process PATH restoration failed: $_"
                }
            } elseif ($ProcessPathStateAmbiguous) {
                $RollbackErrors += "current PowerShell process PATH changed independently and was preserved"
            }

            if ($StableRollbackBinary -and $null -ne $UninstallLifecycleHolder) {
                try {
                    Invoke-DaemonLifecycleCommand `
                        -Holder $UninstallLifecycleHolder `
                        -Command "rollback"
                } catch {
                    $RollbackErrors += "daemon running/service-state restoration failed: $_"
                }
            } elseif ($DaemonWasRunning -or $DaemonServiceInstalled) {
                $RollbackErrors += "daemon running/service state could not be restored without the exact installed binary state"
            } elseif ($null -ne $UninstallLifecycleHolder) {
                $RollbackErrors += "daemon absence authority could not be released as a successful rollback without the exact binary state"
            }

            if ($RollbackErrors.Count -eq 0) {
                $UninstallError = "The uninstall did not commit, and the exact pre-uninstall binary, PATH, and running state were restored: $UninstallFailure"
            } else {
                $UninstallError = "The uninstall did not commit and rollback was incomplete: $UninstallFailure. $($RollbackErrors -join '; ')"
            }
            }
        }
    } finally {
        $LifecycleReleaseError = $null
        if ($null -ne $UninstallLifecycleHolder -and
            $UninstallLifecycleHolder.Phase -cne "Closed") {
            try {
                Close-DaemonLifecycleHolder `
                    -Holder $UninstallLifecycleHolder `
                    -ExpectSuccess $false
            } catch {
                $LifecycleReleaseError = $_
            }
        }
        if ($null -ne $UninstallLockHolder) {
            try {
                Complete-UpdateLockHolder -LockProcess $UninstallLockHolder
            } catch {
                $LockReleaseError = $_
            }
        }
    }

    $NewInstallDirectoryCleanupError = $null
    if (-not $InstallDirWasPresent) {
        try {
            Remove-NewEmptyInstallDirectory -Path $InstallDir
        } catch {
            $NewInstallDirectoryCleanupError = $_
        }
    }

    if ($null -ne $UninstallError) {
        $Suffix = ""
        if ($null -ne $LifecycleReleaseError) {
            $Suffix += " Additionally, the daemon lifecycle holder did not close cleanly: $LifecycleReleaseError"
        }
        if ($null -ne $LockReleaseError) {
            $Suffix += " Additionally, the exclusive update lock did not close cleanly: $LockReleaseError"
        }
        if ($null -ne $NewInstallDirectoryCleanupError) {
            $Suffix += " Additionally, exact cleanup of the newly created install directory failed: $NewInstallDirectoryCleanupError"
        }
        throw "Uninstall did not complete: $UninstallError$Suffix"
    }
    if ($null -ne $PostCommitCleanupError) {
        $Suffix = ""
        if ($null -ne $LifecycleReleaseError) {
            $Suffix += " Additionally, the daemon lifecycle holder did not close cleanly: $LifecycleReleaseError"
        }
        if ($null -ne $LockReleaseError) {
            $Suffix += " Additionally, the exclusive update lock did not close cleanly: $LockReleaseError"
        }
        if ($null -ne $NewInstallDirectoryCleanupError) {
            $Suffix += " Additionally, exact cleanup of the newly created install directory failed: $NewInstallDirectoryCleanupError"
        }
        throw "Uninstall committed, but post-commit cleanup could not be confirmed. Official executable path: $InstalledBin. Recovery residue path: $UninstallBackupBin. $PostCommitCleanupError$Suffix"
    }
    if ($null -ne $LifecycleReleaseError) {
        throw "Uninstall committed, but the daemon lifecycle holder did not close cleanly: $LifecycleReleaseError"
    }
    if ($null -ne $LockReleaseError) {
        throw "Uninstall completed, but the exclusive update lock did not close cleanly: $LockReleaseError"
    }
    if ($null -ne $NewInstallDirectoryCleanupError) {
        throw "Uninstall completed, but exact cleanup of the newly created install directory failed: $NewInstallDirectoryCleanupError"
    }

    # This directory is deliberately shared with codex-switch so existing
    # profiles work without another login. Never remove it from this uninstaller.
    if (Test-Path -LiteralPath $DataDir) {
        Write-Host "[info]  Kept shared profile data: $DataDir" -ForegroundColor Blue
    }
    Write-Host "[info]  codex-switch-global-pace has been uninstalled." -ForegroundColor Blue
    return
}

# Stage the verified candidate beside the installed executable before stopping a
# working daemon. Publication is a same-directory atomic move or replacement,
# so a download drive and the install drive cannot turn it into a partial
# cross-volume operation.
$InstalledBin = Join-Path $InstallDir $BinaryName
try {
    $InstallDirWasPresent = Test-DirectInstallDirectory -Path $InstallDir
} catch {
    $DirectoryError = $_
    throw $DirectoryError
}

$UpdateLockHolder = $null
$InstallLifecycleHolder = $null

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
    $DirectoryCleanupError = $null
    if (-not $InstallDirWasPresent) {
        try {
            Remove-NewEmptyInstallDirectory -Path $InstallDir
        } catch {
            $DirectoryCleanupError = $_
        }
    }
    if ($null -ne $DirectoryCleanupError) {
        throw "${LockError}. Additionally, exact cleanup of the newly created install directory failed: $DirectoryCleanupError"
    }
    throw $LockError
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
    $OriginalUserPathSnapshot = Invoke-RequiredInstallerFileOperation `
        -CandidatePath $CandidateBin `
        -Operation "user-path-snapshot"

    $StagedBinaryToken = $null
    $PreviousBinaryToken = $null
    $BackupBinaryToken = $null
    try {
    $CandidateBinaryToken = Get-InstallerFileToken -CandidatePath $CandidateBin -Path $CandidateBin
    $StagedBinaryToken = Copy-InstallerFileExclusive `
        -CandidatePath $CandidateBin `
        -Source $CandidateBin `
        -Destination $StagedBin `
        -ExpectedToken $CandidateBinaryToken
    $StagedVersionOutput = & $StagedBin --version 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "staged candidate version check exited with code ${LASTEXITCODE}: $StagedVersionOutput"
    }
    $StagedVersionLine = (($StagedVersionOutput | Select-Object -First 1) -as [string]).Trim()
    if ($StagedVersionLine -cne "codex-switch-global-pace $ExpectedReleaseVersion") {
        throw "staged candidate reported '$StagedVersionLine', expected 'codex-switch-global-pace $ExpectedReleaseVersion'"
    }
    $PreviousBinaryToken = if ($ExistingBinaryWasPresent) {
        Get-InstallerFileToken -CandidatePath $CandidateBin -Path $InstalledBin
    } else {
        $null
    }
    if ($ExistingBinaryWasPresent) {
        $BackupBinaryToken = Copy-InstallerFileExclusive `
            -CandidatePath $CandidateBin `
            -Source $InstalledBin `
            -Destination $BackupBin `
            -ExpectedToken $PreviousBinaryToken
    }
    } catch {
        $StageError = $_
        $StageCleanupErrors = @()
        if ($null -ne $StagedBinaryToken) {
            try {
                Remove-InstallerOwnedFile -CandidatePath $CandidateBin -Path $StagedBin -ExpectedToken $StagedBinaryToken
            } catch {
                $StageCleanupErrors += "staged candidate ${StagedBin}: $_"
            }
        }
        if ($null -ne $BackupBinaryToken) {
            try {
                Remove-InstallerOwnedFile -CandidatePath $CandidateBin -Path $BackupBin -ExpectedToken $BackupBinaryToken
            } catch {
                $StageCleanupErrors += "independent backup ${BackupBin}: $_"
            }
        }
        if (-not $InstallDirWasPresent) {
            try {
                Remove-NewEmptyInstallDirectory -Path $InstallDir
            } catch {
                $StageCleanupErrors += "new install directory ${InstallDir}: $_"
            }
        }
        $CleanupSuffix = if ($StageCleanupErrors.Count -gt 0) {
            " Exact cleanup was incomplete: $($StageCleanupErrors -join '; ')"
        } else {
            ""
        }
        throw "Could not stage the verified binary; the existing installation was not changed: $StageError.$CleanupSuffix"
    }

# A release-verified child captures and stops the daemon while retaining both
# service-operation and PID-absence authority through commit or rollback.
$DaemonWasRunning = $false
$DaemonServiceInstalled = $false
try {
    $InstallLifecycleHolder = Start-DaemonLifecycleHolder `
        -CandidatePath $CandidateBin `
        -InitialExecutable $InstalledBin `
        -ReplacementExecutable $InstalledBin
    $DaemonWasRunning = $InstallLifecycleHolder.Running
    $DaemonServiceInstalled = $InstallLifecycleHolder.ServiceInstalled
    if (-not $ExistingBinaryWasPresent -and
        ($DaemonWasRunning -or $DaemonServiceInstalled)) {
        throw "The installed binary is absent while daemon/service state still exists. Restore or explicitly uninstall that exact installation before retrying."
    }
} catch {
    $StatusError = $_
    $LifecycleRollbackError = $null
    if ($null -ne $InstallLifecycleHolder -and
        $InstallLifecycleHolder.Phase -ceq "Stopped") {
        try {
            Invoke-DaemonLifecycleCommand -Holder $InstallLifecycleHolder -Command "rollback"
        } catch {
            $LifecycleRollbackError = $_
        }
    }
    $CleanupErrors = @(
        Remove-InstallerArtifactIfOwned -CandidatePath $CandidateBin -Path $StagedBin -ExpectedToken $StagedBinaryToken
        Remove-InstallerArtifactIfOwned -CandidatePath $CandidateBin -Path $BackupBin -ExpectedToken $BackupBinaryToken
    ) | Where-Object { $null -ne $_ }
    if (-not $InstallDirWasPresent) {
        try {
            Remove-NewEmptyInstallDirectory -Path $InstallDir
        } catch {
            $CleanupErrors += "new install directory ${InstallDir}: $_"
        }
    }
    if ($null -ne $LifecycleRollbackError) {
        $CleanupErrors += "daemon lifecycle restoration failed: $LifecycleRollbackError"
    }
    $CleanupSuffix = if ($CleanupErrors.Count -gt 0) {
        " Exact preparation cleanup was incomplete: $($CleanupErrors -join '; ')"
    } else {
        ""
    }
    throw "Could not validate the existing daemon/service state; nothing was replaced: $StatusError.$CleanupSuffix"
}

$InstallError = $null
$VersionOutput = $null
$OldBinaryBackedUp = $false
$NewBinaryPublished = $false
$PathMutationAttempted = $false
$RequestedUserPathSnapshot = $OriginalUserPathSnapshot
$OriginalProcessPathSnapshot = $null
$RequestedProcessPathSnapshot = $null
$ProcessPathMutationAttempted = $false
$ProcessPathStateAmbiguous = $false
$InstallPostCommitErrors = @()
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
            -ExpectedStagedPath $StagedBin `
            -ExpectedBackupPath $BackupBin
        if ($DaemonServiceInstalled) {
            Assert-CandidateServiceOwner `
                -CandidatePath $CandidateBin `
                -ExpectedExecutable $InstalledBin
        }
        if ((Get-InstallerFileToken -CandidatePath $CandidateBin -Path $StagedBin) -cne $StagedBinaryToken) {
            throw "Staged candidate identity changed before binary publication."
        }
        if ($ExistingBinaryWasPresent -and
            ((Get-InstallerFileToken -CandidatePath $CandidateBin -Path $InstalledBin) -cne $PreviousBinaryToken -or
                (Get-InstallerFileToken -CandidatePath $CandidateBin -Path $BackupBin) -cne $BackupBinaryToken)) {
            throw "Existing binary or independent rollback copy changed before publication."
        }
        if (-not $ExistingBinaryWasPresent -and (Test-DirectInstalledBinary -Path $InstalledBin)) {
            throw "First-install transaction files changed before binary publication."
        }
    } catch {
        $AmbiguousBinaryState = $true
        throw
    }

    if ($ExistingBinaryWasPresent) {
        $Publication = Invoke-ClassifiedInstallerReplace `
            -CandidatePath $CandidateBin `
            -ReplacementPath $StagedBin `
            -DestinationPath $InstalledBin `
            -DisplacedPath $DisplacedBin `
            -ReplacementToken $StagedBinaryToken `
            -DestinationToken $PreviousBinaryToken

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
        $Publication = Invoke-ClassifiedInstallerMoveNoReplace `
            -CandidatePath $CandidateBin `
            -SourcePath $StagedBin `
            -DestinationPath $InstalledBin `
            -SourceToken $StagedBinaryToken
        if ($Publication.State -ceq "Published") {
            $NewBinaryPublished = $true
            if ($null -ne $Publication.OperationError) {
                throw "First-install publication reached the exact state but its boundary reported an error: $($Publication.OperationError)"
            }
        } elseif ($Publication.State -ceq "Unchanged") {
            throw "First-install no-replace publication did not apply: $($Publication.OperationError)"
        } else {
            $AmbiguousBinaryState = $true
            throw "First-install publication produced an ambiguous file state: operation=$($Publication.OperationError); inspection=$($Publication.InspectionError)"
        }
    }

    $UserPathTransition = Set-ExactUserPathTransition `
        -CandidatePath $CandidateBin `
        -TemporaryDirectory $TmpDir `
        -Action "add" `
        -ExpectedSnapshot $OriginalUserPathSnapshot `
        -Entry $InstallDir
    $RequestedUserPathSnapshot = $UserPathTransition.Snapshot
    $PathMutationAttempted = $RequestedUserPathSnapshot -cne $OriginalUserPathSnapshot
    if (-not $UserPathTransition.NotificationSucceeded) {
        Write-Host "[warn]  User PATH committed, but Windows environment notification failed ($($UserPathTransition.Notification)). New terminals may require sign-out/sign-in." -ForegroundColor Yellow
    }

    $ProcessPathTransition = Invoke-ExactProcessPathTransition -Action "add" -Entry $InstallDir
    $OriginalProcessPathSnapshot = $ProcessPathTransition.Original
    $RequestedProcessPathSnapshot = $ProcessPathTransition.Requested
    $ProcessPathMutationAttempted = $ProcessPathTransition.Applied
    $ProcessPathStateAmbiguous = $ProcessPathTransition.Ambiguous
    if ($null -ne $ProcessPathTransition.Error -or $ProcessPathStateAmbiguous) {
        Write-Host "[warn]  User PATH was committed, but the current PowerShell process PATH update was not confirmed exactly: $($ProcessPathTransition.Error)" -ForegroundColor Yellow
    }
    if ($PathMutationAttempted) {
        Write-Host "[info]  Added $InstallDir to user PATH" -ForegroundColor Blue
    }

    $VersionOutput = & $InstalledBin --version 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "installed binary version check exited with code ${LASTEXITCODE}: $VersionOutput"
    }
    $InstalledVersionLine = (($VersionOutput | Select-Object -First 1) -as [string]).Trim()
    if ($InstalledVersionLine -cne "codex-switch-global-pace $ExpectedReleaseVersion") {
        throw "installed binary reported '$InstalledVersionLine', expected 'codex-switch-global-pace $ExpectedReleaseVersion'"
    }

    Invoke-DaemonLifecycleCommand -Holder $InstallLifecycleHolder -Command "new"

    try {
        Invoke-DaemonLifecycleCommand -Holder $InstallLifecycleHolder -Command "finish"
    } catch {
        $InstallPostCommitErrors += "daemon lifecycle finalization failed after the replacement state became ready; exact recovery paths were preserved: $_"
    }
    if ($InstallPostCommitErrors.Count -eq 0 -and $OldBinaryBackedUp) {
        try {
            if ((Get-InstallerFileToken -CandidatePath $CandidateBin -Path $InstalledBin) -cne $StagedBinaryToken -or
                (Get-InstallerFileToken -CandidatePath $CandidateBin -Path $DisplacedBin) -cne $PreviousBinaryToken -or
                (Get-InstallerFileToken -CandidatePath $CandidateBin -Path $BackupBin) -cne $BackupBinaryToken) {
                throw "Binary state changed before the rollback backup commit boundary."
            }
            Remove-InstallerOwnedFile `
                -CandidatePath $CandidateBin `
                -Path $BackupBin `
                -ExpectedToken $BackupBinaryToken
            $BackupBinaryToken = $null
            Remove-InstallerOwnedFile `
                -CandidatePath $CandidateBin `
                -Path $DisplacedBin `
                -ExpectedToken $PreviousBinaryToken
            $OldBinaryBackedUp = $false
        } catch {
            $InstallPostCommitErrors += "binary recovery cleanup failed; exact recovery paths were preserved: $_"
        }
    }
    if ($InstallLifecycleHolder.Phase -ceq "FinalConfirmed") {
        try {
            Invoke-DaemonLifecycleCommand -Holder $InstallLifecycleHolder -Command "release"
        } catch {
            $InstallPostCommitErrors += "daemon lifecycle authority release failed after recovery cleanup: $_"
        }
    }
} catch {
    $InstallError = $_
}

if ($null -ne $InstallError) {
    $RollbackErrors = @()

    $DaemonSafeForBinaryRollback = $null -ne $InstallLifecycleHolder -and
        $InstallLifecycleHolder.Phase -ceq "Stopped"
    if (-not $DaemonSafeForBinaryRollback) {
        $RollbackErrors += "daemon lifecycle authority was not retained in the stopped phase; automatic binary rollback was refused"
    }

    $NewBinaryMovedAside = $false
    if ($AmbiguousBinaryState) {
        $RollbackErrors += "binary publication state is ambiguous; the installed, staged, rollback, and failed paths were preserved for explicit recovery"
    } elseif (-not $DaemonSafeForBinaryRollback) {
        $RollbackErrors += "the previous binary remains preserved at $BackupBin; automatic binary rollback was refused"
    } elseif ($NewBinaryPublished -and $OldBinaryBackedUp) {
        $Rollback = Invoke-ClassifiedInstallerReplace `
            -CandidatePath $CandidateBin `
            -ReplacementPath $DisplacedBin `
            -DestinationPath $InstalledBin `
            -DisplacedPath $FailedBin `
            -ReplacementToken $PreviousBinaryToken `
            -DestinationToken $StagedBinaryToken

        if ($Rollback.State -ceq "Published") {
            $OldBinaryBackedUp = $false
            $PreviousBinaryRestored = $true
            $NewBinaryPublished = $false
            $NewBinaryMovedAside = $true

            if ($null -ne $Rollback.OperationError) {
                $RollbackErrors += "atomic rollback reported an error after restoring the exact previous bytes; the failed candidate was preserved at ${FailedBin}: $($Rollback.OperationError)"
            } else {
                try {
                    Remove-InstallerOwnedFile `
                        -CandidatePath $CandidateBin `
                        -Path $FailedBin `
                        -ExpectedToken $StagedBinaryToken
                    $NewBinaryMovedAside = $false
                } catch {
                    $RollbackErrors += "the previous binary was restored, but the failed candidate could not be removed from ${FailedBin}: $_"
                }
            }
            if ($null -ne $BackupBinaryToken) {
                try {
                    Remove-InstallerOwnedFile `
                        -CandidatePath $CandidateBin `
                        -Path $BackupBin `
                        -ExpectedToken $BackupBinaryToken
                    $BackupBinaryToken = $null
                } catch {
                    $RollbackErrors += "the previous binary was restored, but the independent recovery copy remains at ${BackupBin}: $_"
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
            Remove-InstallerOwnedFile `
                -CandidatePath $CandidateBin `
                -Path $InstalledBin `
                -ExpectedToken $StagedBinaryToken
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
            $RestoredUserPath = Restore-ExactUserPathTransition `
                -CandidatePath $CandidateBin `
                -TemporaryDirectory $TmpDir `
                -OriginalSnapshot $OriginalUserPathSnapshot `
                -RequestedSnapshot $RequestedUserPathSnapshot
            if (-not $RestoredUserPath.NotificationSucceeded) {
                $RollbackErrors += "user PATH was restored, but Windows environment notification failed ($($RestoredUserPath.Notification))"
            }
            $PathMutationAttempted = $false
        } catch {
            $RollbackErrors += "could not restore the exact previous User PATH: $_"
        }
    }

    if ($ProcessPathMutationAttempted) {
        try {
            Restore-ExactProcessPathTransition `
                -Original $OriginalProcessPathSnapshot `
                -Requested $RequestedProcessPathSnapshot
            $ProcessPathMutationAttempted = $false
        } catch {
            $RollbackErrors += "could not restore the exact previous current-process PATH: $_"
        }
    } elseif ($ProcessPathStateAmbiguous) {
        $RollbackErrors += "current PowerShell process PATH changed independently and was preserved"
    }

    if (-not $AmbiguousBinaryState -and $DaemonSafeForBinaryRollback -and
        $PreviousBinaryRestored) {
        try {
            Invoke-DaemonLifecycleCommand `
                -Holder $InstallLifecycleHolder `
                -Command "rollback"
        } catch {
            $RollbackErrors += "could not restore and verify the exact daemon state after rollback: $_"
        }
    }

    if (-not $AmbiguousBinaryState) {
        $StageTokenAfter = Get-InstallerFileTokenIfPresent -CandidatePath $CandidateBin -Path $StagedBin
        if ($StageTokenAfter -ceq $StagedBinaryToken) {
            $StageCleanupError = Remove-InstallerArtifactIfOwned `
                -CandidatePath $CandidateBin `
                -Path $StagedBin `
                -ExpectedToken $StagedBinaryToken
            if ($null -ne $StageCleanupError) {
                $RollbackErrors += "staged candidate remains preserved: $StageCleanupError"
            }
        } elseif ($null -ne $StageTokenAfter) {
            $RollbackErrors += "a foreign staged path was preserved at $StagedBin"
        }
    }
    if ($NewBinaryMovedAside) {
        $RollbackErrors += "failed candidate remains at $FailedBin"
    }
    if (-not $AmbiguousBinaryState -and -not $InstallDirWasPresent) {
        try {
            Remove-NewEmptyInstallDirectory -Path $InstallDir
        } catch {
            $RollbackErrors += "could not remove the newly created empty install directory: $_"
        }
    }
    if ($RollbackErrors.Count -eq 0) {
        throw "Installation failed and the previous binary, User PATH, and daemon state were restored: $InstallError"
    }
    throw "Installation failed: $InstallError. Rollback was incomplete: $($RollbackErrors -join '; ')"
}
$TransactionSucceeded = $true
} finally {
    $LifecycleReleaseError = $null
    if ($null -ne $InstallLifecycleHolder -and
        $InstallLifecycleHolder.Phase -cne "Closed") {
        try {
            Close-DaemonLifecycleHolder `
                -Holder $InstallLifecycleHolder `
                -ExpectSuccess $false
        } catch {
            $LifecycleReleaseError = $_
        }
    }
    $LockReleaseError = $null
    try {
        Complete-UpdateLockHolder -LockProcess $UpdateLockHolder
    } catch {
        $LockReleaseError = $_
    }
    if ($null -ne $LockReleaseError) {
        $LockMessage = "The exclusive update lock did not close cleanly: $LockReleaseError"
        if ($TransactionSucceeded) {
            throw "Installer transaction completed, but $LockMessage"
        }
        Write-Host "[error] Additionally, $LockMessage" -ForegroundColor Red
    }
    if ($null -ne $LifecycleReleaseError) {
        if ($TransactionSucceeded) {
            throw "Installer transaction reached its replacement state, but the daemon lifecycle holder did not close cleanly: $LifecycleReleaseError"
        }
        Write-Host "[error] Additionally, the daemon lifecycle holder did not close cleanly: $LifecycleReleaseError" -ForegroundColor Red
    }
}

if ($InstallPostCommitErrors.Count -gt 0) {
    throw "Installation committed, but post-commit cleanup was incomplete: $($InstallPostCommitErrors -join '; ')"
}
Write-Host "[info]  Installed: $VersionOutput" -ForegroundColor Blue
Write-Host "[info]  Run 'codex-switch-global-pace --help' to get started" -ForegroundColor Blue
} catch {
    $InstallerFailure = $_
    Write-Host "[error] $($_.Exception.Message)" -ForegroundColor Red
} finally {
    try {
        if ($null -ne $TmpDir -and (Test-Path -LiteralPath $TmpDir)) {
            $TempItem = Get-Item -LiteralPath $TmpDir -Force
            $ExpectedTempParent = [System.IO.Path]::GetFullPath([System.IO.Path]::GetTempPath()).TrimEnd('\')
            $ObservedTempParent = [System.IO.Path]::GetFullPath($TempItem.Parent.FullName).TrimEnd('\')
            if (-not $TempItem.PSIsContainer -or
                ($TempItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0 -or
                -not [System.StringComparer]::OrdinalIgnoreCase.Equals($ExpectedTempParent, $ObservedTempParent) -or
                $TempItem.Name -cnotmatch '^codex-switch-global-pace-install-[0-9a-f]{32}$') {
                throw "Refused to recursively clean an unexpected installer temp path: $TmpDir"
            }
            Remove-Item -LiteralPath $TmpDir -Recurse -Force
        }
    } catch {
        $TempCleanupError = $_
    }
}
if ($null -ne $InstallerFailure) {
    if ($null -ne $TempCleanupError) {
        throw "$($InstallerFailure.Exception.Message) Additionally, exact installer temp cleanup failed: $TempCleanupError"
    }
    throw $InstallerFailure
}
if ($null -ne $TempCleanupError) {
    throw "Installer work completed, but exact installer temp cleanup failed: $TempCleanupError"
}
}
