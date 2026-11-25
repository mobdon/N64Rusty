# Online play: plan

Status: plan only. Nothing here is built yet. Single-player work comes first;
the steps marked "can start now" do not block it.

## Goal

Two players press "find match", get paired automatically, and play a
two-player match of a fighting game over the internet at original game speed.
The server is the referee: it owns the match clock and the input timeline and
decides who is in sync.

## 1. What the recompiler already gives us

- **Virtual time.** The game's clock (`Ctx::clock`) only moves with the
  emulated console, never with the host clock. Thread switches happen only at
  system calls. So the same inputs should give the same game, frame for frame.
- **Headless runner.** `runner` runs a game with no window, records frames
  and sound, and stops after `--frames n`.
- **Input replay.** `--input script` feeds controller state per video
  retrace. That is exactly the shape netplay needs: "pad state for frame N".
- **Save files.** `--save` loads and writes the cartridge save.

## What is missing

- **A determinism check across machines.** `--hash-out file` writes one RAM
  hash per retrace. First result (Linux, first game, 3000 frames from boot
  through a four-player match with a scripted input file): two runs gave
  identical hashes on every frame. A run with all game code sent to the
  reference interpreter (`--interpret 80000400-80800000`) also matched the
  translated run on every frame, so the translated code computes what the
  console's CPU would. Still to do: the same comparison between a Windows
  and a Linux build, and in the live window, where input arrives from the
  keyboard and the host clock paces frames.
- **A network input source.** Today input comes from a script or the
  keyboard/pad. Netplay needs a source that waits until both players' input
  for frame N has arrived.
- **Full game-state save and restore.** Not possible today in a cheap way:
  guest threads are host threads, so part of the game's state lives on host
  call stacks. RAM plus registers is not enough to restore. This is the main
  reason rollback is not the first choice (see below).

## 2. Design choice

Three options were compared.

| | Server runs the game, streams video | Lockstep, server relays inputs (recommended) | Peer-to-peer rollback |
|---|---|---|---|
| Who runs the game | Server only | Both players | Both players |
| Server needs a ROM | **Yes** | No | No |
| Needs save/restore | No | No | Yes (hard, see above) |
| Server cost per match | 1 CPU core + video encode, 3-5 Mbit/s per player | A few KB/s, no game running | None (plus a small matchmaker) |
| Latency feel | Network round trip plus encode/decode, about 2-4 frames on a good link, video artifacts | Fixed input delay of 2-4 frames on a good link, always crisp | Feels like 0-1 frames, with small "snaps" |
| Cheating | Hard to cheat | Server sees all inputs and compares state hashes; a cheater desyncs and loses the match | Hard to police |

**Recommendation: lockstep with input delay, through our server.**
The server is still the authority for everything that matters: it hands out
frame numbers, collects and orders both players' inputs, sends the same input
stream to both, compares state hashes every few frames, and records the
result. The game itself runs on each player's PC from their own ROM.

Why not "server runs the real game": it needs a ROM on the server and lets a
player play a copy they did not supply (see section 3), and it costs real CPU
and bandwidth per match. Why not rollback first: it needs fast save/restore,
which the thread design does not allow today. Rollback can be added later on
top of the same input protocol.

**How the delay feels.** One frame is 16.7 ms. The input delay is set at match
start from the measured ping, about `ceil(one_way_ms / 16.7) + 1` frames, using
player A to server to player B. Same region (40 ms round trip): 2-3 frames.
Across a continent (80-100 ms): 4-5 frames, noticeable for competitive play,
fine for casual. Bad Wi-Fi spikes cause short stalls instead of
glitches. Picking a server between the two players keeps this low.

## 3. Matchmaking queue and ROMs

**Queue service** (small, game-agnostic):

1. Client connects (WebSocket), sends: ROM sha256, recompiler build id,
   region, and optionally rank.
2. Server pairs the two oldest waiting players with the same ROM hash and build
   id, preferring same region; with ranks on, the allowed rank gap widens the
   longer someone waits.
3. Server measures ping to both, picks the input delay, sends "match start"
   with delay, a seed of zero, and which save data to use.
4. During the match: relay inputs, compare hashes, end on desync or
   disconnect.
5. After the match: store the result; ranks are a simple Elo update
   (optional, later).

One small server ($5-10 a month class) can handle hundreds of matches at
once, since it never runs the game.

**ROMs: the plain rule.** The server must never hold, send or receive ROM bytes.
Each player supplies their own dump of their own cartridge; the client only
sends its sha256 so the server can pair matching copies.
A bare hash proves nothing (known hashes are public), so at connect time the
server also asks for the hash of a random byte range of the ROM, a new range
each time; only a client that has the file can answer. The rule is simple:
you can play as long as you have the ROM. This is also why the
"server runs the game" option is ruled out: it would mean a ROM stored on the
server and a game played by someone who did not supply it, which looks like
distribution. This is a legal risk worth checking with someone qualified
before anything goes public; this document is not legal advice.

**Save data.** Unlocks live in the save file, so both players must start from
the same save or they desync at the select screen. Options: both start with a
blank save, or the host's save (player-made data, 32 KB, not ROM bytes) is
sent to the guest for the match only and thrown away after.

