# Install the ai-memory release binary for the current Windows user.
#
#   irm https://github.com/akitaonrails/ai-memory/releases/latest/download/install.ps1 | iex
#
# Downloads ai-memory-windows-x86_64.zip from a GitHub release, verifies it
# against the release's published .sha256 file, refuses an archive with an
# absolute or `..` entry, unpacks it into %LOCALAPPDATA%\Programs\ai-memory and
# adds that directory to your user PATH. No administrator rights. Existing
# destinations are refused before download; failed publication staging is
# retained beside the destination and its path is reported. A missing or mismatched
# checksum stops the install before anything is unpacked.
#
# Then run `ai-memory setup` to create the data dir, register the server to
# start at logon and wire your agents.
#
# Environment (all optional):
#   AI_MEMORY_VERSION           release to install, e.g. 3.1.0 (default: latest)
#   AI_MEMORY_INSTALL_DIR       where the release is unpacked
#                               (default: %LOCALAPPDATA%\Programs\ai-memory)
#   AI_MEMORY_RELEASE_BASE_URL  Releases-compatible URL prefix, the one
#                               `ai-memory upgrade` reads (default: GitHub).
#                               The checksum comes from the same place as the
#                               archive, so point it only at an origin you trust.
#   AI_MEMORY_NO_MODIFY_PATH    set to 1 to leave your user PATH alone
#
# Everything runs inside Install-AiMemory, called on the last line, so a
# download cut short runs nothing. Errors are thrown, never `exit`, so a failure
# under `iex` does not close your PowerShell window.

