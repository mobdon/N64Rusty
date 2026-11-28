# Pitfalls

Things that have caught this project out, kept short so they are read before
the next change. The known limits are marked `shortcut:` in the code; the
list below picks the ones most likely to bite and says where they live.

## Guest data and game memory

- **Guest data must never panic the player.** Command lists, display lists,
  DMA lengths and RAM fields come from the game and can hold anything. Wrap
  16-bit DMEM indexes (`wrapping_add`, then mask) instead of adding plainly,
  widen before multiplying (`-32768 * -32768 * 2` overflows `i32`), and skip
  an unknown command with one warning instead of asserting.
- **Retail and debug struct layouts differ.** Structure layouts published
  for a debug build of a game can sit a fixed number of bytes away from the
  retail build's for every field after a shared base part. Check the offset
  of one known field in the retail ROM's memory before trusting a whole
  layout.
- **A new player object per scene resets per-object state.** A game can
  free and allocate its player object on every scene change. State a mod
  keeps keyed by that object's address, or stored inside it, is lost or
  stale after each transition; key it by something that survives, or reset
  it on purpose.
- **Physical, KSEG0 and KSEG1 addresses are the same RAM.** Mask with
  `0x1FFF_FFFF` before indexing; a TLB-mapped address is not covered by that
  mask and needs the page map.

## Mods

- **Hook chaining order: the last loaded runs first.** Hooks on one address
  chain in load order; the last mod loaded runs first, and its
  `call_original` reaches the hook loaded before it, then the game's own
  function. A hook that does not call the original ends the chain.
- **The mod version must match exactly.** The player loads a mod only when
  its `n64rusty_mod_version` returns the player's `VERSION`. An appended
  `Api` field still bumps the version, so rebuild every mod after a bump.
- **Handoff data is capped** at `HANDOFF_MAX` bytes by `Api::switch_game`.
  The raw `switch_game` field does not check it.

## Files and the checkout

- **`unwrap_or_default()` on a read that is then written back wipes
  files.** Any read error (locked, permission denied, bad UTF-8) becomes an
  empty document, and the write-back replaces the user's file with only the
  keys being changed. Treat only `NotFound` as empty; on any other error,
  leave the file alone and report it.
- **LF and CRLF are mixed in this checkout.** Most files are CRLF, some are
  LF. Keep each file's line endings when editing; a tool that rewrites them
  turns a one-line change into a whole-file diff.
- **Other sessions share the checkout.** Several sessions can edit the same
  working tree on `main` at once. Never run `cargo fmt --all`, and never
  discard, stash or reset files you did not edit. Run `rustfmt` only on a
  file you changed, and only if it was already rustfmt-clean.

## Known limits (`shortcut:` in the code)

- Runtime: TLB address space ids are ignored; mixed-size TLB pages that
  partly overlap can disagree; a translated function must lie in pages
  mapped in order; code the CPU rewrites without a cache operation keeps
  running the old translation.
- Runner: code that runs on into a translated function without a jump stays
  in the interpreter; RSP tasks all take one fixed time; either interrupt
  line lets every event through; the rumble motor is not passed to the host.
- Graphics: G_BRANCH_Z always takes the branch; only one blend formula is
  drawn blended; draws into other colour images are skipped; no LOD fraction
  (no mipmapping); raw TMEM bank tricks decode wrongly; big tiles lose the
  far-edge clamp; matrices are blended element by element.
- Audio: the resampler is close to the microcode's but not bit-exact; the
  third command set's effect filter is left out; a WAV recording keeps its
  first sample rate.
- Host: diagonal keyboard stick input reaches the square's corner; Android
  assumes 120 Hz.
