# xtermcheck — the gated VS Code (xterm.js) oracle leg

`xtermcheck` is the **out-of-process VS Code oracle leg** of the `squeezy-tui`
term-matrix (`crates/squeezy-tui/src/termsim/`). The in-process Rust legs replay
a captured ANSI stream through `vt100` and `alacritty_terminal`; this leg replays
the *same* stream through [`@xterm/headless`](https://www.npmjs.com/package/@xterm/headless),
the exact terminal engine VS Code's integrated terminal (and anything else built
on xterm.js) uses.

It exists to catch the one regression those well-behaved Rust emulators cannot
see: under xterm.js's cursor/reflow behavior, the append-only renderer can leave
**more than one `╰─☽ … ─────` divider** painted in the live viewport at once. The
moon-crescent (`☽`) divider closing a turn must appear **at most once**; two or
more means stale dividers stacked. `xtermcheck` exits non-zero when that happens.

## Why it is gated

xterm.js is a Node/npm dependency, so this leg is **opt-in**: the matrix runs it
only when `node` is present (and `@xterm/headless` is installed). On machines
without Node — and in the default `cargo test` path — the Rust legs run alone and
this oracle is skipped. Treat a missing `node` as "oracle not run", not as a
pass; CI that wants the VS Code guarantee must provision Node and install deps.

## Setup

```sh
cd crates/squeezy-tui/tools/termsim/xtermcheck
npm install        # pulls @xterm/headless
```

## Usage

```sh
node replay.js path/to/capturelog.json
```

Exit codes:

| code | meaning                                              |
|------|------------------------------------------------------|
| `0`  | OK — at most one `☽` divider in the final viewport   |
| `1`  | FAIL — divider **stacked** (2+ visible)              |
| `2`  | usage / bad input (missing file, malformed JSON, …) |

Sample run:

```sh
$ node replay.js fixtures/example-capturelog.json
xtermcheck: replayed 3 frame(s), 412 byte(s); final viewport 80x24; 1 ☽ divider line(s) in viewport
xtermcheck: OK — no divider stacking
```

## CaptureLog JSON contract

The input mirrors `CaptureLog` / `FrameMark` in
`crates/squeezy-tui/src/termsim/types.rs`:

```json
{
  "bytes_base64": "G1s/MjAyNmgbWzJK...",
  "frames": [
    { "byte_offset": 137, "w": 80, "h": 24 },
    { "byte_offset": 290, "w": 80, "h": 24 },
    { "byte_offset": 412, "w": 100, "h": 30 }
  ]
}
```

- `bytes_base64` — the whole append-only ANSI byte stream, base64-encoded.
  Alternatively supply `bytes_hex` (a hex string; whitespace ignored). Exactly
  one of the two is required.
- `frames` — one mark per painted frame, in paint order. Frame *i*'s bytes are
  `bytes[frames[i-1].byte_offset .. frames[i].byte_offset]` (frame 0 starts at
  offset 0), so the log is self-slicing. `w`/`h` are the terminal columns/rows
  (the `FixedSize`) in effect for that paint.

The replay seeds the terminal at frame 0's size, then for each frame calls
`term.resize(w, h)` **before** writing that frame's byte slice — reproducing the
per-frame resize the harness drove. After the last frame it reads the live
viewport (`buffer.active`, anchored at `baseY`) and counts rows matching
`/☽[^\n]*?[─╌┈]/u`.

## Exporting a CaptureLog from the Rust harness

The Rust side already produces a `CaptureLog` (`bytes: Vec<u8>` + `frames:
Vec<FrameMark>`) in `crates/squeezy-tui/src/termsim/`. To feed it here, serialize
it to the JSON shape above. A minimal exporter (no new public types needed):

```rust
// In a #[cfg(test)] helper or a small bin, given a `CaptureLog` `log`:
use base64::Engine as _;

let json = serde_json::json!({
    "bytes_base64": base64::engine::general_purpose::STANDARD.encode(&log.bytes),
    "frames": log.frames.iter().map(|f| serde_json::json!({
        "byte_offset": f.byte_offset,
        "w": f.w,
        "h": f.h,
    })).collect::<Vec<_>>(),
});
std::fs::write("capturelog.json", serde_json::to_vec_pretty(&json)?)?;
```

(If you prefer zero base64 dependency, emit `bytes_hex` instead:
`log.bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()`.)

Then the matrix's gated step is simply:

```sh
command -v node >/dev/null 2>&1 \
  && node crates/squeezy-tui/tools/termsim/xtermcheck/replay.js capturelog.json
```

a non-zero exit fails the VS Code oracle leg.
