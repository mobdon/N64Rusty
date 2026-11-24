# N64Rusty

A general N64 static recompiler written in Rust. It reads N64 machine
code (MIPS VR4300, big-endian) and writes Rust, which links against a small
runtime crate and builds into a native program. You supply your own legally
obtained ROM; this repository contains no game data.

Status: **step 1, CPU translator.** The VR4300 integer instruction set
translates to Rust, with branch delay slots, and is tested per instruction
against a reference interpreter. Floating point, ROM loading and the rest of
the console come next.

## Layout

| Crate | Path | Role |
|---|---|---|
| `n64rusty-translator` | `crates/translator` | MIPS machine code to Rust source |
| `n64rusty-runtime` | `crates/runtime` | Tiny runtime that generated Rust links against. Kept separate so generated code is not rebuilt when the tool changes |
| `n64rusty` | `crates/cli` | Command-line front end |
| `n64rusty-refcpu` | `crates/refcpu` | Reference VR4300 interpreter, used only by tests |
| `n64rusty-conformance` | `crates/conformance` | Per-instruction tests: translated code against the reference CPU |

```
cargo build --workspace
cargo test --workspace
cargo run -p n64rusty -- version
```

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
- **Not modelled yet.** Overflow exceptions (ADD, ADDI, SUB and their
  64-bit forms behave like the unsigned forms), traps, LL/SC, the TLB, and
  coprocessor instructions. These emit a call that panics with the address,
  so the first one a game reaches is easy to find.

### Tests

`crates/conformance` writes several hundred tiny programs by hand with a
built-in encoder (one or a few instructions each, plus branch, loop, call and
tail-call shapes), translates them at build time, and runs each 64 times from
random register and memory states. The translated code and the reference CPU
must agree on every register, HI, LO and every byte of memory. The reference
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
