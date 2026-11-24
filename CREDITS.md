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

## Adding a source

Every new source gets an entry here before its influence lands: name, link,
licence, what it was used for, and the full licence text when the licence asks
for it. Emulators may be read as a reference for hardware behaviour only.
