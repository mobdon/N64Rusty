# Checks that a change to the player leaves every game as it was: runs each
# ROM headless with two builds of n64rusty-play and compares their RAM at
# the end, on Windows.
#
#   powershell -ExecutionPolicy Bypass -File scripts\compare.ps1 -Old <old.exe> -New <new.exe> -Roms <folder> [-Data <folder>] [-Frames 4000]
#
# -Old, -New  two n64rusty-play.exe builds (for example main's and yours).
# -Roms       a folder of your own ROMs (.z64, .n64, .v64).
# -Data       optional player data folder (its games\ and routines.txt), the
#             one N64RUSTY_PLAYER_DATA names, so games set up by hand run.
# -Frames     how long each game runs (default 4000).
# Prints, per game, SAME or DIFF and each build's time. A DIFF means the
# change altered what the game does (a run is otherwise repeatable).

param(
    [Parameter(Mandatory = $true)][string]$Old,
    [Parameter(Mandatory = $true)][string]$New,
    [Parameter(Mandatory = $true)][string]$Roms,
    [string]$Data = "",
    [int]$Frames = 4000
)
$ErrorActionPreference = "Stop"

# One folder per run (this process), so two comparisons at once keep apart.
# Folders of runs whose process has ended (kept after a DIFF, or left by an
# error) go now.
$tmp = [System.IO.Path]::GetTempPath()
Get-ChildItem -LiteralPath $tmp -Directory -Filter "n64rusty-compare-*" | Where-Object {
    $_.Name -match '-(\d+)$' -and -not (Get-Process -Id $Matches[1] -ErrorAction SilentlyContinue)
} | Remove-Item -Recurse -Force -ErrorAction SilentlyContinue
$work = Join-Path $tmp "n64rusty-compare-$PID"
Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
# Each build runs from its own folder: the player reads games\ and
# routines.txt next to itself and writes saves there.
foreach ($b in @{ name = "old"; exe = $Old }, @{ name = "new"; exe = $New }) {
    $dir = Join-Path $work $b.name
    New-Item -ItemType Directory -Force $dir | Out-Null
    Copy-Item -LiteralPath (Resolve-Path -LiteralPath $b.exe).Path (Join-Path $dir "player.exe")
    if ($Data) {
        Copy-Item -Recurse -LiteralPath (Join-Path $Data "games") $dir
        Copy-Item -LiteralPath (Join-Path $Data "routines.txt") $dir -ErrorAction SilentlyContinue
    }
}

$games = Get-ChildItem -LiteralPath $Roms -File | Where-Object { $_.Extension -in ".z64", ".n64", ".v64" }
if (-not $games) { throw "no .z64, .n64 or .v64 file in $Roms" }
$bad = 0
foreach ($g in $games) {
    $times = @{}
    foreach ($b in "old", "new") {
        $dir = Join-Path $work $b
        $ram = Join-Path $dir "ram.bin"
        Remove-Item -LiteralPath $ram -ErrorAction SilentlyContinue
        $t = [System.Diagnostics.Stopwatch]::StartNew()
        # The player is a window program: PowerShell would not wait for it.
        $p = Start-Process -FilePath (Join-Path $dir "player.exe") -Wait -PassThru -NoNewWindow `
            -ArgumentList "`"$($g.FullName)`"", "--frames", $Frames, "--ram-out", "`"$ram`"" `
            -RedirectStandardOutput (Join-Path $dir "log.txt") -RedirectStandardError (Join-Path $dir "err.txt")
        $times[$b] = [math]::Round($t.Elapsed.TotalSeconds, 1)
        if (-not (Test-Path -LiteralPath $ram)) { $times[$b] = "no RAM" }
    }
    $a = Get-FileHash (Join-Path $work "old\ram.bin") -ErrorAction SilentlyContinue
    $n = Get-FileHash (Join-Path $work "new\ram.bin") -ErrorAction SilentlyContinue
    $same = $a -and $n -and $a.Hash -eq $n.Hash
    $verdict = if ($same) { "SAME" } else { "DIFF" }
    Write-Host ("{0,-4} {1}  old {2} s, new {3} s" -f $verdict, $g.Name, $times["old"], $times["new"])
    if (-not $same) {
        $bad++
        # This game's logs and RAM, before the next game's run replaces them.
        $keep = Join-Path $work ("game" + $bad)
        foreach ($b in "old", "new") {
            $to = Join-Path $keep $b
            New-Item -ItemType Directory -Force $to | Out-Null
            foreach ($f in "log.txt", "err.txt", "ram.bin") {
                Copy-Item -LiteralPath (Join-Path (Join-Path $work $b) $f) $to -ErrorAction SilentlyContinue
            }
        }
        Write-Host "     logs and RAM: $keep"
    }
}
# Kept when a game differs or fails (see the paths above); the next run
# removes it.
if ($bad -eq 0) { Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue }
exit $bad
