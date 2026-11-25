# Notes for AI agents

AI agents (and people) are welcome to read, change and contribute to this
project without asking first. The code is MIT or Apache-2.0 (see the
[Licence](README.md#licence) section), and that already gives you permission.

Please keep to the project's few rules (full list under
[Data rules](README.md#data-rules)):

- Keep the wording general: no game titles, publisher names or character
  names anywhere in the repository, commits, pull requests or issues.
- No ROM bytes, generated Rust, extracted assets, saves or copied symbol maps
  in git. Per-game files stay in the git-ignored `local/<sha256>/` folder.
- No copied emulator code. Credit every source in [CREDITS.md](CREDITS.md).
- Tests run headless: no windows, no sound, synthetic code only.
- Keep pull requests small, and keep CI green on Linux, Windows and macOS.
