# gmrecomp

Runs Windows GameMaker games (GameMaker 8.0/8.1, GameMaker: Studio 1.4 VM and YYC) in
desktop browsers by recompiling their original executables. The x86 code of the
runner, the game (YYC) and every DLL the game ships is translated to WebAssembly; the
Windows APIs it calls (kernel32, user32, gdi32, Direct3D 8/9, DirectSound,
DirectMusic, MCI, XInput, ...) are implemented in Rust. Nothing is re-implemented
from GameMaker itself, so games behave as they do on Windows, x87 rounding included.

## Requirements

- Rust (stable for the native tools; nightly for the browser build:
  `rustup toolchain install nightly`, `rustup target add wasm32-unknown-unknown --toolchain nightly`)
- Node.js 22+ (dev server, tests), a Chromium-based browser or Firefox
- `7z` (importing IExpress/CAB and RAR archives), `objcopy` (optional, strips the
  system DLLs it supplies), `gcc` with 32-bit support (the CPU differential tests
  run a small native reference program)
- Wine's 32-bit PE DLLs (`/usr/lib64/wine-wow64/wine/i386-windows`, used by the
  importer for the few runtime DLLs games need but do not ship)
- Optional: a General MIDI SoundFont as `web/soundfont.sf2` (e.g. a link to
  `/usr/share/soundfonts/default.sf2`), for games with MIDI music

## Using it

```sh
# import games (folders, .zip, .rar, .exe) into web/public/games/<slug>/
cargo run --release -p importer -- import path/to/game.zip --out web/public/games

# build the browser runtime
rustup run nightly cargo build --release --target wasm32-unknown-unknown -p web --target-dir target/wasm

# serve it (cross-origin isolation headers included) and open http://localhost:8766/
node web/dev/server.mjs
```

The page lists the imported games; a game plays at its own speed with keyboard,
mouse and game controllers, sound starts with the first click or key press, and
saves are kept in the browser (export/import from the game page).

## How it works

```
game.exe + DLLs ──discovery──► functions ──recomp (x86 → wasm, one module per batch)──┐
                                                                                        ▼
browser worker: runtime.wasm (Rust: engine, HLE APIs, x87 model) + cpu glue + x87 fast paths
   │ guest memory = its own wasm memory      │ D3D draw records ──► WebGL2 (gpu.mjs)
   │ virtual clock, deterministic threads   │ sound mix ──► SharedArrayBuffer ring ──► AudioWorklet
```

- `crates/recomp`: the x86 → wasm translator (lazy flags, register locals, a
  `br_table` dispatcher per function, SSE on wasm SIMD, x87 fast paths in a shared
  module with an exact 80-bit model behind them).
- `crates/fpu`: the x87 model (rustc_apfloat; bit-exact in every precision and
  rounding mode).
- `crates/runtime`: PE loader, process state, the HLE layer (`hle/`), the engine
  (dispatcher, SEH, API calls, threads).
- `crates/web`: the browser build of the runtime; `web/`: page, worker, WebGL2
  renderer, audio worklet.
- `crates/harness`: runs the same runtime natively on wasmtime (games headless, state
  hashes, WAV output, traces) and the CPU differential tests against the host CPU.
- `crates/importer`: unpacks games, detects the engine, inventories modules and APIs.

Runs are deterministic: the clock is virtual, threads are scheduled cooperatively,
and the same input gives the same state hash in the harness, Node, Chrome and Firefox.

## Status

| Game | Engine | Verified so far |
|---|---|---|
| Not Another Needle Game | GM 8.1 | mouse menus; New Game starts the game; music (supersound.dll on DirectSound) |
| i wanna uhuhu spike | GM 8.0 | the kid runs and jumps in the hub; first stage; MP3 music (MCI), sound effects (DirectMusic) |
| I wanna run the Marathon | GMS 1.4 VM | file select, hub, the marathon starts; title music from the GMS audio thread |
| Crimson Needle 3 | GMS 1.4 VM | title, Shift to the save-file screen |
| indigo needle | GMS 1.4 VM + GMFile.dll | title room, the player moves (extension runs on Wine's msvcp100) |
| untitled needle game | GMS 1.4 YYC | intro and tutorial: run, jump; shaders compiled by Wine's d3dcompiler |
| RNG Dungeon | GMS 1.4 YYC | intro, file select, a save chosen; all 22 shaders translate to GLSL |

See `docs/` for each milestone's notes (`r1-cpu.md` CPU and tests, `r2-harness.md`
the native harness, `r3-r5-runtime.md` the browser runtime, audio, threads,
Direct3D 9 and shaders).

## Tests

```sh
cargo test --release                                   # unit tests
cargo run --release -p harness -- cpu-diff --mode x87fast --count 5000   # CPU vs. host
cargo run --release -p harness -- run web/public/games/<slug> --frames 300
web/tools/run-browser.sh chrome 'game=<slug>&frames=300&epoch=1767225600&nopace=1'
```
