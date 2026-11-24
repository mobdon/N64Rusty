# Translates, builds and plays a game live in a window, on Windows.
#
#   powershell -ExecutionPolicy Bypass -File scripts\play.ps1 -Rom <your.z64> -Symbols <map.txt>,<map2.txt> [-Files <folder>]
#
# -Rom      your own ROM (any byte order), or a folder holding it.
# -Symbols  symbol maps (`name = 0xADDR;` lines), passed to translate and to the game.
# -Files    optional folder with per-game run files (config.toml, dma.txt,
#           missing.txt, run.toml); they are copied into local\<sha256>\ first.
# Extra arguments after these go to the game (for example -- --frames 3000).
# Run it from anywhere; it works in the repository it lives in.

param(
    [Parameter(Mandatory = $true)][string]$Rom,
    [string[]]$Symbols = @(),
    [string]$Files = "",
    [Parameter(ValueFromRemainingArguments = $true)][string[]]$GameArgs = @()
)
$ErrorActionPreference = "Stop"

function Check($what) {
    if ($LASTEXITCODE -ne 0) { throw "$what failed (exit code $LASTEXITCODE); see the messages above" }
}

$Rom = (Resolve-Path -LiteralPath $Rom).Path
if (Test-Path -LiteralPath $Rom -PathType Container) {
    $found = Get-ChildItem -LiteralPath $Rom -File | Where-Object { $_.Extension -in ".z64", ".n64", ".v64" } | Select-Object -First 1
    if (-not $found) { throw "no .z64, .n64 or .v64 file in $Rom" }
    $Rom = $found.FullName
}
Write-Host "ROM: $Rom"
# `powershell -File` passes "a","b" as one string "a,b", so split it.
$Symbols = @($Symbols | ForEach-Object { $_ -split "," } | Where-Object { $_ } | ForEach-Object { (Resolve-Path -LiteralPath $_).Path })
$sym = @($Symbols | ForEach-Object { "--symbols"; $_ })
Set-Location (Join-Path $PSScriptRoot "..")
$repo = (Get-Location).Path

Write-Host "== Building the translator"
cargo build --release -p n64rusty; Check "building the translator"
$cli = Join-Path $repo "target\release\n64rusty.exe"

# The per-game folder is named by the ROM's SHA-256; `info` prints it.
$info = & $cli info $Rom; Check "reading the ROM"
$line = $info | Select-String "local folder:\s*(\S+)" | Select-Object -First 1
if (-not $line) { throw "could not find the per-game folder in: $info" }
$local = Join-Path $repo ($line.Matches[0].Groups[1].Value.TrimEnd("/"))
New-Item -ItemType Directory -Force $local | Out-Null

if ($Files) {
    foreach ($n in "config.toml", "dma.txt", "missing.txt", "run.toml") {
        $p = Join-Path $Files $n
        if (Test-Path -LiteralPath $p) { Copy-Item -LiteralPath $p $local -Force; Write-Host "copied $n" }
    }
}

Write-Host "== Translating"
& $cli translate $Rom @sym; Check "translating"

Write-Host "== Building the game (the first time takes several minutes)"
Push-Location (Join-Path $local "out")
cargo build --release; Check "building the game"
$exe = Get-ChildItem "target\release\n64rusty-game-*.exe" | Select-Object -First 1
Pop-Location
if (-not $exe) { throw "the game program was not built" }

# Run a copy, so a later rebuild never fights a running game.
$game = Join-Path $local "game.exe"
Copy-Item $exe.FullName $game -Force

Write-Host "== Playing (close the window to stop)"
Push-Location $local
& $game $Rom @sym --local . --window @GameArgs
$code = $LASTEXITCODE
Pop-Location
exit $code
