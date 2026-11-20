# Rebuild the README images from the real program (Windows).
#
#   powershell -File docs/assets/make-visuals.ps1
#
# Uses a separate demo instance (HOTBAR_INSTANCE, HOTBAR_DATA), so it never
# touches your own history or a running daemon. Needs `hotbar` on PATH and
# uv (https://docs.astral.sh/uv/) for readme_gifs.py.
param([string]$Hotbar = "hotbar")

$ErrorActionPreference = "Stop"
$here = $PSScriptRoot
$env:HOTBAR_INSTANCE = "readme"
$env:HOTBAR_DATA = Join-Path $here ".demo"
$env:HOTBAR_CONFIG = Join-Path $here ".demo\config.toml"   # missing on purpose: defaults
if (Test-Path $env:HOTBAR_DATA) { Remove-Item -Recurse -Force $env:HOTBAR_DATA }
New-Item -ItemType Directory -Force $env:HOTBAR_DATA | Out-Null

Add-Type -AssemblyName System.Drawing

# Demo history, oldest first. `set 9` then `clear 9` leaves plain history entries.
$history = @(
    "export RUST_LOG=debug",
    "Meeting moved to Thursday 10:00, same room.",
    "docker compose up -d --build",
    "#89b4fa",
    "https://github.com/mimobn/hotbar",
    "cargo build --release && ./target/release/hotbar doctor"
)
foreach ($t in $history) { & $Hotbar set 9 $t }
& $Hotbar clear 9

# A small flat image (a bar chart) to show image support.
$png = Join-Path $env:HOTBAR_DATA "chart.png"
$bmp = New-Object System.Drawing.Bitmap 360, 200
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.Clear([System.Drawing.Color]::FromArgb(24, 24, 37))
$colors = @("#89b4fa", "#a6e3a1", "#f9e2af", "#f38ba8", "#cba6f7")
$heights = @(120, 160, 90, 140, 70)
for ($i = 0; $i -lt 5; $i++) {
    $brush = New-Object System.Drawing.SolidBrush ([System.Drawing.ColorTranslator]::FromHtml($colors[$i]))
    $g.FillRectangle($brush, 30 + $i * 64, 180 - $heights[$i], 44, $heights[$i])
}
$bmp.Save($png, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose(); $bmp.Dispose()

# Slots.
& $Hotbar set 2 "git log --oneline --graph --decorate -20"
& $Hotbar set 3 "Best regards,`nAlex"
& $Hotbar set 4 "support@example.com"
& $Hotbar set 5 "SELECT id, email FROM users WHERE created_at > now() - interval '7 days';"
cmd /c "`"$Hotbar`" set 6 < `"$png`""
& $Hotbar set 1 "ssh deploy@10.0.0.12 -p 2222"

# Picker stills, rendered off screen by the real window.
$imageRow = 0
$i = 0
foreach ($line in (& $Hotbar list)) {
    if ($line -match "\[image") { $imageRow = $i; break }
    $i++
}
& $Hotbar pick --snapshot (Join-Path $here "picker-slots.png")
& $Hotbar pick --snapshot (Join-Path $here "picker-search.png") --query "git"
& $Hotbar pick --snapshot (Join-Path $here "picker-image.png") --select $imageRow

# The popup, captured by window handle (nothing else on screen is recorded).
Add-Type @"
using System; using System.Runtime.InteropServices;
public static class Win {
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindow(string c, string t);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint f);
  public struct RECT { public int L, T, R, B; }
}
"@
[Win]::SetProcessDPIAware() | Out-Null
$p = Start-Process -FilePath $Hotbar -ArgumentList "popup", "`"Slot 1   ssh deploy@10.0.0.12 -p 2222`"" -PassThru -WindowStyle Hidden
$h = [IntPtr]::Zero
for ($n = 0; $n -lt 40 -and $h -eq [IntPtr]::Zero; $n++) {
    Start-Sleep -Milliseconds 50
    $h = [Win]::FindWindow("hotbar_osd", [NullString]::Value)
}
if ($h -ne [IntPtr]::Zero) {
    Start-Sleep -Milliseconds 150
    $r = New-Object Win+RECT
    [Win]::GetWindowRect($h, [ref]$r) | Out-Null
    $shot = New-Object System.Drawing.Bitmap ($r.R - $r.L), ($r.B - $r.T)
    $sg = [System.Drawing.Graphics]::FromImage($shot)
    $hdc = $sg.GetHdc(); [Win]::PrintWindow($h, $hdc, 2) | Out-Null; $sg.ReleaseHdc($hdc)
    $shot.Save((Join-Path $here "popup.png"), [System.Drawing.Imaging.ImageFormat]::Png)
}
$p.WaitForExit()

# GIFs: the terminal scene runs real commands, the picker scene fades the stills.
Push-Location $here
try { uv run readme_gifs.py all . } finally { Pop-Location }
Remove-Item -Recurse -Force $env:HOTBAR_DATA
Write-Output "done"
