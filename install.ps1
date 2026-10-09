# Install the latest rivet release for Windows (x86_64).
# Usage: irm https://raw.githubusercontent.com/cfuncdoor/rivet/main/install.ps1 | iex
$ErrorActionPreference = "Stop"

$Repo = if ($env:RIVET_REPO) { $env:RIVET_REPO } else { "cfuncdoor/rivet" }
$Version = if ($env:RIVET_VERSION) { $env:RIVET_VERSION } else { "latest" }
$Dest = if ($env:DEST) { $env:DEST } else { "$HOME\.rivet\bin" }

if ($Version -eq "latest") {
  $Url = "https://github.com/$Repo/releases/latest/download/rivet-x86_64-pc-windows-msvc.zip"
} else {
  $Url = "https://github.com/$Repo/releases/download/$Version/rivet-x86_64-pc-windows-msvc.zip"
}

$tmp = New-Item -ItemType Directory -Path (Join-Path $env:TEMP ("rivet-" + [guid]::NewGuid()))
try {
  Write-Host "downloading $Url"
  Invoke-WebRequest -Uri $Url -OutFile (Join-Path $tmp "rivet.zip")
  Expand-Archive -Path (Join-Path $tmp "rivet.zip") -DestinationPath $tmp -Force
  New-Item -ItemType Directory -Path $Dest -Force | Out-Null
  Move-Item -Path (Join-Path $tmp "rivet.exe") -Destination (Join-Path $Dest "rivet.exe") -Force
  Write-Host "installed rivet to $Dest\rivet.exe"
  if (($env:PATH -split ";") -notcontains $Dest) {
    Write-Host "note: $Dest is not on PATH; add it via System Properties or: `$env:PATH += `";$Dest`""
  }
} finally {
  Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
