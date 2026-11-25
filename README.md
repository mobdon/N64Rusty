# N64Rusty

A general N64 static recompiler written in Rust. It reads N64 machine
code (MIPS VR4300, big-endian) and writes Rust, which links against a small
runtime crate and builds into a native program. You supply your own legally
obtained ROM; this repository contains no game data.

Status: **step 3, headless runs.** The VR4300 integer and floating-point
instruction sets translate to Rust, with branch delay slots, and are tested
per instruction against a reference interpreter. ROMs load in any byte order;
the boot segment and the code the game loads at run time are found and written
out as a Rust crate that builds into a native program. That program runs the
game headless with the system library replaced by native Rust, its display
lists drawn off screen and its audio command lists mixed, until the first
routine that is still missing.

## Layout

| Crate | Path | Role |
|---|---|---|
| `n64rusty-translator` | `crates/translator` | MIPS machine code to Rust source |
| `n64rusty-runtime` | `crates/runtime` | Tiny runtime that generated Rust links against. Kept separate so generated code is not rebuilt when the tool changes |
| `n64rusty` | `crates/cli` | Command-line front end |
| `n64rusty-rom` | `crates/rom` | ROM loading: byte order, SHA-256, header, boot code variant |
| `n64rusty-refcpu` | `crates/refcpu` | Reference VR4300 interpreter, used only by tests |
| `n64rusty-conformance` | `crates/conformance` | Per-instruction tests: translated code against the reference CPU |
| `n64rusty-runner` | `crates/runner` | Runs a recompiled game: boot, threads, messages, timers, DMA and the system library replaced in Rust; devices plug in through one trait |
| `n64rusty-gfx` | `crates/gfx` | High-level display-list interpreter and headless wgpu renderer with PNG capture |
| `n64rusty-host` | `crates/host` | Window, frame pacing, controllers, input record and replay, per-run config, debug port |
| `n64rusty-audio` | `crates/audio` | Host audio: runs the game's audio command lists, plays the result to a WAV file or the speakers |

```
cargo build --workspace
cargo test --workspace
cargo run -p n64rusty -- version
```

With your own ROM:

```
cargo run -p n64rusty -- info path/to/your.z64
cargo run -p n64rusty -- translate path/to/your.z64 [--symbols local-map.txt]
cd local/<sha256>/out && cargo build
```

`translate` finds functions by following calls and branches from the entry
point through the boot segment (the first 1 MiB the boot code copies to
RAM), resolves jump tables and jumps to constant addresses, and also takes
function pointers built with `lui`/`addiu` that point at a function prologue.
`--symbols` adds function addresses from a symbol map (lines like
`name = 0x80001234;`) read from your own local checkout of a decompilation
project; it is never copied into this repository. Output goes to the
git-ignored `local/<sha256>/` folder: `out/` (the generated game: one small
crate per 64 KiB of address space and per overlay under `out/parts/`, each
rewritten only when its contents change, so adding a function rebuilds one
part and relinks; formatting is turned off there), `report.txt` and a
`config.toml` template. Keep `out/target/`: it is the build cache, and
cleaning it means a full rebuild.

On Linux, building needs `libudev-dev` (for gamepads) and `libasound2-dev`
(for sound).

## Running headless

The generated crate builds a program that runs the game with no window and
no sound:

```
cd local/<sha256>/out && cargo build
./target/debug/n64rusty-game-<hash> path/to/your.z64 [--symbols local-map.txt] [--frames 600]
```

It replaces the system library (threads, message queues, timers, video
retrace, cartridge DMA, controllers, task and audio submission) by role
name. The addresses of those routines come from `--symbols` or from
`local/<sha256>/config.toml` (`name = 0xADDR` lines); neither is ever
committed. Guest threads run one at a time on host threads, in priority
order, on a virtual clock: retrace and timer interrupts arrive on time
even while a thread is busy, and when every thread waits, time jumps to the
next event. The game always runs at its original speed in game time.

A run stops after `--frames` retraces, on a deadlock, at the first
panic, or on a hang (no retrace for 5 seconds of host time: a guest loop
that makes no calls, so game time cannot move), and prints the threads,
the cartridge copies and the most recent calls. Any access past RAM that is
not through the KSEG0/KSEG1 windows (a wild pointer, or TLB-mapped memory,
which is not modelled yet) panics there; an address starting `0x27BD` is
also named as code read as a pointer. It also writes two files next to `out/`:

