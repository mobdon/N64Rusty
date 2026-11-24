# N64Rusty

A general N64 static recompiler written in Rust. It reads N64 machine
code (MIPS VR4300, big-endian) and writes Rust, which links against a small
runtime crate and builds into a native program. You supply your own legally
obtained ROM; this repository contains no game data.

Status: **step 0, scaffold only.** There is no translation logic yet.

## Layout

| Crate | Path | Role |
|---|---|---|
| `n64rusty-translator` | `crates/translator` | MIPS machine code to Rust source |
| `n64rusty-runtime` | `crates/runtime` | Tiny runtime that generated Rust links against. Kept separate so generated code is not rebuilt when the tool changes |
| `n64rusty` | `crates/cli` | Command-line front end |

```
cargo build --workspace
cargo test --workspace
cargo run -p n64rusty -- version
```

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
