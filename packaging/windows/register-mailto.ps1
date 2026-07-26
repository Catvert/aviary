# Registers Aviary as a `mailto:` handler for the current user.
#
# Windows has no equivalent of the desktop entry that does this on Linux: a
# handler is a set of registry keys. Everything below lives under HKCU, so no
# administrator rights are involved and nothing is written for other users.
#
#   powershell -ExecutionPolicy Bypass -File register-mailto.ps1
#   powershell -ExecutionPolicy Bypass -File register-mailto.ps1 -Unregister
#
# Windows still asks the user to confirm the default mail app the first time a
# link is clicked; this only makes Aviary appear in that list.

[CmdletBinding()]
param(
    # Defaults to aviary.exe sitting next to this script, which is how the
    # release archive is laid out.
    [string] $ExePath = (Join-Path $PSScriptRoot 'aviary.exe'),
    [switch] $Unregister
)

$ErrorActionPreference = 'Stop'

$progId    = 'Aviary.Mailto'
$classes   = 'HKCU:\Software\Classes'
$capability = 'HKCU:\Software\Aviary\Capabilities'

if ($Unregister) {
    Remove-Item -Path (Join-Path $classes $progId) -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item -Path 'HKCU:\Software\Aviary' -Recurse -Force -ErrorAction SilentlyContinue
    Remove-ItemProperty -Path 'HKCU:\Software\RegisteredApplications' -Name 'Aviary' -ErrorAction SilentlyContinue
    Write-Host 'Aviary is no longer registered as a mailto: handler.'
    return
}

if (-not (Test-Path -LiteralPath $ExePath)) {
    throw "aviary.exe not found at $ExePath. Pass -ExePath <path to aviary.exe>."
}
$exe = (Resolve-Path -LiteralPath $ExePath).Path

# The ProgId: what to run, and what to show in the "open with" list.
$progIdKey = Join-Path $classes $progId
New-Item -Path $progIdKey -Force | Out-Null
New-ItemProperty -Path $progIdKey -Name '(Default)' -Value 'Aviary mail' -PropertyType String -Force | Out-Null
New-ItemProperty -Path $progIdKey -Name 'URL Protocol' -Value '' -PropertyType String -Force | Out-Null

$iconKey = Join-Path $progIdKey 'DefaultIcon'
New-Item -Path $iconKey -Force | Out-Null
New-ItemProperty -Path $iconKey -Name '(Default)' -Value "`"$exe`",0" -PropertyType String -Force | Out-Null

# `%1` is the clicked URL. Aviary parses it with the same RFC 6068 reader it
# uses everywhere else, and hands it to the running instance over its named
# pipe rather than starting a second one.
$commandKey = Join-Path $progIdKey 'shell\open\command'
New-Item -Path $commandKey -Force | Out-Null
New-ItemProperty -Path $commandKey -Name '(Default)' -Value "`"$exe`" `"%1`"" -PropertyType String -Force | Out-Null

# Default Programs: what makes Aviary appear in Settings → Default apps.
New-Item -Path $capability -Force | Out-Null
New-ItemProperty -Path $capability -Name 'ApplicationName' -Value 'Aviary' -PropertyType String -Force | Out-Null
New-ItemProperty -Path $capability -Name 'ApplicationDescription' `
    -Value 'Desktop email, calendar and kanban client' -PropertyType String -Force | Out-Null

$urlAssociations = Join-Path $capability 'URLAssociations'
New-Item -Path $urlAssociations -Force | Out-Null
New-ItemProperty -Path $urlAssociations -Name 'mailto' -Value $progId -PropertyType String -Force | Out-Null

New-Item -Path 'HKCU:\Software\RegisteredApplications' -Force | Out-Null
New-ItemProperty -Path 'HKCU:\Software\RegisteredApplications' -Name 'Aviary' `
    -Value 'Software\Aviary\Capabilities' -PropertyType String -Force | Out-Null

Write-Host "Aviary registered as a mailto: handler ($exe)."
Write-Host 'Pick it under Settings → Apps → Default apps to make it the default.'