- `dma.txt`: every cartridge copy seen (ROM offset, RAM address, length).
- `missing.txt`: each call into code that was never translated, with the
  copy that put it there.

Run `translate` again and it adds that code: code copied in at run time
becomes an overlay (its own module, active while that copy is in RAM; a
later copy into part of it hides only the functions it covers), boot
segment code that a later copy overwrites is dropped, and boot code reached
only through a pointer is added. Repeat until the run reaches its frame
limit.

`translate` also reads the game's own load tables before any run: a pair
of ROM offsets (start, end) followed within three words by a RAM address,
where the code there calls into itself at function starts (or, with no
such calls, builds addresses inside itself). Each such piece becomes an
overlay up front, so modes no run has reached yet are already translated.
A copy seen in a run replaces the guess.

Graphics, audio and input reach the game through
`n64rusty_runner::devices::Devices` (RSP tasks, audio buffers, frame swaps,
retrace and controller state). The default runs every graphics task through
`n64rusty-gfx` and every audio task through `n64rusty-audio`, and reports
one connected controller with nothing pressed. Nothing is shown or played:

```
./target/debug/n64rusty-game-<hash> your.z64 --capture frames/ --capture-every 30 --wav run.wav
```

saves every 30th finished frame as a PNG and records the sound.
`--capture-scale 4` renders the saved frames at four times the game's
resolution (the geometry is drawn again at that size, not stretched). Captures
and recordings are game output: keep them in the git-ignored local folder.
The audio command set follows the game's audio library: when the symbol map
names the alternative library's routines (`n_alSynNew`, `n_alAudioFrame`),
its command set is used. `--input script.txt` plays a controller script
(`frame port buttons x y` lines, the format `n64rusty-host` records),
matched against retrace numbers, so a headless run can press buttons.

Cartridge save memory (battery-backed SRAM, reached with PI DMA) is kept
in a file: `--save file`, and window runs default to `save.sra` in the
per-game folder. The file is read at start and rewritten after every write
the game makes, so closing the window or a crash loses nothing. Headless
runs without `--save` start from blank save memory and keep nothing, so
repeated runs stay identical. EEPROM, flash and controller pak saves are
not done yet.

Guest threads start with the floating-point register mode the system
library gives them (32-bit registers, doubles in even/odd pairs); the boot
code keeps the 64-bit mode the boot ROM sets.

## Playing in a window

Add `--window` to the game program and it plays live: a window that draws
each finished frame at the window's own resolution (keeping the 4:3 shape,
with black bars), sound on the default speakers, and input from the keyboard
and gamepads as described below. Frames run at the game's own rate (59.94
per second); the game never runs faster, and if the computer falls behind,
the game slows down. There is no frame limit unless `--frames` is given.
Window size, frame rate, key layout, gamepad ports and input record/replay
come from `run.toml` in the per-game folder, or `--run file`.

`widescreen = true` under `[window]` in `run.toml` fills a window wider
than 4:3 instead of drawing black bars. The 3D scene shows more at the
sides, everything keeps its shape with the 4:3 picture in the middle, and
full-width backgrounds stretch to the edges. Things the game itself leaves
out of view (objects it culls past the original edge) stay missing.
`--capture-wide` saves headless captures in 16:9 the same way.

