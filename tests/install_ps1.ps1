# install.ps1 against crafted releases served over local HTTP: a good release
# installs (under PowerShell 7 and Windows PowerShell 5.1), also while another
# process holds the unpacked binary open, and a checksum mismatch, an archive
# entry escaping the install dir, and an existing install are each refused
# with nothing written. Outside GitHub Actions the user PATH is never touched
# (AI_MEMORY_NO_MODIFY_PATH=1); on a runner, one case lets install.ps1 add to
# it and restores the value it found. Needs python on PATH for the HTTP server.
#
# Usage: pwsh -NoProfile -File tests/install_ps1.ps1 -Binary target\debug\ai-memory.exe
# An isolated Windows Sandbox can use -Offline -Shells powershell with a
# synthetic console executable. -InstallerPath runs the same assertions against
# a selected installer; never run a known-defective installer on the host.

param(
    [Parameter(Mandatory = $true)][string] $Binary,
    [string] $InstallerPath,
    [string[]] $Shells = @('pwsh', 'powershell'),
    [switch] $Offline,
    [string] $TestRoot,
    [string] $ResultPath
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$script = if ($InstallerPath) { (Resolve-Path -LiteralPath $InstallerPath).Path } else { Join-Path $root 'install.ps1' }
$asset = 'ai-memory-windows-x86_64.zip'
$testParent = if ($TestRoot) { (Resolve-Path -LiteralPath $TestRoot).Path } else { [IO.Path]::GetTempPath() }
$tmp = Join-Path $testParent ("ai-memory-install-ps1-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp | Out-Null
$testTmp, $testTemp, $testSystemTemp = $env:TMP, $env:TEMP, $env:SystemTemp
$env:TMP = $tmp; $env:TEMP = $tmp
$systemProcess = [Security.Principal.WindowsIdentity]::GetCurrent().IsSystem
if ($systemProcess) {
    # GetTempPath2 ignores TMP/TEMP for SYSTEM. Its override must stay private:
    # https://learn.microsoft.com/windows/win32/api/fileapi/nf-fileapi-gettemppath2w
    $temporaryAcl = New-Object Security.AccessControl.DirectorySecurity
    $temporaryAcl.SetAccessRuleProtection($true, $false)
    foreach ($sid in @('S-1-5-18', 'S-1-5-32-544')) {
        $temporaryAcl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
            [Security.Principal.SecurityIdentifier]::new($sid),
            [Security.AccessControl.FileSystemRights]::FullControl,
            [Security.AccessControl.InheritanceFlags]'ContainerInherit,ObjectInherit',
            [Security.AccessControl.PropagationFlags]::None,
            [Security.AccessControl.AccessControlType]::Allow))
    }
    (Get-Item -LiteralPath $tmp).SetAccessControl($temporaryAcl)
    $env:SystemTemp = $tmp
}
$cases = New-Object 'System.Collections.Generic.List[object]'
$failure = $null
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

function Fail([string] $message) { throw "install_ps1: $message" }
function Check([string] $name, [bool] $passed) {
    $cases.Add([pscustomobject]@{ name = $name; passed = $passed })
    if (-not $passed) { Fail $name }
}

