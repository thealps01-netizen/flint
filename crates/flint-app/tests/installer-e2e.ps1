# End-to-end check of the Flint installer on this machine. Installs and
# uninstalls for real (Program Files, the FlintIndexer service, Start menu),
# so each phase shows one Windows administrator prompt that a person must
# accept. Leaves the machine as it found it after the "uninstall" phase,
# except for the user's own Flint data, which a passive uninstall keeps.
#
#   powershell -File installer-e2e.ps1 -Phase install -Setup <setup.exe> [-Full]
#   powershell -File installer-e2e.ps1 -Phase update -Setup <newer setup.exe>
#   powershell -File installer-e2e.ps1 -Phase uninstall
#   powershell -File installer-e2e.ps1 -Phase refuse-folder -Setup <setup.exe>
#
# "update" runs the newer installer the way Flint's updater does (/P
# /UPDATE) but without /R, so no Flint is started (a started Flint would
# index this user's other drives).
# "refuse-folder" asks for an install outside Program Files, which the
# installer must refuse (it runs the service program as administrator
# from the install folder).
#
# Prints one "ok"/"FAIL" line per check and exits 1 if any check failed.
param(
  [Parameter(Mandatory)][ValidateSet("install", "update", "uninstall", "refuse-folder")][string]$Phase,
  [string]$Setup,
  [switch]$Full
)
$ErrorActionPreference = "Stop"
$dir = Join-Path $env:ProgramFiles "Flint"
$data = Join-Path $env:ProgramData "Flint"
$failed = 0
function Check([string]$what, [bool]$ok) {
  if ($ok) { "ok   $what" } else { "FAIL $what"; $script:failed++ }
}
function ServiceState {
  $s = Get-Service -Name FlintIndexer -ErrorAction SilentlyContinue
  if ($s) { "$($s.Status)" } else { "none" }
}
function ServicePath {
  $c = Get-CimInstance Win32_Service -Filter "Name='FlintIndexer'" -ErrorAction SilentlyContinue
  if ($c) { $c.PathName } else { "" }
}
# Waits (up to $Seconds) until the service returns results for a search
# this machine surely has; says how long that took.
function ServiceFinds([int]$Seconds) {
  $t0 = Get-Date
  $deadline = $t0.AddSeconds($Seconds)
  do {
    $said = & (Join-Path $dir "flint-indexer.exe") query "windows" --limit 1 2>&1 | Out-String
    if ($LASTEXITCODE -eq 0 -and $said -match "\(service indexer\)" -and $said -match "^([1-9][\d,.]*) matches") {
      $first = ($said -split "`n")[0].Trim()
      Check "the service finds files ($first; after $([int]((Get-Date) - $t0).TotalSeconds) s)" $true
      return
    }
    Start-Sleep -Seconds 1
  } while ((Get-Date) -lt $deadline)
  Check "the service finds files within $Seconds s (last answer: $($said.Trim()))" $false
}
$startMenu = Join-Path $env:ProgramData "Microsoft\Windows\Start Menu\Programs\Flint\Flint.lnk"

if ($Phase -eq "install") {
  $args = @("/P")
  if ($Full) { $args += "/FULLINDEX" }
  $p = Start-Process -FilePath $Setup -ArgumentList $args -Wait -PassThru
  Check "installer exit code 0 (was $($p.ExitCode))" ($p.ExitCode -eq 0)
  Check "flint.exe installed" (Test-Path (Join-Path $dir "flint.exe"))
  Check "flint-indexer.exe installed next to it" (Test-Path (Join-Path $dir "flint-indexer.exe"))
  Check "Start menu entry" (Test-Path $startMenu)
  if ($Full) {
    $deadline = (Get-Date).AddSeconds(30)
    while ((ServiceState) -ne "Running" -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 500 }
    Check "service running (is $(ServiceState))" ((ServiceState) -eq "Running")
    $path = ServicePath
    Check "service program is the installed one ($path)" ($path -like "*$dir\flint-indexer.exe*")
    # the first scan of every NTFS drive takes a few minutes
    ServiceFinds 600
  } else {
    Check "no service without full indexing (is $(ServiceState))" ((ServiceState) -eq "none")
  }
} elseif ($Phase -eq "refuse-folder") {
  $other = Join-Path $env:SystemDrive "FlintE2E\Flint"
  # /D must come last and unquoted (NSIS)
  $p = Start-Process -FilePath $Setup -ArgumentList "/P", "/FULLINDEX", "/D=$other" -Wait -PassThru
  Check "installer refused (exit code $($p.ExitCode))" ($p.ExitCode -ne 0)
  Check "nothing installed in $other" (-not (Test-Path (Join-Path $other "flint.exe")))
  Check "no service (is $(ServiceState))" ((ServiceState) -eq "none")
  Remove-Item (Split-Path $other) -Recurse -Force -ErrorAction SilentlyContinue
} elseif ($Phase -eq "update") {
  ServiceFinds 600
  $before = (Get-Item (Join-Path $dir "flint.exe")).VersionInfo.ProductVersion
  $p = Start-Process -FilePath $Setup -ArgumentList "/P", "/UPDATE" -Wait -PassThru
  Check "update installer exit code 0 (was $($p.ExitCode))" ($p.ExitCode -eq 0)
  $v = (Get-Item (Join-Path $dir "flint.exe")).VersionInfo.ProductVersion
  Check "installed version changed ($before -> $v)" ($v -ne $before)
  Check "service running after the update (is $(ServiceState))" ((ServiceState) -eq "Running")
  $path = ServicePath
  Check "service program is the installed one ($path)" ($path -like "*$dir\flint-indexer.exe*")
  # the index survives the update: results come back right away
  ServiceFinds 60
  $running = Get-Process -Name flint -ErrorAction SilentlyContinue
  Check "no Flint started by the update" ($null -eq $running)
} else {
  $un = Join-Path $dir "uninstall.exe"
  Check "uninstaller present" (Test-Path $un)
  # the uninstaller copies itself and returns at once; wait for the copy
  $p = Start-Process -FilePath $un -ArgumentList "/P" -Wait -PassThru
  $deadline = (Get-Date).AddSeconds(60)
  while ((Test-Path (Join-Path $dir "flint.exe")) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 500 }
  Start-Sleep -Seconds 2
  Check "uninstaller exit code 0 (was $($p.ExitCode))" ($p.ExitCode -eq 0)
  Check "service removed (is $(ServiceState))" ((ServiceState) -eq "none")
  Check "service index removed ($data)" (-not (Test-Path $data))
  Check "install folder removed ($dir)" (-not (Test-Path $dir))
  Check "Start menu entry removed" (-not (Test-Path $startMenu))
  $run = Get-ItemProperty "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" -Name Flint -ErrorAction SilentlyContinue
  Check "no start-with-Windows entry" ($null -eq $run)
  Check "no Flint registry key left" (-not (Test-Path "HKLM:\Software\Flint contributors"))
}
if ($failed) { "$failed check(s) failed"; exit 1 } else { "all checks passed" }
