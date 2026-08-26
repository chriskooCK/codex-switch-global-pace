[CmdletBinding()]
param(
    [Parameter()]
    [long]$RunId
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$Repo = 'chriskooCK/codex-switch-global-pace'
$ApiVersion = '2026-03-10'
$RemoteLockTag = 'codex-switch-publish-dev-lock'
$Utf8 = New-Object System.Text.UTF8Encoding($false, $true)
$Archives = @(
    'codex-switch-global-pace-linux-amd64.tar.gz',
    'codex-switch-global-pace-linux-arm64.tar.gz',
    'codex-switch-global-pace-darwin-amd64.tar.gz',
    'codex-switch-global-pace-darwin-arm64.tar.gz',
    'codex-switch-global-pace-windows-amd64.zip',
    'codex-switch-global-pace-windows-arm64.zip'
)
$Assets = @(
    'INSTALL.md', 'codex-switch-global-pace-build-provenance.json',
    'codex-switch-global-pace-darwin-amd64.tar.gz',
    'codex-switch-global-pace-darwin-amd64.tar.gz.sha256',
    'codex-switch-global-pace-darwin-arm64.tar.gz',
    'codex-switch-global-pace-darwin-arm64.tar.gz.sha256',
    'codex-switch-global-pace-linux-amd64.tar.gz',
    'codex-switch-global-pace-linux-amd64.tar.gz.sha256',
    'codex-switch-global-pace-linux-arm64.tar.gz',
    'codex-switch-global-pace-linux-arm64.tar.gz.sha256',
    'codex-switch-global-pace-windows-amd64.zip',
    'codex-switch-global-pace-windows-amd64.zip.sha256',
    'codex-switch-global-pace-windows-arm64.zip',
    'codex-switch-global-pace-windows-arm64.zip.sha256',
    'install.ps1', 'install.sh'
)

if ($PSVersionTable.PSVersion -lt [Version]'5.1') { throw 'PowerShell 5.1 or newer is required.' }
if ($PSBoundParameters.ContainsKey('RunId') -and $RunId -le 0) {
    throw '-RunId must be a positive Actions run ID.'
}
if ($env:GH_TOKEN -or $env:GITHUB_TOKEN) {
    throw 'Unset GH_TOKEN and GITHUB_TOKEN; this publisher requires the locally authenticated gh user.'
}
$gh = Get-Command gh -ErrorAction Stop
$git = Get-Command git -ErrorAction Stop
$RepoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$TempBase = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\', '/')
$Temp = Join-Path $TempBase ('codex-switch-publish-dev-' + [Guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($Temp)
$Seq = 0

function RunCommand([string]$Executable, [string[]]$CommandArguments, [string]$What, [switch]$AllowFailure) {
    $script:Seq++
    $err = Join-Path $script:Temp ('command-{0:D3}.err' -f $script:Seq)
    try {
        $lines = @(& $Executable @CommandArguments 2> $err)
        $code = $LASTEXITCODE
        $out = [string]::Join("`n", @($lines | ForEach-Object { [string]$_ }))
        $errorText = if ([IO.File]::Exists($err)) { [IO.File]::ReadAllText($err).Trim() } else { '' }
        $result = [pscustomobject]@{ Code = $code; Out = $out; Error = $errorText }
        if ($code -ne 0 -and -not $AllowFailure) {
            if (-not $errorText) { $errorText = 'no diagnostic text' }
            $program = [IO.Path]::GetFileName($Executable)
            throw "$What failed ($program exit $code): $errorText"
        }
        return $result
    }
    finally { if ([IO.File]::Exists($err)) { Remove-Item -LiteralPath $err -Force } }
}

function RunGh([string[]]$CommandArguments, [string]$What, [switch]$AllowFailure) {
    return RunCommand $script:gh.Source $CommandArguments $What -AllowFailure:$AllowFailure
}

function RunGit([string[]]$CommandArguments, [string]$What, [switch]$AllowFailure) {
    return RunCommand $script:git.Source $CommandArguments $What -AllowFailure:$AllowFailure
}

function ApiArgs([string]$Endpoint, [string]$Method, [string]$InputPath) {
    $a = @('api', '--hostname', 'github.com', '-H', 'Accept: application/vnd.github+json',
        '-H', "X-GitHub-Api-Version: $script:ApiVersion")
    if ($Method) { $a += @('--method', $Method) }
    if ($InputPath) { $a += @('--input', $InputPath) }
    return @($a + $Endpoint)
}

function Json([string]$Endpoint, [string]$What) {
    $r = RunGh (ApiArgs $Endpoint '' '') $What
    if (-not $r.Out) { throw "$What returned an empty response." }
    try { return ($r.Out | ConvertFrom-Json) } catch { throw "$What returned invalid JSON." }
}

function Maybe([string]$Endpoint, [string]$What) {
    $r = RunGh (ApiArgs $Endpoint '' '') $What -AllowFailure
    if ($r.Code -eq 0) { return [pscustomobject]@{ Found = $true; Value = ($r.Out | ConvertFrom-Json) } }
    if ($r.Error -match '(?i)(HTTP 404|Not Found)') { return [pscustomobject]@{ Found = $false; Value = $null } }
    throw "$What failed (gh exit $($r.Code)): $($r.Error)"
}

function Payload([string]$Name, [object]$Value) {
    $p = Join-Path $script:Temp $Name
    [IO.File]::WriteAllText($p, ($Value | ConvertTo-Json -Compress -Depth 20), $script:Utf8)
    return $p
}

function Mutate([string]$Endpoint, [string]$Method, [string]$InputPath, [string]$What) {
    return RunGh (ApiArgs $Endpoint $Method $InputPath) $What -AllowFailure
}

function Prop([object]$Object, [string]$Name) {
    $p = $Object.PSObject.Properties[$Name]
    if ($null -eq $p) { throw "GitHub response lacks '$Name'." }
    return $p.Value
}

function Same([object]$A, [object]$B) {
    if ($null -eq $A -or $null -eq $B) { return ($null -eq $A -and $null -eq $B) }
    return [string]::Equals([string]$A, [string]$B, [StringComparison]::Ordinal)
}
function SameSha([object]$A, [object]$B) {
    return ($null -ne $A -and $null -ne $B -and
        [string]::Equals([string]$A, [string]$B, [StringComparison]::OrdinalIgnoreCase))
}
function Hash([string]$Path) { return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() }
function SafeWarning([string]$Message) {
    try { Write-Warning $Message -WarningAction Continue } catch {}
}

function Ref([string]$Tag) { return Maybe "repos/$script:Repo/git/ref/tags/$Tag" "Read refs/tags/$Tag" }
function ResolveDev {
    $r = Ref 'dev'; if (-not $r.Found) { throw 'refs/tags/dev does not exist.' }
    $kind = [string](Prop (Prop $r.Value 'object') 'type'); $sha = [string](Prop (Prop $r.Value 'object') 'sha')
    foreach ($depth in 0..5) {
        if ($kind -eq 'commit') { if ($sha -notmatch '^[0-9a-fA-F]{40}$') { throw 'dev has an invalid commit SHA.' }; return $sha.ToLowerInvariant() }
        if ($kind -ne 'tag') { throw "dev resolves to unsupported object $kind."
        }
        if ($depth -eq 5) { break }
        $o = Json "repos/$script:Repo/git/tags/$sha" 'Resolve annotated dev tag'
        $kind = [string](Prop (Prop $o 'object') 'type'); $sha = [string](Prop (Prop $o 'object') 'sha')
    }
    throw 'dev has more than five nested annotated tags.'
}
function AssertDev([string]$Sha) { if (-not (SameSha (ResolveDev) $Sha)) { throw 'refs/tags/dev moved during publication.' } }
function AssertNoRef([string]$Tag) { if ((Ref $Tag).Found) { throw "Unexpected Git ref refs/tags/$Tag exists." } }

function AssertRemotePublicationLock([hashtable]$Lock) {
    $current = Ref $script:RemoteLockTag
    if (-not $current.Found) { throw 'The remote development-publication lock disappeared.' }
    $refObject = Prop $current.Value 'object'
    if (-not (Same (Prop $current.Value 'ref') "refs/tags/$script:RemoteLockTag") -or
        -not (Same (Prop $refObject 'type') 'tag') -or
        -not (SameSha (Prop $refObject 'sha') $Lock.TagObjectSha)) {
        throw 'The remote development-publication lock changed identity.'
    }

    $tagObject = Json "repos/$script:Repo/git/tags/$($Lock.TagObjectSha)" 'Verify remote development-publication lock object'
    $target = Prop $tagObject 'object'
    if (-not (SameSha (Prop $tagObject 'sha') $Lock.TagObjectSha) -or
        -not (Same (Prop $tagObject 'tag') $script:RemoteLockTag) -or
        -not (Same (Prop $tagObject 'message') $Lock.Message) -or
        -not (Same (Prop $target 'type') 'commit') -or
        -not (SameSha (Prop $target 'sha') $Lock.SourceSha)) {
        throw 'The remote development-publication lock object changed identity.'
    }
}

function AcquireRemotePublicationLock([string]$SourceSha) {
    $existing = Ref $script:RemoteLockTag
    if ($existing.Found) {
        throw "Remote lock refs/tags/$script:RemoteLockTag already exists; it was not acquired or removed."
    }

    $transaction = [Guid]::NewGuid().ToString('N')
    $message = "codex-switch-global-pace publish-dev lock v1|repo=$script:Repo|source=$($SourceSha.ToLowerInvariant())|transaction=$transaction"
    $tagRequest = [ordered]@{
        tag = $script:RemoteLockTag
        message = $message
        object = $SourceSha
        type = 'commit'
    }
    $tagResult = Mutate "repos/$script:Repo/git/tags" 'POST' (Payload 'remote-lock-tag.json' $tagRequest) `
        'Create remote development-publication lock object'
    if ($tagResult.Code -ne 0) {
        throw "Remote lock object creation failed or its response was lost; no lock ownership was claimed: $($tagResult.Error)"
    }
    try { $tagObject = $tagResult.Out | ConvertFrom-Json }
    catch { throw 'Remote lock object creation returned invalid JSON; no lock ownership was claimed.' }
    $tagObjectSha = [string](Prop $tagObject 'sha')
    $tagTarget = Prop $tagObject 'object'
    if ($tagObjectSha -notmatch '^[0-9a-fA-F]{40}$' -or
        -not (Same (Prop $tagObject 'tag') $script:RemoteLockTag) -or
        -not (Same (Prop $tagObject 'message') $message) -or
        -not (Same (Prop $tagTarget 'type') 'commit') -or
        -not (SameSha (Prop $tagTarget 'sha') $SourceSha)) {
        throw 'Remote lock object creation returned an unexpected identity; no lock ownership was claimed.'
    }

    $refRequest = [ordered]@{ ref = "refs/tags/$script:RemoteLockTag"; sha = $tagObjectSha }
    $refResult = Mutate "repos/$script:Repo/git/refs" 'POST' (Payload 'remote-lock-ref.json' $refRequest) `
        'Acquire remote development-publication lock'
    if ($refResult.Code -ne 0) {
        throw "Remote lock acquisition failed or its response was lost; the lock was not claimed and will not be removed automatically: $($refResult.Error)"
    }
    try { $createdRef = $refResult.Out | ConvertFrom-Json }
    catch { throw 'Remote lock acquisition returned invalid JSON; the lock was not claimed and will not be removed automatically.' }
    $createdObject = Prop $createdRef 'object'
    if (-not (Same (Prop $createdRef 'ref') "refs/tags/$script:RemoteLockTag") -or
        -not (Same (Prop $createdObject 'type') 'tag') -or
        -not (SameSha (Prop $createdObject 'sha') $tagObjectSha)) {
        throw 'Remote lock acquisition returned an unexpected identity; the lock was not claimed and will not be removed automatically.'
    }

    $lock = @{
        TagObjectSha = $tagObjectSha.ToLowerInvariant()
        SourceSha = $SourceSha.ToLowerInvariant()
        Message = $message
        Transaction = $transaction
    }
    AssertRemotePublicationLock $lock
    return $lock
}

function ReleaseRemotePublicationLock([hashtable]$Lock) {
    AssertRemotePublicationLock $Lock
    $refName = "refs/tags/$script:RemoteLockTag"
    $lockEndpoint = "repos/$script:Repo/git/refs/tags/$script:RemoteLockTag"
    $lease = "--force-with-lease=$refName`:$($Lock.TagObjectSha)"
    $delete = RunGit @('-C', $script:RepoRoot, '-c', 'credential.helper=',
        '-c', 'credential.helper=!gh auth git-credential',
        'push', '--porcelain', '--no-verify', $lease, "https://github.com/$script:Repo.git", ":$refName") `
        'Release remote development-publication lock' -AllowFailure
    $after = Ref $script:RemoteLockTag
    if ($after.Found) {
        $afterObject = Prop $after.Value 'object'
        if ((Same (Prop $after.Value 'ref') "refs/tags/$script:RemoteLockTag") -and
            (Same (Prop $afterObject 'type') 'tag') -and
            (SameSha (Prop $afterObject 'sha') $Lock.TagObjectSha)) {
            throw "The exact remote development-publication lock at $lockEndpoint remains after leased deletion: $($delete.Error)"
        }
        throw 'The remote development-publication lock changed identity during leased deletion and was preserved.'
    }
}

function AssertRemotePublicationMutationLock {
    if (-not $script:RemoteLockOwned -or $null -eq $script:RemoteLock) {
        throw 'A remote development-publication mutation was attempted without the owned lock.'
    }
    AssertRemotePublicationLock $script:RemoteLock
}

function Pages([string]$Base, [string]$ArrayProperty) {
    [object[]]$all = @(); $page = 1
    do {
        $j = Json "$Base&per_page=100&page=$page" "Read page $page"
        $value = if ($ArrayProperty) { Prop $j $ArrayProperty } else { $j }
        [object[]]$batch = @()
        if ($null -ne $value) { $batch = [object[]]@($value) }
        $all += $batch; $page++
    } while ($batch.Length -eq 100)
    return $all
}
function AllReleases { return @(Pages "repos/$script:Repo/releases?x=1" '') }
function ReleaseId([long]$Id) { return Maybe "repos/$script:Repo/releases/$Id" "Read release $Id" }
function ReleaseAnyTag([string]$Tag) {
    $matches = @(AllReleases | Where-Object { Same (Prop $_ 'tag_name') $Tag })
    if ($matches.Count -gt 1) { throw "Release tag '$Tag' is ambiguous." }
    if ($matches.Count -eq 1) { return [pscustomobject]@{ Found = $true; Value = $matches[0] } }
    return [pscustomobject]@{ Found = $false; Value = $null }
}
function ReleaseAssets([long]$Id) { return @(Pages "repos/$script:Repo/releases/$Id/assets?x=1" '') }

function RepoBytes([string]$Path, [string]$Sha) {
    $j = Json "repos/$script:Repo/contents/$Path`?ref=$Sha" "Read $Path at $Sha"
    if ((Prop $j 'type') -ne 'file' -or (Prop $j 'encoding') -ne 'base64') { throw "$Path is not a base64 file." }
    return [Convert]::FromBase64String(([string](Prop $j 'content') -replace '\s', ''))
}

function ExactFiles(
    [string]$Dir,
    [Parameter(Mandatory = $true)][AllowEmptyCollection()][string[]]$Names
) {
    $items = @(Get-ChildItem -LiteralPath $Dir -Force)
    if ($items.Count -ne $Names.Length) { throw "$Dir must contain exactly $($Names.Length) files." }
    $set = New-Object 'System.Collections.Generic.HashSet[string]' ([StringComparer]::Ordinal)
    foreach ($i in $items) {
        if ($i.PSIsContainer -or ($i.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or $i.Length -le 0) {
            throw "Invalid bundle entry '$($i.Name)'."
        }
        [void]$set.Add([string]$i.Name)
    }
    foreach ($n in $Names) { if (-not $set.Contains($n)) { throw "Bundle lacks exact file '$n'." } }
}

function AssetProjection([object[]]$Remote) {
    $rows = @()
    foreach ($a in @($Remote | Sort-Object { [long](Prop $_ 'id') })) {
        $rows += [ordered]@{ id = [long](Prop $a 'id'); name = [string](Prop $a 'name');
            label = $a.label; state = [string](Prop $a 'state'); content_type = [string](Prop $a 'content_type');
            size = [long](Prop $a 'size'); digest = $a.digest }
    }
    return (ConvertTo-Json -InputObject @($rows) -Compress -Depth 5)
}

function AppendFingerprintField([Text.StringBuilder]$Builder, [object]$Value) {
    if ($null -eq $Value) { [void]$Builder.Append('n;'); return }
    $text = [string]$Value
    [void]$Builder.Append('s')
    [void]$Builder.Append($text.Length.ToString([Globalization.CultureInfo]::InvariantCulture))
    [void]$Builder.Append(':')
    [void]$Builder.Append($text)
    [void]$Builder.Append(';')
}

function Fingerprint(
    [Parameter(Mandatory = $true)][object]$Release,
    [Parameter(Mandatory = $true)][bool]$OriginalDraft
) {
    $id = [long](Prop $Release 'id')
    $remote = @(ReleaseAssets $id | Sort-Object { [long](Prop $_ 'id') })
    $builder = New-Object Text.StringBuilder
    [void]$builder.Append('codex-switch-old-release-v2;')
    AppendFingerprintField $builder ($id.ToString([Globalization.CultureInfo]::InvariantCulture))
    AppendFingerprintField $builder $(if ($OriginalDraft) { 'draft' } else { 'public' })
    AppendFingerprintField $builder ([string](Prop $Release 'target_commitish')).ToLowerInvariant()
    AppendFingerprintField $builder (Prop $Release 'name')
    AppendFingerprintField $builder (Prop $Release 'body')
    AppendFingerprintField $builder $(if ([bool](Prop $Release 'prerelease')) { '1' } else { '0' })
    AppendFingerprintField $builder $(if ([bool](Prop $Release 'immutable')) { '1' } else { '0' })
    AppendFingerprintField $builder ($remote.Count.ToString([Globalization.CultureInfo]::InvariantCulture))
    foreach ($asset in $remote) {
        AppendFingerprintField $builder ([long](Prop $asset 'id')).ToString([Globalization.CultureInfo]::InvariantCulture)
        AppendFingerprintField $builder (Prop $asset 'name')
        AppendFingerprintField $builder (Prop $asset 'label')
        AppendFingerprintField $builder (Prop $asset 'state')
        AppendFingerprintField $builder (Prop $asset 'content_type')
        AppendFingerprintField $builder ([long](Prop $asset 'size')).ToString([Globalization.CultureInfo]::InvariantCulture)
        AppendFingerprintField $builder (Prop $asset 'digest')
    }
    $bytes = $script:Utf8.GetBytes($builder.ToString())
    $h = [Security.Cryptography.SHA256]::Create()
    try { return ([BitConverter]::ToString($h.ComputeHash($bytes))).Replace('-', '').ToLowerInvariant() } finally { $h.Dispose() }
}

function AssertState([object]$R, [long]$Id, [string]$Tag, [string]$Target, [bool]$Draft) {
    if ([long](Prop $R 'id') -ne $Id -or -not (Same (Prop $R 'tag_name') $Tag) -or
        -not (SameSha (Prop $R 'target_commitish') $Target) -or [bool](Prop $R 'draft') -ne $Draft) {
        throw "Release $Id is not in expected state tag=$Tag draft=$Draft target=$Target."
    }
}

function PriorTarget([object]$Release) {
    $target = [string](Prop $Release 'target_commitish')
    if (-not [bool](Prop $Release 'prerelease') -or [bool](Prop $Release 'immutable') -or
        $target -notmatch '^[0-9a-fA-F]{40}$') {
        throw 'Prior dev release is not a mutable SHA-bound prerelease.'
    }
    return $target
}

function LocalMap([string]$Dir) {
    $m = New-Object 'System.Collections.Generic.Dictionary[string,object]' ([StringComparer]::Ordinal)
    foreach ($n in $script:Assets) { $p = Join-Path $Dir $n; $m.Add($n, [pscustomobject]@{ Path = $p; Size = (Get-Item -LiteralPath $p).Length; Hash = (Hash $p) }) }
    return $m
}

function AssertCandidateAssets([long]$Id, [object]$Map, [switch]$Exact, [string]$ExpectedProjection) {
    $remote = @(ReleaseAssets $Id); $seen = New-Object 'System.Collections.Generic.HashSet[string]' ([StringComparer]::Ordinal)
    foreach ($a in $remote) {
        $n = [string](Prop $a 'name'); if (-not $seen.Add($n) -or -not $Map.ContainsKey($n)) { throw "Candidate has unowned asset '$n'." }
        $local = $Map[$n]; $digest = [string](Prop $a 'digest')
        if ((Prop $a 'state') -ne 'uploaded' -or [long](Prop $a 'size') -ne $local.Size -or
            -not (Same $digest ('sha256:' + $local.Hash))) { throw "Candidate asset '$n' differs from the bundle." }
    }
    if ($Exact -and $seen.Count -ne $script:Assets.Count) { throw 'Candidate does not have exactly 16 assets.' }
    $projection = AssetProjection $remote
    if ($ExpectedProjection -and -not (Same $projection $ExpectedProjection)) { throw 'Candidate asset identity changed.' }
    return $projection
}

function AssertCandidateMetadata([hashtable]$C, [object]$R) {
    $id = [long](Prop $R 'id'); if ($C.CandidateId -and $id -ne $C.CandidateId) { throw 'Candidate release ID changed.' }
    if ($id -le 0) { throw 'Candidate release ID is not positive.' }
    $stagedState = ((Prop $R 'tag_name') -eq $C.CandidateTag -and [bool](Prop $R 'draft'))
    $finalState = ((Prop $R 'tag_name') -eq 'dev' -and -not [bool](Prop $R 'draft'))
    if (-not ($stagedState -or $finalState) -or -not (SameSha (Prop $R 'target_commitish') $C.Sha) -or
        -not (Same (Prop $R 'name') $C.Name) -or -not (Same (Prop $R 'body') $C.Body) -or
        -not [bool](Prop $R 'prerelease') -or [bool](Prop $R 'immutable')) {
        throw 'Candidate release metadata changed.'
    }
    return [pscustomobject]@{ Release = $R; Id = $id; Staged = $stagedState; Final = $finalState }
}

function AcceptCreatedCandidate([hashtable]$C, [object]$Result) {
    if ($Result.Code -ne 0) {
        throw "Candidate creation failed or its response was lost; the deterministic journal was preserved: $($Result.Error)"
    }
    if (-not $Result.Out) {
        throw 'Candidate creation returned an empty response; the deterministic journal was preserved.'
    }
    try { $candidate = $Result.Out | ConvertFrom-Json }
    catch { throw 'Candidate creation returned invalid JSON; the deterministic journal was preserved.' }

    $state = AssertCandidateMetadata $C $candidate
    if (-not $state.Staged -or $state.Final) {
        throw 'Candidate creation did not return the exact staged state; the deterministic journal was preserved.'
    }
    $assetsProperty = $candidate.PSObject.Properties['assets']
    if ($null -eq $assetsProperty -or $null -eq ($assetsProperty.Value)) {
        throw 'Candidate creation response did not contain an explicit empty asset set; the deterministic journal was preserved.'
    }
    [object[]]$responseAssets = [object[]]@($assetsProperty.Value)
    if ($responseAssets.Length -ne 0) {
        throw 'Candidate creation response did not contain an explicit empty asset set; the deterministic journal was preserved.'
    }

    $C.CandidateId = $state.Id
    $C.CandidateCreateAmbiguous = $false
    $C.CandidateCreated = $true
    return $state
}

function AssertCandidate([hashtable]$C, [object]$R, [switch]$ExactAssets) {
    $state = AssertCandidateMetadata $C $R
    $projection = AssertCandidateAssets $state.Id $C.Local -Exact:([bool]$ExactAssets) `
        -ExpectedProjection $C.CandidateProjection
    return [pscustomobject]@{ Release = $state.Release; Id = $state.Id; Staged = $state.Staged;
        Final = $state.Final; Assets = $projection }
}

function DownloadExact([string]$Tag, [string]$Dir, [object]$Map) {
    [void][IO.Directory]::CreateDirectory($Dir)
    RunGh @('release', 'download', $Tag, '--repo', $script:Repo, '--dir', $Dir) "Download release $Tag" | Out-Null
    ExactFiles $Dir $script:Assets
    foreach ($n in $script:Assets) { if ((Hash (Join-Path $Dir $n)) -ne $Map[$n].Hash) { throw "Remote asset '$n' differs byte-for-byte." } }
}

function DownloadProjection([string]$Tag, [string]$Dir, [object]$Map, [string]$Projection) {
    [object[]]$rows = @()
    if ($Projection) {
        $parsed = $Projection | ConvertFrom-Json
        if ($null -ne $parsed) { $rows = [object[]]@($parsed) }
    }
    [string[]]$names = [string[]]@($rows | ForEach-Object { [string](Prop $_ 'name') })
    [void][IO.Directory]::CreateDirectory($Dir)
    if ($names.Length -gt 0) {
        RunGh @('release', 'download', $Tag, '--repo', $script:Repo, '--dir', $Dir) "Download release $Tag subset" | Out-Null
    }
    ExactFiles -Dir $Dir -Names $names
    foreach ($n in $names) {
        if (-not $Map.ContainsKey($n) -or (Hash (Join-Path $Dir $n)) -ne $Map[$n].Hash) {
            throw "Remote asset '$n' is not an exact local-bundle subset member."
        }
    }
}

function RemoveRelease([long]$Id) {
    AssertRemotePublicationMutationLock
    $r = Mutate "repos/$script:Repo/releases/$Id" 'DELETE' '' "Delete release $Id"
    $after = ReleaseId $Id
    if ($after.Found) { throw "Release $Id remains after deletion: $($r.Error)" }
}

function FindCandidate([hashtable]$C) {
    if ($C.CandidateId) { $r = ReleaseId $C.CandidateId; if ($r.Found) { return $r.Value }; return $null }
    $matches = @(AllReleases | Where-Object { (Prop $_ 'tag_name') -eq $C.CandidateTag })
    if ($matches.Count -gt 1) { throw 'Candidate tag is ambiguous.' }
    if ($matches.Count -eq 1) { $C.CandidateId = [long](Prop $matches[0] 'id'); return $matches[0] }
    return $null
}

function DiscoverJournal {
    $candidates = @()
    $parks = @()
    foreach ($release in @(AllReleases)) {
        $tag = [string](Prop $release 'tag_name')
        if ($tag.StartsWith('dev-candidate', [StringComparison]::Ordinal)) {
            if ($tag -notmatch '\Adev-candidate-([1-9][0-9]*)-(draft|public)-([0-9a-f]{64})-([0-9a-f]{32})\z') {
                throw "Malformed development publication journal tag '$tag'."
            }
            $candidates += [pscustomobject]@{
                Release = $release
                Tag = $tag
                OldId = [long]$Matches[1]
                OldDraft = ($Matches[2] -eq 'draft')
                OldFingerprint = $Matches[3]
                Tx = $Matches[4]
                CandidateId = [long](Prop $release 'id')
            }
        }
        elseif ($tag.StartsWith('dev-park', [StringComparison]::Ordinal)) {
            if ($tag -notmatch '\Adev-park-([1-9][0-9]*)-([1-9][0-9]*)-(draft|public)-([0-9a-f]{64})-([0-9a-f]{32})\z') {
                throw "Malformed development publication journal tag '$tag'."
            }
            $oldId = [long]$Matches[1]
            $parks += [pscustomobject]@{
                Release = $release
                Tag = $tag
                OldId = $oldId
                CandidateId = [long]$Matches[2]
                OldDraft = ($Matches[3] -eq 'draft')
                OldFingerprint = $Matches[4]
                Tx = $Matches[5]
            }
            if ([long](Prop $release 'id') -ne $oldId) {
                throw "Park journal '$tag' does not identify its own release ID."
            }
        }
    }

    if ($candidates.Count -eq 0 -and $parks.Count -eq 0) { return $null }
    if ($candidates.Count -gt 1 -or $parks.Count -gt 1) {
        throw 'Multiple development publication journals exist; no remote state was changed.'
    }

    $candidate = if ($candidates.Count -eq 1) { $candidates[0] } else { $null }
    $park = if ($parks.Count -eq 1) { $parks[0] } else { $null }
    $source = if ($null -ne $candidate) { $candidate } else { $park }
    if ($null -ne $candidate -and $null -ne $park) {
        if ($candidate.OldId -ne $park.OldId -or
            $candidate.OldDraft -ne $park.OldDraft -or
            -not (Same $candidate.OldFingerprint $park.OldFingerprint) -or
            -not (Same $candidate.Tx $park.Tx) -or
            $candidate.CandidateId -ne $park.CandidateId) {
            throw 'Development candidate and park journals do not describe one transaction.'
        }
    }

    $candidateId = if ($null -ne $candidate) { $candidate.CandidateId } else { $park.CandidateId }
    $oldVisibility = if ($source.OldDraft) { 'draft' } else { 'public' }
    $candidateTag = "dev-candidate-$($source.OldId)-$oldVisibility-$($source.OldFingerprint)-$($source.Tx)"
    $parkTag = "dev-park-$($source.OldId)-$candidateId-$oldVisibility-$($source.OldFingerprint)-$($source.Tx)"
    if ($null -ne $candidate -and -not (Same $candidate.Tag $candidateTag)) {
        throw 'Development candidate journal did not round-trip exactly.'
    }
    if ($null -ne $park -and -not (Same $park.Tag $parkTag)) {
        throw 'Development park journal did not round-trip exactly.'
    }
    return [pscustomobject]@{
        OldId = [long]$source.OldId
        CandidateId = [long]$candidateId
        OldDraft = [bool]$source.OldDraft
        OldFingerprint = [string]$source.OldFingerprint
        Tx = [string]$source.Tx
        CandidateTag = $candidateTag
        ParkTag = $parkTag
        HasCandidateJournal = ($null -ne $candidate)
        HasParkJournal = ($null -ne $park)
    }
}

function RecoverJournal([object]$Journal, [string]$Sha, [string]$Name, [string]$Body, [object]$Local) {
    AssertDev $Sha
    AssertNoRef $Journal.CandidateTag
    AssertNoRef $Journal.ParkTag

    $oldOpt = ReleaseId $Journal.OldId
    if (-not $oldOpt.Found) { throw 'Journal old release is missing; no remote state was changed.' }
    $old = $oldOpt.Value
    if ((Fingerprint $old ([bool]$Journal.OldDraft)) -ne $Journal.OldFingerprint) {
        throw 'Journal old release fingerprint differs; no remote state was changed.'
    }
    $oldTarget = PriorTarget $old
    $oldOriginal = ((Prop $old 'tag_name') -eq 'dev' -and
        [bool](Prop $old 'draft') -eq [bool]$Journal.OldDraft)
    $oldParked = ((Prop $old 'tag_name') -eq $Journal.ParkTag -and [bool](Prop $old 'draft'))
    if ($Journal.HasParkJournal -and -not $oldParked) {
        throw 'Park journal release is not the exact parked draft; no remote state was changed.'
    }
    if (-not $Journal.HasParkJournal -and -not $oldOriginal) {
        throw 'Candidate-only journal does not have the exact prior release visibility; no remote state was changed.'
    }

    $context = @{
        Sha = $Sha
        CandidateTag = $Journal.CandidateTag
        CandidateId = [long]$Journal.CandidateId
        CandidateProjection = ''
        CandidateExact = $false
        ParkTag = $Journal.ParkTag
        Local = $Local
        Name = $Name
        Body = $Body
        OldId = [long]$Journal.OldId
        OldTarget = $oldTarget
        OldName = (Prop $old 'name')
        OldBody = (Prop $old 'body')
        OldPrerelease = [bool](Prop $old 'prerelease')
        OldDraft = [bool]$Journal.OldDraft
        OldFingerprint = $Journal.OldFingerprint
    }

    $candidateOpt = ReleaseId $context.CandidateId
    if ($candidateOpt.Found) {
        $owned = AssertCandidate $context $candidateOpt.Value
        if ($owned.Final) {
            $owned = AssertCandidate $context $candidateOpt.Value -ExactAssets
            $context.CandidateExact = $true
        }
        $context.CandidateProjection = $owned.Assets
        $downloadTag = if ($owned.Final) { 'dev' } else { $context.CandidateTag }
        $downloadDir = Join-Path $script:Temp "journal-assets-$($context.CandidateId)"
        DownloadProjection $downloadTag $downloadDir $Local $context.CandidateProjection
        AssertDev $Sha
        $candidateAgain = ReleaseId $context.CandidateId
        if (-not $candidateAgain.Found) { throw 'Journal candidate disappeared during ownership verification.' }
        AssertCandidate $context $candidateAgain.Value -ExactAssets:([bool]$context.CandidateExact) | Out-Null
    }
    elseif (-not $Journal.HasParkJournal) {
        throw 'Candidate-only journal candidate is missing; no remote state was changed.'
    }

    AssertDev $Sha
    Rollback $context
    AssertDev $Sha
    Write-Host "Recovered interrupted development publication $($Journal.Tx) to prior release $($Journal.OldId)."
}

function AssertCurrentPublicExact([object]$Release, [string]$Sha, [string]$Name, [string]$Body, [object]$Local) {
    $id = [long](Prop $Release 'id')
    $context = @{
        Sha = $Sha
        CandidateTag = ''
        CandidateId = $id
        CandidateProjection = ''
        CandidateExact = $true
        Local = $Local
        Name = $Name
        Body = $Body
    }
    $owned = AssertCandidate $context $Release -ExactAssets
    $context.CandidateProjection = $owned.Assets
    $downloadDir = Join-Path $script:Temp "idempotent-release-$id"
    DownloadExact 'dev' $downloadDir $Local
    AssertDev $Sha
    $again = ReleaseId $id
    if (-not $again.Found) { throw 'Exact current dev release disappeared during verification.' }
    AssertCandidate $context $again.Value -ExactAssets | Out-Null
    $byTag = ReleaseAnyTag 'dev'
    if (-not $byTag.Found -or [long](Prop $byTag.Value 'id') -ne $id) {
        throw 'Exact current dev release changed identity during verification.'
    }
}

function Rollback([hashtable]$C) {
    AssertDev $C.Sha
    if ($C.ParkTag) { AssertNoRef $C.ParkTag }
    if ($C.CandidateTag) { AssertNoRef $C.CandidateTag }
    $oldOpt = ReleaseId $C.OldId; if (-not $oldOpt.Found) { throw 'Old release is missing; preserving candidate.' }
    $old = $oldOpt.Value; if ((Fingerprint $old ([bool]$C.OldDraft)) -ne $C.OldFingerprint) { throw 'Old release drifted; preserving both releases.' }
    $oldOriginal = ((Prop $old 'tag_name') -eq 'dev' -and
        [bool](Prop $old 'draft') -eq [bool]$C.OldDraft)
    $oldParked = ((Prop $old 'tag_name') -eq $C.ParkTag -and [bool](Prop $old 'draft'))
    if (-not ($oldOriginal -or $oldParked)) { throw 'Old release state is unowned; preserving both releases.' }
    if ($C.ContainsKey('CandidateCreateAmbiguous') -and [bool]$C.CandidateCreateAmbiguous) {
        throw 'Candidate creation response was ambiguous; the deterministic journal was preserved for rerun recovery.'
    }
    $candidate = FindCandidate $C
    if (-not $candidate -and $C.ContainsKey('CandidateCreated') -and [bool]$C.CandidateCreated) {
        throw 'The authoritatively created candidate is temporarily unavailable; its journal was preserved.'
    }
    if ($candidate) {
        $owned = AssertCandidate $C $candidate -ExactAssets:([bool]$C.CandidateExact)
        if (-not $C.CandidateProjection) {
            $C.CandidateProjection = $owned.Assets
            $downloadTag = if ($owned.Final) { 'dev' } else { $C.CandidateTag }
            $downloadDir = Join-Path $script:Temp "rollback-assets-$($owned.Id)"
            DownloadProjection $downloadTag $downloadDir $C.Local $C.CandidateProjection
            AssertDev $C.Sha
            $candidateAgain = ReleaseId $owned.Id
            if (-not $candidateAgain.Found) { throw 'Candidate disappeared during rollback ownership verification.' }
            $owned = AssertCandidate $C $candidateAgain.Value -ExactAssets:([bool]$C.CandidateExact)
        }
        $devRelease = ReleaseAnyTag 'dev'
        if ($owned.Final -and (-not $devRelease.Found -or [long](Prop $devRelease.Value 'id') -ne $owned.Id)) { throw 'Final dev release ownership is ambiguous.' }
        if ($owned.Staged -and $oldParked -and $devRelease.Found) { throw 'An unrelated dev release exists.' }
        if ($oldOriginal -and (-not $owned.Staged -or -not $devRelease.Found -or [long](Prop $devRelease.Value 'id') -ne $C.OldId)) { throw 'Old release/candidate ownership is ambiguous.' }
        AssertDev $C.Sha; RemoveRelease $owned.Id
    }
    if ($oldParked) {
        AssertDev $C.Sha; if ((ReleaseAnyTag 'dev').Found) { throw 'dev release is occupied; parked old release was preserved.' }
        $body = [ordered]@{ tag_name = 'dev'; target_commitish = $C.OldTarget; name = $C.OldName;
            body = $C.OldBody; draft = [bool]$C.OldDraft; prerelease = $C.OldPrerelease }
        AssertRemotePublicationMutationLock
        $result = Mutate "repos/$script:Repo/releases/$($C.OldId)" 'PATCH' (Payload 'restore.json' $body) 'Restore old dev release'
        $restored = ReleaseId $C.OldId
        if (-not $restored.Found) { throw "Old release disappeared during restore: $($result.Error)" }
        AssertState $restored.Value $C.OldId 'dev' $C.OldTarget ([bool]$C.OldDraft)
        if ((Fingerprint $restored.Value ([bool]$C.OldDraft)) -ne $C.OldFingerprint) { throw 'Restored old release fingerprint differs.' }
        $byTag = ReleaseAnyTag 'dev'; if (-not $byTag.Found -or [long](Prop $byTag.Value 'id') -ne $C.OldId) { throw 'Old release was not restored under dev.' }
        AssertDev $C.Sha
    }
    elseif ($oldOriginal) {
        $byTag = ReleaseAnyTag 'dev'
        if (-not $byTag.Found -or [long](Prop $byTag.Value 'id') -ne $C.OldId -or
            (Fingerprint $byTag.Value ([bool]$C.OldDraft)) -ne $C.OldFingerprint) {
            throw 'Prior dev release changed during candidate rollback.'
        }
    }
}

$Context = $null
$CutoverComplete = $false
$RemoteLock = $null
$RemoteLockOwned = $false
$PendingFailure = $null
$PublisherMutex = $null
$PublisherMutexHeld = $false
try {
    $PublisherMutex = New-Object System.Threading.Mutex($false, 'Global\codex-switch-global-pace-publish-dev-v1')
    try {
        $PublisherMutexHeld = $PublisherMutex.WaitOne(0, $false)
    }
    catch [System.Threading.AbandonedMutexException] {
        # The kernel transferred ownership from a terminated publisher. Remote
        # journal recovery below decides whether any mutation is safe.
        $PublisherMutexHeld = $true
    }
    if (-not $PublisherMutexHeld) {
        throw 'Another publish-dev transaction is already running on this computer.'
    }

    $user = Json 'user' 'Authenticate local gh user'
    $repoInfo = Json "repos/$Repo" 'Inspect repository permission'
    if (-not [bool]$repoInfo.permissions.push) { throw "gh user '$($user.login)' lacks push permission to $Repo." }
    $immutable = Maybe "repos/$Repo/immutable-releases" 'Check immutable releases'
    if ($immutable.Found -and [bool]$immutable.Value.enabled) { throw 'Immutable releases are enabled; transactional replacement is impossible.' }

    $sha = ResolveDev
    $runs = @(Pages "repos/$Repo/actions/workflows/release.yml/runs?status=success" 'workflow_runs' |
        Where-Object { $_.name -eq 'Release' -and $_.path -eq '.github/workflows/release.yml' -and
            $_.event -eq 'push' -and $_.head_branch -eq 'dev' -and $_.status -eq 'completed' -and
            $_.conclusion -eq 'success' -and (SameSha $_.head_sha $sha) -and
            $_.repository.full_name -eq $Repo -and $_.head_repository.full_name -eq $Repo })
    if ($RunId) { $runs = @($runs | Where-Object { [long]$_.id -eq $RunId }) }
    if ($runs.Count -ne 1) {
        $hint = if ($RunId) { "Run $RunId is not the one exact successful dev run." } else { 'Pass -RunId only when more than one exact run exists.' }
        throw "Expected exactly one successful Release run for refs/tags/dev at $sha; found $($runs.Count). $hint"
    }
    $run = $runs[0]; $artifactName = "dev-release-$sha"
    $artifacts = @(Pages "repos/$Repo/actions/runs/$($run.id)/artifacts?x=1" 'artifacts' |
        Where-Object { $_.name -eq $artifactName })
    if ($artifacts.Count -ne 1 -or [bool]$artifacts[0].expired -or [long]$artifacts[0].size_in_bytes -le 0) {
        throw "Run $($run.id) must have one unexpired non-empty artifact named $artifactName."
    }
    $bundle = Join-Path $Temp 'bundle'; [void][IO.Directory]::CreateDirectory($bundle)
    RunGh @('run', 'download', [string]$run.id, '--repo', $Repo, '--name', $artifactName, '--dir', $bundle) 'Download dev release artifact' | Out-Null
    ExactFiles $bundle @($Assets + 'release_body.md')

    $versionText = $Utf8.GetString((RepoBytes 'VERSION' $sha))
    if ($versionText -notmatch '\A([1-9][0-9]*\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*))\r?\n?\z') { throw 'VERSION at dev is not canonical numeric SemVer.' }
    $baseVersion = $Matches[1]; $version = "$baseVersion-dev"
    $cargo = $Utf8.GetString((RepoBytes 'Cargo.toml' $sha))
    if ($cargo -notmatch '(?ms)\A\[package\].*?^name\s*=\s*"codex-switch-global-pace"\s*$.*?^version\s*=\s*"([^"]+)"\s*$' -or $Matches[1] -ne $baseVersion) {
        throw 'Cargo.toml package version does not match VERSION at dev.'
    }
    $installGuide = "https://github.com/$Repo/blob/$sha/docs/wiki/Getting-Started.md#install"
    $expectedBody = "Development build from ``refs/tags/dev`` at $sha.`n`nUse ``codex-switch-global-pace self-update --dev`` to install, or ``codex-switch-global-pace self-update --stable`` to return to stable.`n`nStart with the [source-controlled verified install guide]($installGuide). It verifies the installer before execution with no fallback.`n`nThe attached ``INSTALL.md`` is release-exact and attested in the same provenance bundle. The packaged Windows installer selects dev automatically.`n`nVersion: ``$version```n"
    if (-not (Same ([IO.File]::ReadAllText((Join-Path $bundle 'release_body.md'))) $expectedBody)) { throw 'release_body.md is not exact.' }
    foreach ($spec in @(
        @('scripts/install.sh', 'install.sh', 'PACKAGED_RELEASE_VERSION=""', "PACKAGED_RELEASE_VERSION=`"$version`""),
        @('scripts/install.ps1', 'install.ps1', '$PackagedReleaseVersion = ""', "`$PackagedReleaseVersion = `"$version`"")
    )) {
        $source = $Utf8.GetString((RepoBytes $spec[0] $sha)); if ([regex]::Matches($source, [regex]::Escape($spec[2])).Count -ne 1) { throw "$($spec[0]) version placeholder is not unique." }
        if (-not (Same ([IO.File]::ReadAllText((Join-Path $bundle $spec[1]))) ($source.Replace($spec[2], $spec[3])))) { throw "$($spec[1]) is not the exact packaged source installer." }
    }
    foreach ($a in $Archives) {
        $text = [IO.File]::ReadAllText((Join-Path $bundle "$a.sha256")); $m = [regex]::Match($text, '\A([0-9A-Fa-f]{64})[ \t]+\*?([^\r\n \t]+)\r?\n?\z')
        if (-not $m.Success -or $m.Groups[2].Value -ne $a -or -not (SameSha $m.Groups[1].Value (Hash (Join-Path $bundle $a)))) { throw "Invalid checksum for $a." }
    }
    foreach ($a in @($Archives + @('INSTALL.md', 'install.sh', 'install.ps1'))) {
        $verify = RunGh @('attestation', 'verify', (Join-Path $bundle $a), '--repo', $Repo,
            '--bundle', (Join-Path $bundle 'codex-switch-global-pace-build-provenance.json'),
            '--signer-workflow', "$Repo/.github/workflows/release.yml", '--source-digest', $sha,
            '--source-ref', 'refs/tags/dev', '--deny-self-hosted-runners', '--format', 'json') "Verify $a attestation"
        $ok = $false
        $archiveHash = Hash (Join-Path $bundle $a)
        foreach ($v in @(($verify.Out | ConvertFrom-Json))) {
            $w = $v.verificationResult.statement.predicate.buildDefinition.externalParameters.workflow
            $dep = @($v.verificationResult.statement.predicate.buildDefinition.resolvedDependencies | Where-Object { SameSha $_.digest.gitCommit $sha })
            $subject = @($v.verificationResult.statement.subject | Where-Object { $_.name -eq $a -and (SameSha $_.digest.sha256 $archiveHash) })
            if ($w.repository -eq "https://github.com/$Repo" -and $w.path -eq '.github/workflows/release.yml' -and $w.ref -eq 'refs/tags/dev' -and $dep.Count -and $subject.Count) { $ok = $true }
        }
        if (-not $ok) { throw "$a attestation predicate is not exact." }
    }
    if ($env:OS -ne 'Windows_NT') { throw 'Packaged executable validation requires Windows.' }
    $hostArch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64' -or $env:PROCESSOR_ARCHITEW6432 -eq 'ARM64') {
        'arm64'
    }
    elseif ($env:PROCESSOR_ARCHITECTURE -eq 'AMD64' -or $env:PROCESSOR_ARCHITEW6432 -eq 'AMD64') {
        'amd64'
    }
    else {
        throw "Unsupported Windows host architecture '$($env:PROCESSOR_ARCHITECTURE)'."
    }
    $hostZip = Join-Path $bundle "codex-switch-global-pace-windows-$hostArch.zip"
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [IO.Compression.ZipFile]::OpenRead($hostZip)
    try {
        $entries = @($zip.Entries)
        if ($entries.Count -ne 1 -or $entries[0].FullName.Replace('\', '/') -ne 'codex-switch-global-pace.exe') {
            throw "The host Windows archive must contain only root codex-switch-global-pace.exe."
        }
    }
    finally { $zip.Dispose() }
    $hostDir = Join-Path $Temp 'host-binary'; [IO.Compression.ZipFile]::ExtractToDirectory($hostZip, $hostDir)
    $versionLines = @(& (Join-Path $hostDir 'codex-switch-global-pace.exe') --version 2>&1 | ForEach-Object { [string]$_ })
    if ($LASTEXITCODE -ne 0 -or $versionLines.Count -eq 0 -or $versionLines[0].TrimEnd("`r") -ne "codex-switch-global-pace $version") {
        throw 'The packaged host executable version is not exact.'
    }
    AssertDev $sha

    $local = LocalMap $bundle
    $name = "dev ($version)"
    $RemoteLock = AcquireRemotePublicationLock $sha
    $RemoteLockOwned = $true
    $journal = DiscoverJournal
    if ($null -ne $journal) {
        RecoverJournal $journal $sha $name $expectedBody $local
    }

    $current = ReleaseAnyTag 'dev'
    if ($current.Found -and
        (SameSha (Prop $current.Value 'target_commitish') $sha) -and
        -not [bool](Prop $current.Value 'draft') -and
        [bool](Prop $current.Value 'prerelease') -and
        -not [bool](Prop $current.Value 'immutable') -and
        (Same (Prop $current.Value 'name') $name) -and
        (Same (Prop $current.Value 'body') $expectedBody)) {
        AssertCurrentPublicExact $current.Value $sha $name $expectedBody $local
        Write-Host "Development release $version from run $($run.id) is already exact at $sha."
        return
    }

    $oldByTag = ReleaseAnyTag 'dev'; if (-not $oldByTag.Found) { throw 'A dev release is required for replacement.' }
    $old = $oldByTag.Value; $oldId = [long](Prop $old 'id')
    $oldDraft = [bool](Prop $old 'draft')
    $oldTarget = PriorTarget $old
    $oldFingerprint = Fingerprint $old $oldDraft
    $tx = [string]$RemoteLock.Transaction
    $oldVisibility = if ($oldDraft) { 'draft' } else { 'public' }
    $candidateTag = "dev-candidate-$oldId-$oldVisibility-$oldFingerprint-$tx"
    $Context = @{ Sha = $sha; CandidateTag = $candidateTag; CandidateId = 0L;
        CandidateProjection = ''; CandidateExact = $false; CandidateCreateAmbiguous = $false;
        CandidateCreated = $false; ParkTag = ''; Local = $local; Name = $name;
        Body = $expectedBody; OldId = $oldId; OldTarget = $oldTarget; OldName = (Prop $old 'name');
        OldBody = (Prop $old 'body'); OldPrerelease = [bool](Prop $old 'prerelease');
        OldDraft = $oldDraft; OldFingerprint = $oldFingerprint }
    $collisions = @(AllReleases | Where-Object { (Prop $_ 'tag_name') -eq $candidateTag })
    if ($collisions.Count -ne 0) { throw 'Candidate journal tag collision.' }
    AssertNoRef $candidateTag

    AssertDev $sha
    $oldBeforeCreate = ReleaseId $oldId
    if (-not $oldBeforeCreate.Found) { throw 'Prior dev release disappeared before candidate creation.' }
    AssertState $oldBeforeCreate.Value $oldId 'dev' $oldTarget $oldDraft
    if ((Fingerprint $oldBeforeCreate.Value $oldDraft) -ne $oldFingerprint) {
        throw 'Prior dev release drifted before candidate creation.'
    }
    $createBody = [ordered]@{ tag_name = $candidateTag; target_commitish = $sha; name = $name; body = $expectedBody; draft = $true; prerelease = $true }
    AssertRemotePublicationMutationLock
    $Context.CandidateCreateAmbiguous = $true
    $created = Mutate "repos/$Repo/releases" 'POST' (Payload 'create.json' $createBody) 'Create candidate draft'
    AcceptCreatedCandidate $Context $created | Out-Null
    AssertNoRef $candidateTag
    $parkTag = "dev-park-$oldId-$($Context.CandidateId)-$oldVisibility-$oldFingerprint-$tx"
    $parkCollisions = @(AllReleases | Where-Object { (Prop $_ 'tag_name') -eq $parkTag })
    if ($parkCollisions.Count -ne 0) { throw 'Park journal tag collision.' }
    AssertNoRef $parkTag
    $Context.ParkTag = $parkTag
    AssertDev $sha
    $uploadArgs = @('release', 'upload', $candidateTag, '--repo', $Repo) + @($Assets | ForEach-Object { Join-Path $bundle $_ })
    AssertRemotePublicationMutationLock
    RunGh $uploadArgs 'Upload candidate assets' | Out-Null
    $candidate = (ReleaseId $Context.CandidateId).Value
    $Context.CandidateProjection = AssertCandidateAssets $Context.CandidateId $local -Exact
    $Context.CandidateExact = $true
    $remote = Join-Path $Temp 'remote'; DownloadExact $candidateTag $remote $local
    AssertCandidate $Context $candidate -ExactAssets | Out-Null

    AssertDev $sha
    $oldBeforePark = ReleaseId $oldId
    if (-not $oldBeforePark.Found) { throw 'Prior dev release disappeared before parking.' }
    AssertState $oldBeforePark.Value $oldId 'dev' $oldTarget $oldDraft
    if ((Fingerprint $oldBeforePark.Value $oldDraft) -ne $Context.OldFingerprint) { throw 'Old release drifted before parking.' }
    AssertCandidate $Context (ReleaseId $Context.CandidateId).Value -ExactAssets | Out-Null
    AssertDev $sha
    AssertRemotePublicationMutationLock
    $park = Mutate "repos/$Repo/releases/$oldId" 'PATCH' (Payload 'park.json' ([ordered]@{ tag_name = $parkTag; draft = $true })) 'Park old dev release'
    $parked = (ReleaseId $oldId).Value; AssertState $parked $oldId $parkTag $oldTarget $true
    if ((Fingerprint $parked $oldDraft) -ne $Context.OldFingerprint) { throw "Parking changed old release: $($park.Error)" }
    AssertNoRef $parkTag; if ((ReleaseAnyTag 'dev').Found) { throw 'dev release remained occupied after parking.' }

    AssertDev $sha; AssertCandidate $Context (ReleaseId $Context.CandidateId).Value -ExactAssets | Out-Null
    if ((Fingerprint (ReleaseId $oldId).Value $oldDraft) -ne $Context.OldFingerprint) { throw 'Parked old release drifted.' }
    AssertDev $sha
    $finalizeBody = [ordered]@{ tag_name = 'dev'; target_commitish = $sha; name = $name;
        body = $expectedBody; draft = $false; prerelease = $true }
    AssertRemotePublicationMutationLock
    $cutover = Mutate "repos/$Repo/releases/$($Context.CandidateId)" 'PATCH' `
        (Payload 'finalize.json' $finalizeBody) 'Finalize candidate'
    $finalized = (ReleaseId $Context.CandidateId).Value
    AssertState $finalized $Context.CandidateId 'dev' $sha $false
    if (-not (Same $finalized.name $name) -or -not (Same $finalized.body $expectedBody) -or
        -not [bool]$finalized.prerelease) { throw "Finalized metadata differs: $($cutover.Error)" }
    AssertCandidate $Context $finalized -ExactAssets | Out-Null; AssertDev $sha; AssertNoRef $candidateTag
    $tagRelease = ReleaseAnyTag 'dev'; if (-not $tagRelease.Found -or [long](Prop $tagRelease.Value 'id') -ne $Context.CandidateId) { throw 'Final dev release ID is not the candidate.' }

    AssertDev $sha; AssertCandidate $Context (ReleaseId $Context.CandidateId).Value -ExactAssets | Out-Null
    $parked = (ReleaseId $oldId).Value; AssertState $parked $oldId $parkTag $oldTarget $true
    if ((Fingerprint $parked $oldDraft) -ne $Context.OldFingerprint) { throw 'Parked old release drifted before cleanup.' }
    AssertNoRef $parkTag; AssertDev $sha; RemoveRelease $oldId; AssertDev $sha
    $final = ReleaseAnyTag 'dev'
    if (-not $final.Found -or [long](Prop $final.Value 'id') -ne $Context.CandidateId) {
        throw 'Final dev release ID is not the replacement candidate.'
    }
    AssertCandidate $Context $final.Value -ExactAssets | Out-Null
    AssertDev $sha
    $CutoverComplete = $true
    Write-Host "Replaced dev $version from run $($run.id) at $sha as a public prerelease; prior $oldVisibility visibility was journaled for rollback."
}
catch {
    $primary = $_.Exception.Message
    if ($CutoverComplete) {
        $PendingFailure = "The replacement dev release is verified and was preserved; cleanup failed: $primary"
    }
    elseif ($Context) {
        try {
            Rollback $Context
            $PendingFailure = "Replacement failed and the exact prior release was restored: $primary"
        }
        catch {
            $PendingFailure = "Replacement failed: $primary`nRollback was not safe; candidate '$($Context.CandidateTag)' and old release $($Context.OldId) were preserved where present: $($_.Exception.Message)"
        }
    }
    else {
        $PendingFailure = $primary
    }
    throw $PendingFailure
}
finally {
    $LockCleanupFailure = $null
    if ($RemoteLockOwned) {
        try {
            ReleaseRemotePublicationLock $RemoteLock
            $RemoteLockOwned = $false
        }
        catch {
            $LockCleanupFailure = $_.Exception.Message
        }
    }
    try {
        $full = [IO.Path]::GetFullPath($Temp).TrimEnd('\', '/')
        if (-not [string]::Equals([IO.Path]::GetDirectoryName($full), $TempBase, [StringComparison]::OrdinalIgnoreCase) -or
            -not [IO.Path]::GetFileName($full).StartsWith('codex-switch-publish-dev-', [StringComparison]::Ordinal)) {
            throw "Refusing unsafe temporary cleanup: $full"
        }
        if ([IO.Directory]::Exists($full)) { Remove-Item -LiteralPath $full -Recurse -Force }
    }
    catch {
        SafeWarning "Temporary publisher files were preserved: $($_.Exception.Message)"
    }
    if ($PublisherMutexHeld) {
        try {
            $PublisherMutex.ReleaseMutex()
            $PublisherMutexHeld = $false
        }
        catch {
            SafeWarning "The local publisher mutex could not be released cleanly: $($_.Exception.Message)"
        }
    }
    if ($null -ne $PublisherMutex) {
        try { $PublisherMutex.Dispose() }
        catch { SafeWarning "The local publisher mutex handle could not be disposed: $($_.Exception.Message)" }
    }
    if ($LockCleanupFailure) {
        if ($PendingFailure) {
            throw "$PendingFailure`nThe exact remote publication lock could not be released: $LockCleanupFailure"
        }
        throw "The publication transaction finished, but its exact remote lock could not be released: $LockCleanupFailure"
    }
}