function Install-AiMemory {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'
    $defaultBase = 'https://github.com/akitaonrails/ai-memory/releases'
    $asset = 'ai-memory-windows-x86_64.zip'

    function Say([string] $message) { Write-Host "ai-memory install: $message" }
    function Fail([string] $message) { throw "ai-memory install: error: $message" }

    # Windows PowerShell 5.1 may still default to TLS 1.0; GitHub needs 1.2.
    if ($PSVersionTable.PSVersion.Major -lt 6) {
        [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    }

    $arch = $env:PROCESSOR_ARCHITECTURE
    if ($env:PROCESSOR_ARCHITEW6432) { $arch = $env:PROCESSOR_ARCHITEW6432 }
    switch ($arch) {
        'AMD64' { }
        'ARM64' { Say 'no native ARM64 build yet; installing the x86_64 build, which Windows 11 runs under emulation' }
        default { Fail "unsupported processor architecture $arch; ai-memory publishes an x86_64 Windows build" }
    }

    $base = if ($env:AI_MEMORY_RELEASE_BASE_URL) { $env:AI_MEMORY_RELEASE_BASE_URL.TrimEnd('/') } else { $defaultBase }
    # The archive and its `.sha256` come from the same origin, so a plain-http
    # GitHub base lets an on-path rewrite replace both and the checksum proves
    # nothing. The previous guard was nested inside a condition whose two arms
    # (the default base, or an https GitHub prefix) each already implied https,
    # so its inner scheme test could never fire — and a plain-http GitHub base
    # matched neither arm, so it skipped the check altogether. A non-GitHub
    # base stays the operator's trust decision, as the header documents.
    $baseUri = $null
    if ([Uri]::TryCreate($base, [UriKind]::Absolute, [ref]$baseUri) -and
        $baseUri.Scheme -eq 'http' -and
        $baseUri.Host.TrimEnd('.') -eq 'github.com') {
        Fail "refusing a non-https GitHub URL: $base"
    }
    $version = $env:AI_MEMORY_VERSION
    if ($version) {
        $version = $version.TrimStart('v')
        if ($version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.-]+)?$') { Fail "AI_MEMORY_VERSION must look like 3.1.0, not $version" }
        $url = "$base/download/v$version/$asset"
    } else {
        $url = "$base/latest/download/$asset"
    }

    $dest = if ($env:AI_MEMORY_INSTALL_DIR) { $env:AI_MEMORY_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\ai-memory' }
    if (-not [IO.Path]::IsPathRooted($dest)) { Fail "AI_MEMORY_INSTALL_DIR must be an absolute path, not $dest" }
    $dest = [IO.Path]::GetFullPath($dest)
    $exe = Join-Path $dest 'ai-memory.exe'
    # Any existing filesystem entry belongs to its caller, even an empty
    # directory. Check before downloading or executing a release, not just
    # before publishing it. Get-Item also sees dangling directory links.
    $existing = $null
    try { $existing = Get-Item -LiteralPath $dest -Force -ErrorAction Stop }
    catch [System.Management.Automation.ItemNotFoundException] { }
    if ($null -ne $existing) {
        Fail "destination already exists at $dest; remove it if it is empty or left over from a failed install, pick a new dedicated directory, or run ``ai-memory upgrade`` to update an existing installation"
    }

    $work = Join-Path ([IO.Path]::GetTempPath()) ("ai-memory-install-" + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $work | Out-Null
    try {
        $zip = Join-Path $work $asset
        Say "downloading $url"
        try {
            Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $zip
            Invoke-WebRequest -UseBasicParsing -Uri "$url.sha256" -OutFile "$zip.sha256"
        } catch {
            Fail "download failed: $($_.Exception.Message)"
        }

        $line = (Get-Content -LiteralPath "$zip.sha256" -Raw).Trim()
        if ($line -notmatch "^([0-9a-fA-F]{64})\s+\*?$([regex]::Escape($asset))$") {
            Fail "the checksum file does not name $asset; nothing was installed"
        }
        $expected = $Matches[1].ToLowerInvariant()
        $actual = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actual -ne $expected) {
            Fail "checksum mismatch for $asset (expected $expected, got $actual); nothing was installed"
        }

        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $archive = [IO.Compression.ZipFile]::OpenRead($zip)
        try {
            foreach ($entry in $archive.Entries) {
                $name = $entry.FullName
                $parts = $name -split '[\\/]'
                if ($name.StartsWith('/') -or $name.StartsWith('\') -or $name -match '^[A-Za-z]:' -or $parts -contains '..') {
                    Fail "the archive holds an unsafe path ($name); nothing was installed"
                }
            }
        } finally {
            $archive.Dispose()
        }

        $staged = Join-Path $work 'release'
        Expand-Archive -LiteralPath $zip -DestinationPath $staged
        $stagedExe = Join-Path $staged 'ai-memory.exe'
        if (-not (Test-Path -LiteralPath $stagedExe)) { Fail "$asset has no ai-memory.exe" }
        Get-ChildItem -LiteralPath $staged -Recurse -File | Unblock-File
        $reported = & $stagedExe --version 2>$null
        if ($LASTEXITCODE -ne 0) { Fail 'the downloaded binary does not run on this machine' }

        $parent = Split-Path -Parent $dest
        if ($parent) { New-Item -ItemType Directory -Force -Path $parent | Out-Null }
        # CreateDirectoryW fails if another attempt already owns this name;
        # Directory.CreateDirectory would silently accept an existing folder.
        # Compile only in the installer's own temporary directory on PS 5.1.
        if (-not ('AiMemory.InstallDirectory' -as [type])) {
            $savedTmp, $savedTemp = $env:TMP, $env:TEMP
            try {
                $env:TMP = $work; $env:TEMP = $work
                Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
namespace AiMemory {
    public static class InstallDirectory {
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool CreateDirectoryW(string path, IntPtr attributes);
        public static void CreateExclusive(string path) {
            if (!CreateDirectoryW(path, IntPtr.Zero))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "Cannot create install staging directory");
        }
    }
}
'@
            } finally {
                $env:TMP = $savedTmp; $env:TEMP = $savedTemp
            }
        }
        $prepared = Join-Path $parent ('.ai-memory-install-' + [Guid]::NewGuid().ToString('N'))
        [AiMemory.InstallDirectory]::CreateExclusive($prepared)
        # Copy the binary that ran (#1480): antivirus may still hold it open
        # without delete sharing. Publish its unexecuted copy from the same
        # volume as the destination. Directory.Move refuses *any* existing
        # destination, including one created after the initial check.
        # https://learn.microsoft.com/dotnet/api/system.io.directory.move
        try {
            foreach ($entry in Get-ChildItem -LiteralPath $staged -Force) {
                Copy-Item -LiteralPath $entry.FullName -Destination $prepared -Recurse
            }
            [IO.Directory]::Move($prepared, $dest)
        } catch {
            # Never remove the destination: it may belong to a concurrent
            # process. Retain failed staging rather than risking a cleanup of
            # a directory replaced by somebody else after we created it.
            Say "installation was not published; inspect leftover staging at $prepared before removing it"
            throw
        }
    } finally {
        Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
    }

    Say "installed $reported to $dest"
    if ($env:AI_MEMORY_NO_MODIFY_PATH -eq '1') {
        Say "left your PATH alone (AI_MEMORY_NO_MODIFY_PATH=1)"
        Say "next: & `"$exe`" setup"
        return
    }
    # Read and write the registry value itself (#1509):
    # [Environment]::GetEnvironmentVariable expands every %VAR% entry, and
    # SetEnvironmentVariable stores the result as a plain REG_SZ string, so an
    # entry such as %JAVA_HOME%\bin would stop following its variable. Entries
    # are still compared expanded, so one naming $dest through a variable counts.
    $environment = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')
    try {
        $userPath = $environment.GetValue('Path', '', 'DoNotExpandEnvironmentNames')
        $entries = if ($userPath) { $userPath -split ';' | Where-Object { $_ } } else { @() }
        $expanded = $entries | ForEach-Object { [Environment]::ExpandEnvironmentVariables($_) }
        if ($expanded -notcontains $dest) {
            $environment.SetValue('Path', (@($entries) + $dest) -join ';', 'ExpandString')
            # Removing a variable that does not exist changes nothing, but .NET
            # still broadcasts WM_SETTINGCHANGE after it, so Explorer and the
            # terminals it starts pick up the new PATH.
            $notify = 'ai-memory-install-' + [Guid]::NewGuid().ToString('N')
            [Environment]::SetEnvironmentVariable($notify, [NullString]::Value, 'User')
            Say "added $dest to your user PATH"
        }
    } finally {
        $environment.Dispose()
    }
    if (($env:Path -split ';') -notcontains $dest) { $env:Path = "$env:Path;$dest" }
    Say 'next: ai-memory setup'
    Say "(in a terminal opened before this install, run: & `"$exe`" setup)"
}

Install-AiMemory
