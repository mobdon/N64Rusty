# N64Rusty

A general N64 static recompiler written in Rust. It reads N64 machine
code (MIPS VR4300, big-endian) and writes Rust, which links against a small
runtime crate and builds into a native program. You supply your own legally
obtained ROM; this repository contains no game data.

Status: **step 2, ROM loader.** The VR4300 integer and floating-point
instruction sets translate to Rust, with branch delay slots, and are tested
per instruction against a reference interpreter. ROMs load in any byte order, and the boot
segment's functions are found from the entry point and written out as a Rust
crate that compiles. Floating point and the rest of the console come next.

## Layout

| Crate | Path | Role |
|---|---|---|
| `n64rusty-translator` | `crates/translator` | MIPS machine code to Rust source |
| `n64rusty-runtime` | `crates/runtime` | Tiny runtime that generated Rust links against. Kept separate so generated code is not rebuilt when the tool changes |
| `n64rusty` | `crates/cli` | Command-line front end |
| `n64rusty-rom` | `crates/rom` | ROM loading: byte order, SHA-256, header, boot code variant |
| `n64rusty-refcpu` | `crates/refcpu` | Reference VR4300 interpreter, used only by tests |
| `n64rusty-conformance` | `crates/conformance` | Per-instruction tests: translated code against the reference CPU |
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
git-ignored `local/<sha256>/` folder: `out/` (the generated crate, one file
per 64 KiB of address space, rewritten only when its contents change),
`report.txt` and a `config.toml` template.

On Linux, building needs `libudev-dev` (for gamepads).

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
  port. Gamepads: south face button A, west B, left trigger Z, shoulders L and
  R, right stick the C buttons.
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

Modelled on [N64Recomp](https://github.com/N64Recomp/N64Recomp) (MIT). See
[CREDITS.md](CREDITS.md) and [NOTICE](NOTICE).