# Only the harness injects faults. The shipped installer has no test switches.
# Offline mode still validates a real ZIP, hash, and executable; only the
# transport copies local fixture files rather than talking to the HTTP server.
$wrapper = Join-Path $tmp 'invoke-installer.ps1'
@'
param([string] $Installer, [string] $FixtureRoot, [string] $Fault, [string] $Requests, [string] $LinkTarget)
$ErrorActionPreference = 'Stop'
$script:copyFaultApplied = $false
function Invoke-WebRequest {
    [CmdletBinding()]
    param([switch] $UseBasicParsing, [string] $Uri, [string] $OutFile)
    [IO.File]::AppendAllText($Requests, "download`n")
    if ($FixtureRoot) {
        $relative = ([Uri]$Uri).AbsolutePath.TrimStart('/').Replace('/', [IO.Path]::DirectorySeparatorChar)
        $source = [IO.Path]::GetFullPath((Join-Path $FixtureRoot $relative))
        if (-not $source.StartsWith($FixtureRoot.TrimEnd('\') + '\', [StringComparison]::OrdinalIgnoreCase)) { throw 'fixture path escaped' }
        Microsoft.PowerShell.Management\Copy-Item -LiteralPath $source -Destination $OutFile
    } else {
        Microsoft.PowerShell.Utility\Invoke-WebRequest @PSBoundParameters
    }
}
function Copy-Item {
    [CmdletBinding()]
    param([string] $LiteralPath, [string] $Destination, [switch] $Recurse)
    if ($Fault -and -not $script:copyFaultApplied) {
        $script:copyFaultApplied = $true
        if ($Fault -eq 'race-file') {
            [IO.File]::WriteAllText($env:AI_MEMORY_INSTALL_DIR, 'concurrent-file')
        } elseif ($Fault -eq 'race-junction') {
            New-Item -ItemType Junction -Path $env:AI_MEMORY_INSTALL_DIR -Target $LinkTarget | Out-Null
        } elseif ($Fault -eq 'race-directory' -or $Fault -eq 'copy-fail-race') {
            New-Item -ItemType Directory -Path $env:AI_MEMORY_INSTALL_DIR | Out-Null
            [IO.File]::WriteAllText((Join-Path $env:AI_MEMORY_INSTALL_DIR 'sentinel.txt'), 'concurrent-directory')
        }
        Microsoft.PowerShell.Management\Copy-Item @PSBoundParameters
        if ($Fault -like 'copy-fail*') { throw 'injected copy failure after a partial copy' }
    } else {
        Microsoft.PowerShell.Management\Copy-Item @PSBoundParameters
    }
}
& $Installer
'@ | Set-Content -LiteralPath $wrapper -Encoding UTF8

# publish <name> [-Evil] [-BadSum]: a release dir serving latest/download/<asset>.
function Publish([string] $name, [switch] $Evil, [switch] $BadSum) {
    $dir = Join-Path $tmp "$name\latest\download"
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    $zip = Join-Path $dir $asset
    $archive = [IO.Compression.ZipFile]::Open($zip, 'Create')
    try {
        [IO.Compression.ZipFileExtensions]::CreateEntryFromFile($archive, (Resolve-Path $Binary), 'ai-memory.exe') | Out-Null
        $hook = $archive.CreateEntry('hooks/_lib.sh')
        $writer = New-Object IO.StreamWriter($hook.Open())
        $writer.Write('echo hook'); $writer.Dispose()
        if ($Evil) {
            $escape = $archive.CreateEntry('../escape.txt')
            $writer = New-Object IO.StreamWriter($escape.Open())
            $writer.Write('outside'); $writer.Dispose()
        }
    } finally {
        $archive.Dispose()
    }
    $hash = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($BadSum) { $hash = '0' * 64 }
    "$hash  $asset`n" | Out-File -Encoding ascii -NoNewline "$zip.sha256"
    return (Join-Path $tmp $name)
}

$port = Get-Random -Minimum 20000 -Maximum 60000
$server = $null
if (-not $Offline) {
    $server = Start-Process python -ArgumentList @('-m', 'http.server', "$port", '--bind', '127.0.0.1', '--directory', $tmp) -PassThru -WindowStyle Hidden
}
try {
    Check 'temporary directory confined to fixture' ([IO.Path]::GetTempPath().TrimEnd('\') -eq $tmp.TrimEnd('\'))
    $deadline = (Get-Date).AddSeconds(20)
    while (-not $Offline) {
        try { (New-Object Net.Sockets.TcpClient('127.0.0.1', $port)).Dispose(); break } catch { }
        if ((Get-Date) -gt $deadline) { Fail 'the local HTTP server did not start' }
        Start-Sleep -Milliseconds 200
    }

    # install <shell> <release> <dest> [-ModifyPath]: run install.ps1 with the
    # test env, leaving the user PATH alone unless -ModifyPath; returns the exit code.
    function Install([string] $shell, [string] $release, [string] $dest, [string] $log, [switch] $ModifyPath, [string] $Fault, [string] $LinkTarget) {
        $env:AI_MEMORY_RELEASE_BASE_URL = "http://127.0.0.1:$port/$release"
        $env:AI_MEMORY_INSTALL_DIR = $dest
        if (-not $ModifyPath) { $env:AI_MEMORY_NO_MODIFY_PATH = '1' }
        try {
            $arguments = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $wrapper, '-Installer', $script, '-Requests', "$log.requests")
            if ($Offline) { $arguments += @('-FixtureRoot', $tmp) }
            if ($Fault) { $arguments += @('-Fault', $Fault) }
            if ($LinkTarget) { $arguments += @('-LinkTarget', $LinkTarget) }
            # Windows PowerShell turns redirected native stderr into error
            # records. A refused install is an expected exit status here.
            $savedPreference = $ErrorActionPreference
            try {
                $ErrorActionPreference = 'Continue'
                & $shell @arguments *> $log
                $installExit = $LASTEXITCODE
            } finally { $ErrorActionPreference = $savedPreference }
            return $installExit
        } finally {
            Remove-Item Env:AI_MEMORY_RELEASE_BASE_URL, Env:AI_MEMORY_INSTALL_DIR, Env:AI_MEMORY_NO_MODIFY_PATH -ErrorAction SilentlyContinue
        }
    }

    Publish good | Out-Null
    foreach ($shell in $Shells) {
        $dest = Join-Path $tmp "inst-$shell\ai-memory"
        $log = Join-Path $tmp "good-$shell.log"
        if ((Install $shell 'good' $dest $log) -ne 0) { Fail "$shell refused a good release: $(Get-Content $log -Raw)" }
        if (-not (Test-Path (Join-Path $dest 'ai-memory.exe'))) { Fail "$shell did not unpack ai-memory.exe" }
        if (-not (Test-Path (Join-Path $dest 'hooks\_lib.sh'))) { Fail "$shell did not unpack hooks" }
        if ((Get-Content $log -Raw) -notmatch 'next: ') { Fail "$shell printed no next step: $(Get-Content $log -Raw)" }
        Check "${shell}: fresh install" $true

        # A second run finds the install and points to `ai-memory upgrade`.
        $again = Join-Path $tmp "again-$shell.log"
        if ((Install $shell 'good' $dest $again) -eq 0) { Fail "$shell reinstalled over an existing install" }
        if ((Get-Content $again -Raw) -notmatch 'ai-memory upgrade') { Fail "$shell did not point to upgrade: $(Get-Content $again -Raw)" }
        Check "${shell}: existing executable preserved" $true

        # Refuse every existing entry before making the first download request.
        foreach ($kind in @('directory-with-data', 'empty-directory', 'file', 'junction', 'dangling-junction')) {
            $caseRoot = Join-Path $tmp "existing-$shell-$kind"
            New-Item -ItemType Directory -Path $caseRoot | Out-Null
            $dest = Join-Path $caseRoot 'destination'
            $target = Join-Path $caseRoot 'link-target'
            $sentinel = Join-Path $dest 'sentinel.txt'
            if ($kind -eq 'file') {
                [IO.File]::WriteAllText($dest, 'existing-file')
            } elseif ($kind -like '*junction') {
                New-Item -ItemType Directory -Path $target | Out-Null
                New-Item -ItemType Junction -Path $dest -Target $target | Out-Null
                if ($kind -eq 'dangling-junction') {
                    # Only the empty directory this fixture just created.
                    [IO.Directory]::Delete($target)
                } else {
                    [IO.File]::WriteAllText((Join-Path $target 'sentinel.txt'), 'existing-data')
                }
            } else {
                New-Item -ItemType Directory -Path $dest | Out-Null
                if ($kind -eq 'directory-with-data') { [IO.File]::WriteAllText($sentinel, 'existing-data') }
            }
            $log = Join-Path $tmp "existing-$shell-$kind.log"
            $exitCode = Install $shell 'good' $dest $log
            if ($kind -eq 'directory-with-data') {
                # This runs first so the old installer demonstrates loss of
                # fixture data, not merely a difference in its error text.
                Check "${shell}: existing directory sentinel preserved" ((Test-Path -LiteralPath $sentinel) -and [IO.File]::ReadAllText($sentinel) -eq 'existing-data')
            }
            Check "${shell}: $kind refused before download" ($exitCode -ne 0 -and -not (Test-Path -LiteralPath "$log.requests"))
            Check "${shell}: $kind refusal explained" ((Get-Content -LiteralPath $log -Raw) -match 'destination already exists')
            if ($kind -eq 'file') {
                Check "${shell}: existing file preserved" ([IO.File]::ReadAllText($dest) -eq 'existing-file')
            } elseif ($kind -eq 'junction') {
                Check "${shell}: $kind sentinel preserved" ([IO.File]::ReadAllText($sentinel) -eq 'existing-data')
            } elseif ($kind -eq 'empty-directory') {
                Check "${shell}: empty directory preserved" ((Test-Path -LiteralPath $dest -PathType Container) -and @(Get-ChildItem -LiteralPath $dest -Force).Count -eq 0)
            } elseif ($kind -eq 'dangling-junction') {
                Check "${shell}: dangling junction preserved" ([bool]((Get-Item -LiteralPath $dest -Force).Attributes -band [IO.FileAttributes]::ReparsePoint))
            }
        }

        foreach ($fault in @('race-directory', 'race-file', 'race-junction', 'copy-fail', 'copy-fail-race')) {
            $caseRoot = Join-Path $tmp "fault-$shell-$fault"
            New-Item -ItemType Directory -Path $caseRoot | Out-Null
            $dest = Join-Path $caseRoot 'destination'
            $target = Join-Path $caseRoot 'link-target'
            New-Item -ItemType Directory -Path $target | Out-Null
            [IO.File]::WriteAllText((Join-Path $target 'sentinel.txt'), 'link-target-data')
            $log = Join-Path $tmp "fault-$shell-$fault.log"
            $exitCode = Install $shell 'good' $dest $log -Fault $fault -LinkTarget $target
            Check "${shell}: $fault refuses publication" ($exitCode -ne 0)
            Check "${shell}: $fault reports retained staging" ((Get-Content -LiteralPath $log -Raw) -match 'installation was not published; inspect leftover staging')
            $staging = @(Get-ChildItem -LiteralPath $caseRoot -Force -Directory | Where-Object { $_.Name -like '.ai-memory-install-*' })
            Check "${shell}: $fault keeps its uncommitted staging" ($staging.Count -eq 1)
            if ($fault -eq 'race-file') {
                Check "${shell}: concurrent file preserved" ([IO.File]::ReadAllText($dest) -eq 'concurrent-file')
            } elseif ($fault -eq 'race-junction') {
                Check "${shell}: concurrent junction preserved" ([bool]((Get-Item -LiteralPath $dest -Force).Attributes -band [IO.FileAttributes]::ReparsePoint))
                Check "${shell}: concurrent junction target preserved" ([IO.File]::ReadAllText((Join-Path $target 'sentinel.txt')) -eq 'link-target-data')
            } elseif ($fault -eq 'copy-fail') {
                Check "${shell}: partial copy does not appear installed" (-not (Test-Path -LiteralPath $dest))
            } else {
                Check "${shell}: concurrent directory sentinel preserved" ([IO.File]::ReadAllText((Join-Path $dest 'sentinel.txt')) -eq 'concurrent-directory')
                Check "${shell}: concurrent directory received no release files" (@(Get-ChildItem -LiteralPath $dest -Force).Count -eq 1)
            }
        }
    }

    # An antivirus scan of the unpacked binary holds it open, sharing read and
    # write but not delete access, so Windows refuses to rename or delete it
    # (#1480). The install must still go through. TMP points at a directory of
    # its own, so the watcher sees only the staged release.
    Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.IO;
using System.Threading;

public static class StagedBinaryScan
{
    static readonly List<FileStream> held = new List<FileStream>();
    static FileSystemWatcher watcher;
    static volatile bool stopped;

    public static void Start(string root)
    {
        watcher = new FileSystemWatcher(root, "ai-memory.exe");
        watcher.IncludeSubdirectories = true;
        watcher.Created += (sender, e) => Hold(e.FullPath);
        watcher.EnableRaisingEvents = true;
    }

    // Retried: the unpacker may still be writing the file without sharing it.
    static void Hold(string path)
    {
        DateTime deadline = DateTime.UtcNow.AddSeconds(60);
        while (!stopped && DateTime.UtcNow < deadline)
        {
            try
            {
                FileStream stream = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite);
                lock (held)
                {
                    if (stopped) { stream.Dispose(); } else { held.Add(stream); }
                }
                return;
            }
            catch (Exception)
            {
                Thread.Sleep(1);
            }
        }
    }

    public static int Count
    {
        get { lock (held) { return held.Count; } }
    }

    public static void Stop()
    {
        stopped = true;
        watcher.Dispose();
        lock (held)
        {
            foreach (FileStream stream in held) { stream.Dispose(); }
            held.Clear();
        }
    }
}
'@
    $scanTemp = Join-Path $tmp 'scan-temp'
    New-Item -ItemType Directory -Path $scanTemp | Out-Null
    $savedTmp, $savedTemp, $savedSystemTemp = $env:TMP, $env:TEMP, $env:SystemTemp
    [StagedBinaryScan]::Start($scanTemp)
    try {
        $env:TMP = $scanTemp
        $env:TEMP = $scanTemp
        if ($systemProcess) { $env:SystemTemp = $scanTemp }
        Check 'scan temporary directory confined to watcher' ([IO.Path]::GetTempPath().TrimEnd('\') -eq $scanTemp.TrimEnd('\'))
        foreach ($shell in $Shells) {
            $dest = Join-Path $tmp "inst-scan-$shell\ai-memory"
            $log = Join-Path $tmp "scan-$shell.log"
            if ((Install $shell 'good' $dest $log) -ne 0) { Fail "$shell failed while the unpacked binary was held open: $(Get-Content $log -Raw)" }
            if (-not (Test-Path (Join-Path $dest 'ai-memory.exe'))) { Fail "$shell did not install ai-memory.exe while it was held open" }
            if (-not (Test-Path (Join-Path $dest 'hooks\_lib.sh'))) { Fail "$shell did not install hooks while ai-memory.exe was held open" }
            Check "${shell}: installation succeeds while scan watcher runs" $true
        }
        Check 'staged binary was held open during each install' ([StagedBinaryScan]::Count -ge $Shells.Count)
    } finally {
        $env:TMP = $savedTmp
        $env:TEMP = $savedTemp
        $env:SystemTemp = $savedSystemTemp
        [StagedBinaryScan]::Stop()
    }

    # Adding the install dir must leave the rest of the user PATH as it was:
    # still REG_EXPAND_SZ, with %VAR% entries unexpanded (#1509). This rewrites
    # the real HKCU\Environment Path, so it runs only on a GitHub Actions
    # runner, and the value it found is put back.
    if ($env:GITHUB_ACTIONS -eq 'true') {
        $environment = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')
        $saved = $environment.GetValue('Path', $null, 'DoNotExpandEnvironmentNames')
        $savedKind = if ($null -ne $saved) { $environment.GetValueKind('Path') }
        $probe = '%USERPROFILE%\ai-memory-path-probe'
        try {
            $environment.SetValue('Path', (@($probe, $saved) | Where-Object { $_ }) -join ';', 'ExpandString')
            foreach ($shell in $Shells) {
                $dest = Join-Path $tmp "inst-path-$shell\ai-memory"
                $log = Join-Path $tmp "path-$shell.log"
                if ((Install $shell 'good' $dest $log -ModifyPath) -ne 0) { Fail "$shell failed to install with a PATH update: $(Get-Content $log -Raw)" }
                $kind = $environment.GetValueKind('Path')
                $entries = $environment.GetValue('Path', '', 'DoNotExpandEnvironmentNames') -split ';'
                if ($kind -ne 'ExpandString') { Fail "$shell stored the user PATH as $kind" }
                if ($entries -notcontains $probe) { Fail "$shell expanded $probe in the user PATH: $($entries -join ';')" }
                if ($entries -notcontains $dest) { Fail "$shell did not add $dest to the user PATH" }
            }
        } finally {
            if ($null -eq $saved) { $environment.DeleteValue('Path', $false) } else { $environment.SetValue('Path', $saved, $savedKind) }
            $environment.Dispose()
        }
    }

    Publish badsum -BadSum | Out-Null
    $dest = Join-Path $tmp 'inst-badsum\ai-memory'
    $log = Join-Path $tmp 'badsum.log'
    if ((Install $Shells[0] 'badsum' $dest $log) -eq 0) { Fail 'a checksum mismatch was installed' }
    if ((Get-Content $log -Raw) -notmatch 'checksum mismatch') { Fail "mismatch not explained: $(Get-Content $log -Raw)" }
    if (Test-Path $dest) { Fail 'a refused release was unpacked' }
    Check 'checksum mismatch refused' $true

    Publish evil -Evil | Out-Null
    $dest = Join-Path $tmp 'inst-evil\ai-memory'
    $log = Join-Path $tmp 'evil.log'
    if ((Install $Shells[0] 'evil' $dest $log) -eq 0) { Fail 'an archive with a .. entry was installed' }
    if ((Get-Content $log -Raw) -notmatch 'unsafe path') { Fail "unsafe entry not explained: $(Get-Content $log -Raw)" }
    if (Test-Path $dest) { Fail 'a refused release was unpacked' }
    if (Test-Path (Join-Path $tmp 'inst-evil\escape.txt')) { Fail 'the .. entry escaped' }
    Check 'archive traversal refused' $true

    Write-Host "install.ps1 checks passed ($($cases.Count) checks)"
} catch {
    $failure = $_.Exception.Message
    throw
} finally {
    if ($null -ne $server) { Stop-Process -Id $server.Id -Force -ErrorAction SilentlyContinue }
    if ($ResultPath) {
        [pscustomobject]@{ passed = ($null -eq $failure); failure = $failure; checks = @($cases.ToArray()); checkCount = $cases.Count; offline = [bool]$Offline; shells = $Shells } |
            ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $ResultPath -Encoding UTF8
    }
    $env:TMP = $testTmp; $env:TEMP = $testTemp
    $env:SystemTemp = $testSystemTemp
    $resolved = [IO.Path]::GetFullPath($tmp)
    if (-not $resolved.StartsWith([IO.Path]::GetFullPath($testParent).TrimEnd('\') + '\', [StringComparison]::OrdinalIgnoreCase) -or
        (Get-Item -LiteralPath $resolved -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'refusing cleanup outside the owned fixture' }
    Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