## 4. Steps

| # | Step | General or game-specific |
|---|---|---|
| 1 | Per-frame RAM hash in the headless runner, and a test: same input script twice gives identical hashes (done: `--hash-out`; matches run to run and against the interpreter) | General |
| 2 | Same test across Windows and Linux builds (CI plus mob's PC) | General |
| 3 | Fix whatever step 1-2 finds (host-dependent ordering, float differences, uninitialised memory) | General |
| 4 | Network input source in `runner`: waits for both pads for frame N, with a delay buffer | General |
| 5 | Local two-process test: two headless runners plus a relay on one machine, hashes match | General |
| 6 | Queue and relay server as its own crate/binary | General |
| 7 | Client "find match" option in the window runner, showing ping and delay | General |
| 8 | Save sync at match start | General (the file), game-specific only in which save to choose |
| 9 | Match result: by default players' clients agree via hashes; optional per-game RAM address for the winner, kept in the per-game local folder | Game-specific (optional) |
| 10 | Ranks and Elo | General, optional |
| 11 | Later: save/restore of full game state, then rollback | General, large |

Nothing in steps 1-8 needs to know which game is running: the whole match,
menus included, runs in lockstep from boot.

## 5. What can start now

Steps 1, 2, 3 and 6 can start now without touching single-player work:
the hash test is headless and only reads RAM, and the server is a separate
binary. Step 4 touches the runner's input path, so it should wait until the
controller input work has merged. Steps 1-3 also help single player: they
catch nondeterminism bugs that show up as rare crashes.

## 6. Notes per game

Short notes from each game brought up, so the plan keeps up with what real
games need. Games are described, not named.

### Four-player fighting game (first game)

- **Determinism.** Same input script twice: identical RAM hash on all 3000
  frames, through a four-player match. Translated against interpreted: also
  identical. No host-dependent state found so far.
- **Players.** Up to four pads, so lockstep has to carry four pad states per
  frame, with bots filling empty ports exactly as offline.
- **Timing.** One scene sets the console clock back (`osSetTime`); the
  runner now keeps that as an offset for `osGetTime` only, so retraces and
  timers stay on virtual time for both peers.

### Single-player adventure with compressed files (third game)

- **Save type.** 32 KB battery SRAM, reached through PI DMA on a hand-built
  handle whose base is the KSEG1 address of the SRAM domain. A new file is
  written as soon as it is named, so both players must start from the same
  save, as in section 3.
- **Players.** One player. Online play would mean a shared session or co-op
  built on top, not a versus match; lockstep from boot still applies.
- **Determinism concerns.**
  - Almost all code is unpacked into RAM by the game at run time, and some of
    it is relocated to wherever the heap puts it. Translated functions only
    run while their exact bytes are in RAM, so both peers need the same
    translation build and the same saved images, and the heap must lay out
    the same way on both sides.
  - Decompression runs on the DMA thread, which raises and lowers its own
    priority around each file. Loads finish asynchronously and the game polls
    for them, so load timing must come only from virtual time, never from
    host speed.
  - Pre-rendered room pictures are unpacked by an RSP picture-decode task.
    Its output feeds the frame and the depth buffer, so the decode must be
    bit-identical on every host (it uses floating point today).
- **Input quirks.** The controller manager probes for a controller pak (and
  rumble) every few frames with raw serial transfers. The answers must be the
  same on both sides, so report "no pak" to everyone. A peer's pad reported
  as unplugged changes the game's control flow.
- **Timing.** The game runs at 20 frames a second during play over 60 Hz
  retraces and uses a 3-second timeout timer on graphics tasks. Lockstep
  should count retraces, not game frames. Audio runs a task about every
  retrace from its own thread.

### Single-player rail shooter with a photo check (second game)

- **Save type.** 128 KB flash chip: commands to its register, status and
  id reads, 128-byte page writes and sector erases. Both peers must start
  from the same save file, and a save in progress must finish identically,
  since the game shows an error screen when a status read disagrees.
- **Players.** One player. Online play would mean a shared or co-op session;
  lockstep from boot still applies.
- **Determinism concerns.**
  - The game reads back what the RDP drew: it copies blocks of the frame
    buffer into small images and compares them on the CPU to decide what is
    in a photo, and draws pictures to show inside other pictures. Those
    pixels come from the GPU at the game's resolution, so the GPU render
    must be bit-identical on every host or results (photo scoring, unlocks)
    will drift.
  - Most course code is unpacked into RAM at run time; translations run only
    while their bytes are in RAM, so peers need the same build and the same
    saved images.
  - RSP tasks report done after a fixed virtual delay; the scheduler's
    requests about a running task depend on that timing, so it must come
    from virtual time only.
- **Input quirks.** Aiming is a held button (the camera only pans while it is
  held) and a second button takes the photo; analog stick values matter, so
  peers must share raw stick values, not a direction.
- **Timing.** Gameplay runs at 30 frames a second over 60 Hz retraces; audio
  runs a task every retrace from its own thread. Lockstep should count
  retraces.
