# Starts the demo build of Inlet on Windows and saves screenshots: of the whole screen
# for the numbered demo runs (checked by eye), and of the window's inside for every scene
# file in tests/visual/scenes (compared with the baselines by inlet-visual).
# Used by CI so Windows rendering is checked, not only compiled.
# Usage: scripts/windows-screenshot.ps1 <path to inlet.exe> <output folder>
#   The scene screenshots and their logs land in <output folder>/visual.
param(
    [Parameter(Mandatory = $true)][string]$Exe,
    [Parameter(Mandatory = $true)][string]$OutDir,
    [string]$ScenesDir = (Join-Path $PSScriptRoot "../tests/visual/scenes")
)
$ErrorActionPreference = "Stop"
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class InletScreen {
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    public struct DEVMODE {
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string dmDeviceName;
        public short dmSpecVersion; public short dmDriverVersion; public short dmSize; public short dmDriverExtra;
        public int dmFields; public int dmPositionX; public int dmPositionY; public int dmDisplayOrientation; public int dmDisplayFixedOutput;
        public short dmColor; public short dmDuplex; public short dmYResolution; public short dmTTOption; public short dmCollate;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string dmFormName;
        public short dmLogPixels; public int dmBitsPerPel; public int dmPelsWidth; public int dmPelsHeight;
        public int dmDisplayFlags; public int dmDisplayFrequency; public int dmICMMethod; public int dmICMIntent; public int dmMediaType;
        public int dmDitherType; public int dmReserved1; public int dmReserved2; public int dmPanningWidth; public int dmPanningHeight;
    }
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern bool EnumDisplaySettings(string dev, int mode, ref DEVMODE dm);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int ChangeDisplaySettings(ref DEVMODE dm, int flags);
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr hwnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr hwnd, ref POINT point);
    [DllImport("user32.dll")] public static extern int GetDpiForWindow(IntPtr hwnd);
    // The runner's screen is 1024x768, too small for a 1280x860 scene window.
    public static int SetResolution(int w, int h) {
        var dm = new DEVMODE(); dm.dmSize = (short)Marshal.SizeOf(typeof(DEVMODE));
        EnumDisplaySettings(null, -1, ref dm);
        dm.dmPelsWidth = w; dm.dmPelsHeight = h; dm.dmFields = 0x80000 | 0x100000;
        return ChangeDisplaySettings(ref dm, 0);
    }
    // The window's inside (no frame, no title bar) in screen coordinates.
    public static int[] ClientArea(IntPtr hwnd) {
        RECT r; if (!GetClientRect(hwnd, out r)) return null;
        var p = new POINT(); if (!ClientToScreen(hwnd, ref p)) return null;
        return new int[] { p.X, p.Y, r.Right - r.Left, r.Bottom - r.Top };
    }
}
'@

# A fixed screen: 1920x1080 at 96 DPI (100 %), the pointer in the corner, away from
# the window, so nothing shows a hover state.
$set = [InletScreen]::SetResolution(1920, 1080)
$screen = [System.Windows.Forms.SystemInformation]::VirtualScreen
Write-Host "display: $($screen.Width)x$($screen.Height) (change result $set)"
[System.Windows.Forms.Cursor]::Position = New-Object System.Drawing.Point(0, 0)

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

