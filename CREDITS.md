# Credits

N64Rusty is original code. It credits every project whose design or
documentation it learns from. Nothing here is copied from an emulator, and no
GPL-licensed code or GPL-derived tables are included.

## N64Recomp

- Project: https://github.com/N64Recomp/N64Recomp
- Licence: MIT
- Used for: the overall approach of statically recompiling N64 MIPS code into
  a native language, with a small runtime and per-function overrides.
  N64Rusty is a separate implementation in Rust that writes Rust.

Licence text, reproduced as required:

```
The MIT License (MIT)

Copyright (c) 2024 Wiseguy

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
THE SOFTWARE.
```

## Hardware documentation

Read for instruction behaviour; no text or tables are copied.

- NEC VR4300 64-bit microprocessor user's manual: instruction semantics,
  sign extension of 32-bit results, multiply and divide results, unaligned
  loads and stores.
- MIPS III / MIPS IV instruction set references: encodings and branch delay
  slot rules, including branch-likely behaviour.
- N64brew wiki (https://n64brew.dev), Audio Interface and RSP pages: how
  audio tasks, the AI frequency and AI buffers work.
- The VADPCM sample format and the audio microcode's command encodings and
  flags as described in public N64 development documentation and the
  decompiled system library headers (https://github.com/decompals/ultralib).
  `crates/audio` is written from those descriptions; no microcode, tables or
  code are copied. The microcode's resampling filter table is not
  reproduced (the host uses its own Catmull-Rom interpolation).

## Dependencies

- cpal (https://github.com/RustAudio/cpal), Apache-2.0: speaker output in
  `crates/audio`, only with its `speaker` feature.

## Adding a source

Every new source gets an entry here before its influence lands: name, link,
licence, what it was used for, and the full licence text when the licence asks
for it. Emulators may be read as a reference for hardware behaviour only.

## Host crates (crates/host)

Used as ordinary dependencies, not copied:

- winit (https://github.com/rust-windowing/winit), Apache-2.0: windows and keyboard input.
- gilrs (https://gitlab.com/gilrs-project/gilrs), MIT OR Apache-2.0: gamepads.
- serde (https://serde.rs) and toml (https://github.com/toml-rs/toml), MIT OR Apache-2.0: the run config file.

The controller's button word layout follows the public documentation on the
N64brew wiki (https://n64brew.dev), on the controller protocol. The keyboard and
gamepad layouts, script format and debug port are our own.
