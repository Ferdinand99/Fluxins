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
        $script:Failed += $Name
    }
    Save-Screen $Name
    # A panic on another thread can leave the window up: the log tells.
    if (-not ($script:Failed -contains $Name) -and (Select-String -Path $log -Pattern 'panicked' -Quiet)) {
        Write-Host "$($Name): a panic in the log"
        Get-Content $log | Select-String -Pattern 'panicked' -Context 0,3
        $script:Failed += $Name
    }
    if (-not $p.HasExited) {
        $p.Refresh()
        Write-Host ("memory {0}: working set {1} MB, private {2} MB" -f $Name, [math]::Round($p.WorkingSet64 / 1MB), [math]::Round($p.PrivateMemorySize64 / 1MB))
        Stop-Process -Id $p.Id -Force
    }
    foreach ($k in $Env.Keys) { Remove-Item -Path "Env:$k" -ErrorAction SilentlyContinue }
    Start-Sleep -Seconds 1
}

$script:Failed = @()
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
# Images, files, a video still, a spoiler and explicit media; every embed type; the viewer.
Run-Demo "14-attachments" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "attachments"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-attachments" } 10
Run-Demo "15-embed-all-types" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "embed-all-types"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-embeds" } 10
Run-Demo "16-image-viewer" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "image-viewer"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-attachments" } 10
# A prompt over a chat full of Twemoji: the dimmed backdrop must cover the emoji too.
Run-Demo "17-overlay-over-emoji" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "markdown-inline"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-dev"; INLET_DEMO_DEEP_LINK = "inlet://invite/abc123" } 10

# Message actions: reactions and the hover bar, the context menu, the delete confirmation.
Run-Demo "18-chat-reactions" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "chat-reactions"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-general" } 10
Run-Demo "19-chat-menu" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "chat-menu"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-general" } 10
Run-Demo "20-chat-delete" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "chat-delete"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-general" } 10
# Several accounts: the account switcher and adding an account.
Run-Demo "21-accounts" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "accounts"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-general" } 10
Run-Demo "22-accounts-add" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "accounts-add"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-general" } 10

# The panels under the channel header: pinned messages, and the inbox's three tabs.
Run-Demo "23-chat-pins" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "chat-pins"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-general" } 10
Run-Demo "24-inbox-unreads" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "inbox-unreads"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-general" } 10
Run-Demo "25-inbox-saved" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "inbox-saved"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-general" } 10
Run-Demo "26-inbox-mentions" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "inbox-mentions"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-general" } 10

# Threads: the side panel with its own composer, the thread list, a forum and a media channel.
Run-Demo "27-thread-panel" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "thread-panel"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-general" } 10
Run-Demo "28-thread-list" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "thread-list"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-general" } 10
Run-Demo "29-forum" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "forum"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-forum" } 10
Run-Demo "30-media-channel" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "media-channel"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-gallery" } 10

# A demo that quit before its screenshot crashed: fail the job, the pictures are not
# worth checking.
if ($script:Failed.Count -gt 0) {
    Write-Host "exited early: $($script:Failed -join ', ')"
    exit 1
}
