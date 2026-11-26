# xshell: install (or update) the xshell Desktop on this Windows computer (design-rjw-windows W7).
#
#   irm https://github.com/Lesani/xshell/releases/download/<tag>/install.ps1 | iex
#
# It downloads xshell-windows-x86_64.zip of the release this script is pinned to and its
# .sha256, verifies the SHA-256 before anything is unpacked, installs to
# %LOCALAPPDATA%\xshell and adds xshell to the Start menu. No administrator rights; nothing is
# installed system-wide. If xshell is running from that folder, it says so and closes it first
# (that ends the sessions running on this computer).
#
# Nothing is sent but the downloads (User-Agent "xshell-install").
#
# Environment (also for tests): XSHELL_TAG (another release), XSHELL_INSTALL_BASE (download
# base: an https URL, a local folder or a file: URI holding <tag>\<files>), XSHELL_INSTALL_DIR
# (default %LOCALAPPDATA%\xshell), XSHELL_INSTALL_YES=1 (close a running xshell without asking).

& {
  Set-StrictMode -Version 2
  $ErrorActionPreference = 'Stop'
  $ProgressPreference = 'SilentlyContinue'

  # Pinned by publish-release.sh: the release this script belongs to.
  $TAG = "v1.6.0-rjw.3"
  $REPO_BASE = "https://github.com/Lesani/xshell/releases/download"
  $ZIP = "xshell-windows-x86_64.zip"

  $tag = if ($env:XSHELL_TAG) { $env:XSHELL_TAG } else { $TAG }
  $base = if ($env:XSHELL_INSTALL_BASE) { $env:XSHELL_INSTALL_BASE.TrimEnd('/', '\') } else { $REPO_BASE }
  $version = $tag -replace '^v', ''
  $dir = if ($env:XSHELL_INSTALL_DIR) { $env:XSHELL_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'xshell' }

  function Stop-Install([string]$message) {
    Write-Host $message -ForegroundColor Red
    throw 'xshell installation stopped'
  }

  if (-not [Environment]::Is64BitOperatingSystem -or $env:PROCESSOR_ARCHITECTURE -eq 'ARM64') {
    Stop-Install "xshell isn't available for this Windows ($env:PROCESSOR_ARCHITECTURE). Installation stopped."
  }

  function Get-Download([string]$name, [string]$to) {
    $from = "$base/$tag/$name"
    if ($from -match '^https?://') {
      [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
      Invoke-WebRequest -UseBasicParsing -UserAgent 'xshell-install' -Uri $from -OutFile $to
    } else {
      $path = if ($from -match '^file:') { ([Uri]$from).LocalPath } else { $from }
      Copy-Item -LiteralPath $path -Destination $to
    }
  }

  $tmp = Join-Path ([IO.Path]::GetTempPath()) ("xshell-install-" + [Guid]::NewGuid().ToString('N'))
  New-Item -ItemType Directory -Path $tmp | Out-Null
  try {
    Write-Host "Downloading xshell $version..."
    try {
      Get-Download $ZIP (Join-Path $tmp $ZIP)
      Get-Download "$ZIP.sha256" (Join-Path $tmp "$ZIP.sha256")
    } catch {
      Stop-Install "The download failed: $($_.Exception.Message). Installation stopped. Nothing was changed."
    }

    Write-Host "Verifying download..."
    $line = (Get-Content -LiteralPath (Join-Path $tmp "$ZIP.sha256") -TotalCount 1)
    $want = if ($line -match '^([0-9A-Fa-f]{64})') { $Matches[1].ToLowerInvariant() } else { '' }
    $got = (Get-FileHash -LiteralPath (Join-Path $tmp $ZIP) -Algorithm SHA256).Hash.ToLowerInvariant()
    if (-not $want -or $want -ne $got) {
      Stop-Install "Checksum verification failed. Installation stopped. Nothing was changed. Download again and retry."
    }

    $unpacked = Join-Path $tmp 'unpacked'
    Expand-Archive -LiteralPath (Join-Path $tmp $ZIP) -DestinationPath $unpacked
    if (-not (Test-Path -LiteralPath (Join-Path $unpacked 'xshell.exe'))) {
      Stop-Install "The download does not contain xshell.exe. Installation stopped. Nothing was changed."
    }

    # A running xshell from the install folder holds its files: close it, after saying so.
    $full = [IO.Path]::GetFullPath($dir).TrimEnd('\') + '\'
    $running = @(Get-Process -Name xshell, xshelld -ErrorAction SilentlyContinue | Where-Object {
      $p = $null; try { $p = $_.Path } catch { }
      $p -and $p.StartsWith($full, [StringComparison]::OrdinalIgnoreCase)
    })
    if ($running.Count -gt 0) {
      Write-Host "xshell is running from $dir. Updating it closes xshell, and the sessions running on this computer end."
      $interactive = [Environment]::UserInteractive -and -not [Console]::IsInputRedirected
      if ($env:XSHELL_INSTALL_YES -ne '1' -and $interactive) {
        $answer = Read-Host "Close xshell and continue? [y/N]"
        if ($answer -notmatch '^(y|yes)$') { Stop-Install "Installation stopped. xshell is still running; nothing was changed." }
      } elseif ($env:XSHELL_INSTALL_YES -ne '1') {
        Stop-Install "Close xshell, then run this command again (or set XSHELL_INSTALL_YES=1). Nothing was changed."
      }
      $running | Where-Object Name -eq 'xshell' | ForEach-Object { $null = $_.CloseMainWindow() }
      $running | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
      $running | Where-Object { -not $_.HasExited } | Stop-Process -Force -ErrorAction SilentlyContinue
      $running | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
    }

    $update = Test-Path -LiteralPath (Join-Path $dir 'xshell.exe')
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    Get-ChildItem -LiteralPath $unpacked -File | ForEach-Object {
      Copy-Item -LiteralPath $_.FullName -Destination (Join-Path $dir $_.Name) -Force
    }
    # An older zip shipped xshelld.exe beside the Desktop; a newer Desktop embeds it.
    if (-not (Test-Path -LiteralPath (Join-Path $unpacked 'xshelld.exe'))) {
      Remove-Item -LiteralPath (Join-Path $dir 'xshelld.exe') -Force -ErrorAction SilentlyContinue
    }

    $programs = [Environment]::GetFolderPath('Programs')
    $shell = New-Object -ComObject WScript.Shell
    $link = $shell.CreateShortcut((Join-Path $programs 'xshell.lnk'))
    $link.TargetPath = Join-Path $dir 'xshell.exe'
    $link.WorkingDirectory = $dir
    $link.Description = 'xshell'
    $link.Save()

    if ($update) { Write-Host "Updated xshell to $version in $dir." } else { Write-Host "Installed xshell $version in $dir." }
    Write-Host "Start it from the Start menu: xshell."
  } finally {
    Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
  }
}