`refresh = 144` (any rate above the game's) under `[window]` draws the
window that often. Each picture between two game frames blends where
every 3D object is, matched by the display list code that drew it, so
motion is smoother while the game keeps running at its own rate. What you
see trails the game by one frame, and 2D elements move only on game frames.
`--capture-between` also saves, with each headless capture, the picture
half-way from the frame before.

On Windows, one script does it all: it builds the translator, translates
your ROM, builds the game program, copies it to `local\<sha256>\game.exe` and
starts it with `--window`. From the repository folder in PowerShell:

```
powershell -ExecutionPolicy Bypass -File scripts\play.ps1 -Rom C:\path\to\your.z64 -Symbols C:\path\to\map.txt -Files C:\path\to\per-game-files
```

`-Rom` can also be the folder that holds the ROM. `-Files` is optional: a
folder with per-game run files (`config.toml`, `dma.txt`, `missing.txt`,
`run.toml`) to copy into `local\<sha256>\` before translating. The script
stops at the first step that fails and says which.

The game runs from a copy, so a rebuild never fights a running game. Close
the window to stop. `record = "take.txt"` in `run.toml`
keeps your input; `replay = "take.txt"` plays it back. With `headless = true`
in the run config nothing is opened and the run goes as fast as it can with
scripted input, which is how this mode is tested.

## Window and input

`crates/host` runs the frame loop around a game. A game plugs in through the
`Guest` trait: it gets each frame's four controller states, and once the
window exists, the window itself (so a renderer can make a wgpu surface on
it).

- **One config file per run.** Everything that decides how a run goes
  (frame limit, frame rate, replay and record files, debug port, window size,
  where each port's input comes from, key layout) is in one TOML file, so a run
  can be repeated from that file. Unknown keys are errors. See
  `crates/host/run.example.toml`.
- **Controllers.** Four ports. By default port 1 is the keyboard plus the
  first gamepad, and ports 2 to 4 are the next gamepads. Keyboard: WASD moves
  the stick (left Shift for half tilt), J is A, K is B, Space is Z, Enter is
  Start, Q and E are L and R, T F G H (or numpad 8 4 5 6) are the C buttons
  and 1 to 4 the D-pad. The arrow keys are not used. Keys can be remapped per
  port. Gamepads (Xbox layout): A is A, B is B, X and Y are C-down and C-up,
  LT and RB are Z, LB is L, RT is R, Start is Start, the left stick is the
  stick, the right stick the C buttons and the d-pad the d-pad. Each port's
  pad buttons, stick deadzone and sensitivity can be changed in `run.toml`,
  and controllers can be plugged in while the game runs. Each one is named on
  the console when found (`controller gamepad0: ...`).
- **Record and replay.** Input scripts are plain text anchored on frame
  numbers (`frame port buttons x y`, one change per line), so they can be
  written by hand as well as recorded. A recording is written as the run goes,
  so the input that led to a crash is kept.
- **Debug port.** A loopback-only TCP port with one-line commands (`step`,
  `frame`, `pad`, `release`, `run`, `quit`) lets a test or tool drive the game
  one frame at a time.
- **Pacing.** Windowed runs start frames on fixed deadlines at the configured
  rate (NTSC 59.94 by default). If the host falls behind, the game slows
  down; it never runs fast to catch up. Headless runs are unpaced.

Tests use the headless path only. To try the window and your controllers by
hand: `cargo run -p n64rusty-host --example window [run.toml]`.

## Graphics

`n64rusty-gfx` does not run the graphics microcode. It reads the display
lists a game builds in guest RAM (F3DEX2 encoding), does the vertex work
itself (matrix stack, lighting, texture coordinates, texture memory and
texel formats), and records triangles with their colour-combiner and
render-mode state. A wgpu renderer then draws each frame offscreen, with a
fragment shader generated per combiner setting, and can save chosen frames
as PNG files. Nothing opens a window.

Vertices stay in object space with their model-view, projection and
viewport kept apart until drawing, so widescreen and in-between frames for
high refresh rates can be added later without changing game speed.

```
cargo run -p n64rusty-gfx --example synthetic -- out.png
```

draws a hand-built display list. Tests use only synthetic lists.

## How translation works

Each guest function becomes one Rust function, `fn(&mut Ctx)`, whose body is
`loop { match pc { ... } }` over its basic blocks. Registers are 64-bit and
every 32-bit result is sign-extended, as on the VR4300.

- **Delay slots.** A branch's condition (or a register jump's target) is read
  first, then the link register is written, then the delay-slot instruction
  runs, and only then does control move. Likely branches run the delay slot
  only when taken. A branch target that is another branch's delay slot gets
  its own block.
- **Calls.** Every call goes through `Ctx::call(addr)`, the single dispatch
  point, so any guest function can be overridden by address with
  `Ctx::set_override`. `jr $ra` returns; jumps and branches that leave the
  function are tail calls.
- **Floating point.** All COP1 moves, loads, stores, arithmetic,
  conversions, compares and branches, in both register layouts (the Status
  FR bit, on by default as the boot code leaves it). Arithmetic rounds to
  nearest; float-to-integer conversions honour the FCR31 rounding mode.
  FPU exceptions and cause/flag bits are not modelled; an invalid conversion
  gives the largest positive integer, the MIPS default result.
- **Not modelled yet.** COP0 (system control), overflow exceptions (ADD, ADDI, SUB and their
  64-bit forms behave like the unsigned forms), traps (BREAK, SYSCALL), LL/SC and
  the TLB. These emit a call that panics with the address,
  so the first one a game reaches is easy to find.

### Audio

`crates/audio` does on the host what the RSP audio microcode and the audio
interface do on the console. Its interface is three calls for the system
library overrides to make:

- `AudioRsp::run(ram, list, len)` when the game starts an audio task: runs
  the command list (ADPCM decode, resample, volume envelopes, mix,
  interleave) against guest RAM.
  `AudioRsp::new()` handles the standard audio library's commands and
  `AudioRsp::n_audio()` the alternative `n_audio` library's, which adds a
  pole filter.
- `AudioOut::set_frequency(hz)` and `AudioOut::play(ram, addr, len)` when the
  game sets the AI frequency and queues the next AI buffer.

Output goes to a WAV file (`record_wav`) and, with the `speaker` feature,
to the default output device (`open_speaker`, through cpal; on Linux this
needs the ALSA development package, `libasound2-dev`). `set_muted` silences
the speakers while WAV recording carries on. Playback runs at the rate the
game sets. Tests use synthetic command lists and only write WAV data.

### Tests

`crates/conformance` writes several hundred tiny programs by hand with a
built-in encoder (one or a few instructions each, plus branch, loop, call and
tail-call shapes), translates them at build time, and runs each 64 times from
random register, float register, FCR31, FR-mode and memory states. The
translated code and the reference CPU must agree on every register, HI, LO,
float register, FCR31 and every byte of memory. The reference
CPU is a separate implementation with its own decoder and byte-by-byte
unaligned accesses, so a mistake in one shows up as a mismatch. No game code
or data is used.

## Data rules

These apply to every commit, pull request, issue and commit message.

- **No game data in git.** No ROM bytes, no generated Rust, no extracted
  assets, no built games, no saves. `.gitignore` blocks the usual paths and
  extensions; do not force-add around it.
- **General wording only.** The project is described as a general N64
  recompiler. No game titles, publisher names or character names anywhere in
  the repository or its history.
- **No copied symbol maps.** Function and symbol maps from a decompilation
  project are read from your own local checkout at build time, never copied in.
- **No emulator code.** Emulators are a reference for hardware behaviour only:
  no copied code and no GPL-derived tables.
- **Credit every source** in [CREDITS.md](CREDITS.md), with licence text.
- **Native code, original speed.** The recompiled game runs as native code at
  its original game speed; smoothness is added in the renderer, never by
  changing game timing.
- **Headless tests.** Tests never open a window, take focus or play sound, and
  use synthetic code only, never game bytes.

## Per-game files

Each game is identified by the SHA-256 hash of its ROM after it has been
normalised to big-endian (`.z64`) byte order, so all three common dumps of the
same ROM (`.z64`, `.v64`, `.n64`) get the same name. Everything for that game
lives under a local, git-ignored folder named by that hash:

```
local/<sha256>/          # git-ignored
    config.toml          # hook addresses with role names, found by us
    out/                 # generated Rust
    dma.txt, missing.txt # what headless runs saw, read by translate
    report.txt           # summary of the last translate
    ...                  # saves, captures, extracted data
```

The only per-game data that may ever be committed is a short file of hook
addresses with role names that we found ourselves, named by the same hash.

## Licence

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT licence ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you state otherwise, any contribution you submit for
inclusion is dual licensed as above, without any additional terms.

Only contributors approved by the owner may work on this repository, and AI
agents need the owner's explicit permission; see [AGENTS.md](AGENTS.md).

Modelled on [N64Recomp](https://github.com/N64Recomp/N64Recomp) (MIT). See
[CREDITS.md](CREDITS.md) and [NOTICE](NOTICE).
