# Holodori Phone Trackpad: product and engineering contract

This repository is an unofficial companion for **hololive Dreams (holodori)**.
Treat it as a latency-sensitive rhythm-game input device, not as a generic
trackpad, remote-control demo, or lossy event stream.

## Why this app exists

QualiArts and hololive describe holodori as a Rhythm & RPG whose rhythm game is
the core experience. The official Steam page presents a full rhythm game with
hard charts and player-created charts, alongside the park and mini-games.

The mobile charts use spatial touch gestures. PC community feedback consistently
describes the Steam keyboard adaptation as much flatter: wide key zones let a
small number of keys cover much of the board, flicks behave like taps, and a
moving hold can often be satisfied by holding one stationary key. This project
exists so a player can use an Android phone as the missing multi-touch surface
while running the PC version.

The default keyboard bridge maps the phone surface to `S D F J K L`. Its job is
not merely to emit the key under the latest coordinate. It must preserve the
physical play vocabulary:

- a tap is a timed down/up;
- a hold is continuous asserted state for as long as the finger remains down;
- a slide is an ordered path through every crossed lane;
- a chord is simultaneous state with independent finger ownership;
- a lift or cancellation must release the corresponding Windows state.

## Non-negotiable gameplay invariants

1. **A stationary hold is active input, not idleness.** Do not use lack of
   `MOVE` callbacks as a disconnect signal. Full-state heartbeats sustain it.
2. **A slide is history, not a latest-value update.** Never coalesce gameplay
   frames to the newest coordinate. Copy Android `MotionEvent` history in time
   order and apply every sequence in order.
3. **Crossed lanes cannot disappear.** If one reported coordinate jumps over
   lanes, walk every intermediate lane. Press the next lane before releasing
   the previous one so a slide has no no-key gap.
4. **Multi-touch ownership is reference-counted.** One finger moving or lifting
   cannot release a lane still held by another finger.
5. **An ACK means durable input progress.** Windows may acknowledge a sequence
   only after the chosen OS sink accepted it. Parsing or buffering is not
   success.
6. **Liveness means committed sequence progress.** Duplicate ACKs, discovery
   packets, malformed frames, and valid future frames behind an ordering hole
   prove traffic, not progress. They must not keep stale input alive forever.
7. **Failure is a clean boundary.** Release all injected Windows input, reject
   delayed gameplay from the failed session, require a fresh session-start
   `CANCEL`, and then reconstruct any still-held phone contacts from the latest
   complete snapshot.
8. **Never replay seconds-old gameplay.** Reliability repairs short packet loss;
   it must not turn an outage into late notes after reconnection.

## Latency contract

The live target is one 120 Hz frame: **8.333 ms**. This is a design budget, not
a universal hardware guarantee. Phone touch scan rate, Android scheduling, the
USB controller, RNDIS, Windows scheduling, and the game all contribute.

Changes must preserve these properties:

- no intentional batching, debounce, frame-age wait, or polling bridge;
- no UI rendering, logging, report formatting, sorting, or file I/O on the hot
  path;
- no Python, JSON, or per-frame process boundary in the native path;
- first send is immediate, its redundant copy is immediate, and repair begins
  after 2 ms;
- every datagram remains below the tethered Ethernet MTU;
- watchdog and recovery logic observes state without delaying transmission;
- metrics remain bounded in memory and are written only when play stops.

When changing transport or input code, measure the current-event path and the
two-copy-loss recovery path. A healthy one-copy or redundant-copy delivery must
remain below 8.333 ms; loss of both immediate copies must still repair within
that frame through the 2 ms replay.

## Why the architecture looks this way

- **USB tethering/RNDIS:** uses the phone's normal USB cable and Windows inbox
  networking. It avoids USB debugging, ADB, root, Android accessory mode,
  WinUSB, UsbDk, and custom driver installation.
- **UDP datagrams:** provide low-overhead atomic framing. Protocol v4 adds the
  reliability UDP does not: session IDs, sequence numbers, CRC, reorder,
  deduplication, cumulative ACKs, redundant sends, and replay.
- **Complete contact snapshots:** make multi-touch state unambiguous and allow
  a fresh host to reconstruct a hold without replaying old transitions.
- **Rust native host:** keeps parsing, ordering, metrics, and User32 submission
  deterministic and outside the launcher/UI runtime.
- **ACK after the OS sink:** makes the retained Android queue an end-to-end
  delivery guarantee rather than a socket-delivery guarantee.
- **Two Windows sinks:** touch mode uses sanctioned Windows Touch injection;
  keys mode supplies the six-lane bridge and explicit slide interpolation.
- **Small active watchdogs:** a rhythm game cannot tolerate a key remaining
  held for seconds after link failure. The watchdogs watch committed progress,
  while idle discovery is deliberately more patient.
- **Fresh-session `CANCEL`:** prevents delayed packets from resurrecting notes
  after an outage.

## Scope and safety boundaries

This tool sends ordinary Windows input. Do not open or modify the game process,
read game memory, hook rendering or input code, synthesize game-network
protocols, bypass anti-cheat, or claim injected touch is indistinguishable from
physical hardware. Keep the app clearly labeled unofficial and subject to the
game's terms and competitive rules.

Preserve unrelated user changes and ignored release artifacts. Do not rewrite a
published tag unless the user explicitly authorizes it.

## Required validation for transport changes

- `cargo test --all-targets`
- `cargo clippy --all-targets -- -D warnings`
- optimized native build
- Android debug and release builds plus debug/release lint
- launcher frontend tests/build and strict Rust checks when launcher code or a
  release bundle is involved
- legacy Python suite when producing a full release
- `git diff --check`
- loopback fault injection for corrupt/lost copies and loss of both immediate
  copies, checked against the 8.333 ms budget
- real phone/cable/PC soak before claiming universal physical latency

## Research basis

Reviewed 2026-08-08:

- [Official hololive Dreams site](https://www.hololive-dreams.com/en) - Rhythm &
  RPG, supported on iOS, Android, and Steam.
- [Official hololive launch announcement](https://hololive.hololivepro.com/en/news/20260723-01-401/)
  - rhythm gameplay is the core of the theme-park progression.
- [Official Steam store page](https://store.steampowered.com/app/4282500/hololive_Dreams/)
  - full rhythm mode, difficult charts, chart creation, and 150+ launch songs.
- [Steam community page](https://steamcommunity.com/app/4282500) and
  [Steam user reviews](https://steamcommunity.com/app/4282500/reviews/) -
  player reports about broad keyboard zones and the loss of meaningful flick
  and moving-slide interaction on PC. Treat these control observations as
  community evidence, not an official specification.

The repository's own normative specifications remain
`EXPERIMENTAL_ARCHITECTURE.md` and `PROTOCOL_V4.md`.