function Run-Demo([string]$Name, [hashtable]$Env, [int]$Seconds, [int]$CpuSeconds = 0) {
    foreach ($k in $Env.Keys) { Set-Item -Path "Env:$k" -Value $Env[$k] }
    $log = Join-Path $OutDir "$Name.log"
    $p = Start-Process -FilePath $Exe -PassThru -RedirectStandardError $log
    Start-Sleep -Seconds $Seconds
    # What the app costs while it shows the scene: CPU time over a few seconds, as a
    # share of one core.
    if ($CpuSeconds -gt 0 -and -not $p.HasExited) {
        $p.Refresh(); $cpu0 = $p.TotalProcessorTime; $t0 = Get-Date
        Start-Sleep -Seconds $CpuSeconds
        $p.Refresh(); $cpu1 = $p.TotalProcessorTime; $t1 = Get-Date
        $share = ($cpu1 - $cpu0).TotalMilliseconds / ($t1 - $t0).TotalMilliseconds * 100
        Write-Host ("cpu {0}: {1:N1} % of one core over {2} s" -f $Name, $share, $CpuSeconds)
    }
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

# Custom emoji (still, animated, large) and stickers in every format; then 100 animated
# custom emoji at once, with the CPU they cost (at most 32 move).
Run-Demo "27-custom-emoji" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "custom-emoji"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-dev" } 10 10
Run-Demo "28-emoji-100" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "emoji-100"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-dev" } 10 10
# The same with animated emoji switched off: every emoji shows its first frame.
Run-Demo "29-emoji-100-still" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "emoji-100"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-dev"; INLET_DEMO_ANIMATE_EMOJI = "0" } 10 10

# Threads: the side panel with its own composer, the thread list, a forum and a media channel.
Run-Demo "30-thread-panel" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "thread-panel"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-general" } 10
Run-Demo "31-thread-list" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "thread-list"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-general" } 10
Run-Demo "32-forum" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "forum"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-forum" } 10
Run-Demo "33-media-channel" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "media-channel"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-gallery" } 10
# The Notifications settings tab. The demo also posts a test notification; the runner's
# session does not show toasts, so the toast itself is checked by hand.
Run-Demo "34-settings-notifications" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-general"; INLET_DEMO_SETTINGS = "notifications"; INLET_DEMO_NOTIFY = "1" } 10

# The member list beside the chat, and the profile popout of a member.
Run-Demo "39-members" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "members"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-general" } 10
Run-Demo "40-profile-popout" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "profile-popout"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-general" } 10

# One scene file: the app opens in the state the file names, at its window size, and
# the inside of the window is saved as visual/<name>.png.
function Run-Scene([System.IO.FileInfo]$File) {
    $name = (Select-String -Path $File.FullName -Pattern '^name\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
    $dir = Join-Path $OutDir "visual"
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    $log = Join-Path $dir "$name.log"
    $env:INLET_DEMO_SCENE = $File.FullName
    $p = Start-Process -FilePath $Exe -PassThru -RedirectStandardError $log
    Start-Sleep -Seconds 10
    $p.Refresh()
    if ($p.HasExited) {
        Write-Host "scene $($name): inlet.exe exited early with code $($p.ExitCode)"
        Get-Content $log | Select-Object -Last 40
        $script:Failed += "scene $name"
    } else {
        $area = [InletScreen]::ClientArea($p.MainWindowHandle)
        if ($null -eq $area) {
            Write-Host "scene $($name): no window"
            $script:Failed += "scene $name"
        } else {
            $dpi = [InletScreen]::GetDpiForWindow($p.MainWindowHandle)
            $bitmap = New-Object System.Drawing.Bitmap $area[2], $area[3]
            $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
            $graphics.CopyFromScreen($area[0], $area[1], 0, 0, $bitmap.Size)
            $path = Join-Path $dir "$name.png"
            $bitmap.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
            $graphics.Dispose()
            $bitmap.Dispose()
            Write-Host "saved $path ($($area[2])x$($area[3]) at $dpi DPI)"
        }
        Stop-Process -Id $p.Id -Force
        $p.WaitForExit(10000) | Out-Null
    }
    if (Select-String -Path $log -Pattern 'panicked' -Quiet) {
        Write-Host "scene $($name): a panic in the log"
        Get-Content $log | Select-String -Pattern 'panicked' -Context 0,3
        $script:Failed += "scene $name"
    }
    Remove-Item -Path Env:INLET_DEMO_SCENE -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 1
}

# The scene files of the visual checks (the app reads each file itself).
foreach ($file in Get-ChildItem -Path $ScenesDir -Filter *.toml | Sort-Object Name) {
    Run-Scene $file
}

# Presence: the status picker open over the user panel, two people typing in #general.
Run-Demo "35-status-picker" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "status-picker"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-general" } 10

# The composer with files attached (previews, spoiler, alt text) and an upload on its way.
Run-Demo "36-composer-attachments" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "composer-attachments"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-general" } 10
# The composer's picker on the emoji tab and on the GIF tab.
Run-Demo "37-picker-emoji" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "picker-emoji"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-general" } 10
Run-Demo "38-picker-gif" @{ INLET_DEMO = "1"; INLET_DEMO_LIVE_SECS = "3600"; INLET_DEMO_SCENE = "picker-gif"; INLET_DEMO_GUILD = "g1"; INLET_DEMO_CHANNEL = "c-general" } 10

# A demo that quit before its screenshot crashed: fail the job, the pictures are not
# worth checking.
if ($script:Failed.Count -gt 0) {
    Write-Host "exited early: $($script:Failed -join ', ')"
    exit 1
}
