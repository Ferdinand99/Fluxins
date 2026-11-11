# Starts the demo build of Inlet on Windows and saves screenshots of the whole screen.
# Used by CI so Windows rendering is checked by eye, not only compiled.
# Usage: scripts/windows-screenshot.ps1 <path to inlet.exe> <output folder>
param(
    [Parameter(Mandatory = $true)][string]$Exe,
    [Parameter(Mandatory = $true)][string]$OutDir
)
$ErrorActionPreference = "Stop"
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

function Save-Screen([string]$Name) {
    $bounds = [System.Windows.Forms.SystemInformation]::VirtualScreen
    $bitmap = New-Object System.Drawing.Bitmap $bounds.Width, $bounds.Height
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $graphics.CopyFromScreen($bounds.Left, $bounds.Top, 0, 0, $bitmap.Size)
    $path = Join-Path $OutDir "$Name.png"
    $bitmap.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    $graphics.Dispose()
    $bitmap.Dispose()
    Write-Host "saved $path ($($bounds.Width)x$($bounds.Height))"
}

function Run-Demo([string]$Name, [hashtable]$Env, [int]$Seconds) {
    foreach ($k in $Env.Keys) { Set-Item -Path "Env:$k" -Value $Env[$k] }
    $log = Join-Path $OutDir "$Name.log"
    $p = Start-Process -FilePath $Exe -PassThru -RedirectStandardError $log
    Start-Sleep -Seconds $Seconds
    if ($p.HasExited) {
        Write-Host "inlet.exe exited early with code $($p.ExitCode)"
        Get-Content $log | Select-Object -Last 40
    }
    Save-Screen $Name
    if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force }
    foreach ($k in $Env.Keys) { Remove-Item -Path "Env:$k" -ErrorAction SilentlyContinue }
    Start-Sleep -Seconds 1
}

$env:RUST_LOG = "info"
# Sign-in screen (no saved session, no demo).
Run-Demo "01-sign-in" @{} 8
# A guild with many channels (emoji, long and non-Latin names) and its general channel.
Run-Demo "02-guild-channels" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-general" } 10
# The bot channel with embed-only messages.
Run-Demo "03-embeds" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-joins"; INLET_DEMO_ACCENT = "#ff7e00" } 10
# The message list: unread line, mentions, tags, sending and failed (cozy and compact),
# system messages and replies.
Run-Demo "04-chat-cozy" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "chat-cozy"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-general" } 10
Run-Demo "05-chat-compact" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "chat-compact"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-general" } 10
Run-Demo "06-chat-system" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "chat-system"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-system" } 10
Run-Demo "07-chat-replies" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "chat-replies"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-replies" } 10
# Markdown: every node kind of Fluxer's parser, inline and block.
Run-Demo "08-markdown-inline" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "markdown-inline"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-dev" } 10
Run-Demo "09-markdown-blocks" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "markdown-blocks"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-dev" } 10
Run-Demo "10-markdown-alerts" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "markdown-alerts"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-dev" } 10
Run-Demo "11-markdown-code" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "markdown-code"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-dev" } 10
# The Desktop settings tab, and the prompt a link from outside opens.
Run-Demo "12-settings-desktop" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-general"; INLET_DEMO_SETTINGS = "desktop" } 10
Run-Demo "13-link-prompt" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-general"; INLET_DEMO_DEEP_LINK = "inlet://invite/abc123" } 10
# Message actions: reactions and the hover bar, the context menu, the delete confirmation.
Run-Demo "14-chat-reactions" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "chat-reactions"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-general" } 10
Run-Demo "15-chat-menu" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "chat-menu"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-general" } 10
Run-Demo "16-chat-delete" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "chat-delete"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-general" } 10
