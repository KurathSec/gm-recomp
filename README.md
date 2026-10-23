# gm-recomp

Windows GameMaker games running in desktop browsers, from their original executables.

gm-recomp statically recompiles a game's x86 code to WebAssembly: the GameMaker runner,
the game's own code when it was compiled with YYC, and every DLL the game ships. The
Windows APIs that code calls (kernel32, user32, gdi32, Direct3D 8/9, DirectSound,
DirectMusic, MCI, DirectInput, XInput, Winsock, ...) are implemented in Rust. Nothing of
GameMaker itself is reimplemented, so a game behaves as it does on Windows, x87 rounding
included.

Supported engines: GameMaker 8.0 and 8.1, and GameMaker: Studio 1.4 (VM and YYC).

- **Faithful.** It runs the original runner code. The x87 FPU is modelled bit-exactly in
  every precision and rounding mode.
- **Deterministic.** The clock is virtual and threads are scheduled cooperatively. The
  same input gives the same state hash in the native harness, Node, Chrome and Firefox.
- **Plays like the original.** Games run at their own speed (usually 50 fps) with
  keyboard, mouse and game controllers. Saves are kept in the browser and can be exported.
- **No per-game code.** Games are imported from their archive (zip, 7z, RAR,
  IExpress packages), natively or in the page.
- **Mods.** A debug switch, and reimplementations of Fangame.ModLoader's mods, can be
  turned on per game.

No game files are included: you import your own copies.

## Status

Every game in the test corpus reaches a playable state in Chrome and Firefox:

| Game | Engine | Verified so far |
|---|---|---|
| Not Another Needle Game | GM 8.1 | mouse menus; New Game starts the game; music (supersound.dll on DirectSound) |
| i wanna uhuhu spike | GM 8.0 | the kid runs and jumps in the hub; first stage; MP3 music (MCI), sound effects (DirectMusic) |
| I wanna run the Marathon | GMS 1.4 VM | file select, hub, the marathon starts; title music from the GMS audio thread; Box2D golf (via a test copy) |
| Crimson Needle 3 | GMS 1.4 VM | title, save select, new game (walk right into the opening); music |
| indigo needle | GMS 1.4 VM + GMFile.dll | title room, the player moves (the extension runs on Wine's msvcp100) |
| untitled needle game | GMS 1.4 YYC | intro and tutorial: run, jump; shaders compiled by Wine's d3dcompiler |
| RNG Dungeon | GMS 1.4 YYC | intro, file select, a save chosen; all 22 shaders translate to GLSL |
| I Wanna be the Vandal | GMS 1.4 VM | held out until the end: title, save select, new game, out of the box |
| I wanna Bread | GM 8.1 (Release Candidate runner) | held out until the end: title, data select, the hub room, out of the box |

GameMaker Studio 2 games start, with their 32-bit runners and with the 64-bit ones most of
them use. Direct3D 11 and DXGI are emulated (`crates/runtime/src/hle/d3d11.rs`), with
their DXBC shaders translated to GLSL (`hle/dxbc.rs`), and their sound goes through
WASAPI (`hle/wasapi.rs`). 64-bit code is recompiled as 32-bit code is (`crates/recomp/src/lift64.rs`)
and calls the same Windows APIs by the Win64 convention (docs/x64.md). In Chrome, I wanna
cat addiction, I wanna Dodge The Bullets and Night Guy (32-bit), Sunshine, I Wanna Find A
New Puter, I Wanna be the State of Destruction, I Wanna Rob Santa Claus, I Wanna Be The
Furry Guy, I Wanna Horizon, Super I Just Wanna Play The Needle Game and two more (64-bit)
show their title screens at 50 fps; New Puter takes input and State of Destruction plays.

## Getting started

### Requirements

- Rust: stable for the native tools; nightly for the browser builds
  (`rustup toolchain install nightly`, `rustup target add wasm32-unknown-unknown --toolchain nightly`)
- Node.js 22+ (dev server, tests), and a Chromium-based browser or Firefox
- `7z`, for importing IExpress/CAB and RAR archives
- Wine's 32-bit PE DLLs (`/usr/lib64/wine-wow64/wine/i386-windows`). The importer adds
  the few runtime DLLs that games need but do not ship.
- Optional:
  - `objcopy`, which strips the system DLLs the importer supplies
  - `gcc` with 32-bit support, for the CPU differential tests (they run a small native
    reference program)
  - a General MIDI SoundFont as `web/soundfont.sf2`, for games with MIDI music (for
    example a link to `/usr/share/soundfonts/default.sf2`)

### Build and run

```sh
# import games (folders, .zip, .rar, .exe) into web/public/games/<slug>/
cargo run --release -p importer -- import path/to/game.zip --out web/public/games

# build the browser runtime (without DWARF: 3.4 MB instead of 21; the name section,
# which profiles and stack traces use, stays)
CARGO_PROFILE_RELEASE_DEBUG=0 cargo +nightly build --release --target wasm32-unknown-unknown -p web --target-dir target/wasm

# serve it (with the cross-origin isolation headers it needs) and open http://localhost:8766/
node web/dev/server.mjs
```

The page lists the imported games. Sound starts with the first click or key press.
While a game plays:
- **F4** switches the browser to full screen. A game's own full-screen switch would only resize its window here.
- **Keys** remaps keys outside the game, for that game or for every game.
- When the game ends (Esc in most fangames), the page goes back to the list.

### Importing in the browser

Games can also be imported from the page:
- "Import a game…" on the page, or
- `?zip=<url>` (for example a link to the archive in object storage), which imports the
  game and starts it.

A zip, 7z or RAR works, and an IExpress package inside it is unwrapped. NSIS
installers still need the native importer. The imported game is kept in the browser
and plays with `?game=<slug>&local`. This needs two more builds:

```sh
# the importer for the page (served at /import/)
CARGO_PROFILE_RELEASE_DEBUG=0 cargo +nightly build --release --target wasm32-unknown-unknown -p webimport --target-dir target/wasm-import
# the system DLLs it may add (served at /system32/; from Wine's PE builds)
web/tools/system32.sh
```

## Mods

Each game's card has a **Mods** button, and so does the bar while a game plays. It
lists changes made to the game's files before the game starts, only those the game's
engine allows.

- **Debug mode** turns on the game's debug switch, as patcher.rs does. It works for
  GameMaker: Studio, VM or YYC.
- Fangame.ModLoader's mods are reimplemented for GameMaker 8 and GameMaker: Studio 1.4
  VM games:
  - GodMode, CrimsonKid and DotKid;
  - DbgHelper: gm8x_fix's patches on GameMaker 8, the sleep margin on GameMaker: Studio;
  - AllowFullscreen, on GameMaker 8 only.
- **Play online** (GameMaker 8): [I wanna play online 1.2.3](https://github.com/Pardocaninacting/i-wanna-play-online-1.2.3)'s
  converter runs in the page and changes the game before it starts: the others playing the
  same game show up, with chat and their saves. Its `http_dll_2_3.dll` is replaced by a port
  in the runtime (`crates/runtime/src/hle/httpdll.rs`).

A GameMaker: Studio data.win is changed in place where sizes allow, and grows at its
end otherwise; `crates/gmmod/src/gms.rs` explains how. The mods' own files (sprites,
scripts) come from a Fangame.ModLoader folder:

```sh
# the mods for the page (served at /gmmod/)
CARGO_PROFILE_RELEASE_DEBUG=0 cargo +nightly build --release --target wasm32-unknown-unknown -p gmmod --target-dir target/wasm-mods
# their files (served at /mods/)
web/tools/mods.sh path/to/Fangame.ModLoader
```

The converter is built from a checkout of I wanna play online into `web/src/iwpo-gm8.mjs`
(not in the repository):

```sh
npm ci --prefix web/tools/iwpo
node web/tools/iwpo/build.mjs path/to/i-wanna-play-online-1.2.3
```

A browser cannot open TCP or UDP connections itself, so a game's network connections go
over WebSockets: to a server that takes them (I wanna play online's server does at `/ws`
on its HTTP port), or through a relay you run. Play online's settings name the WebSocket
URL (`ws://…` or `wss://…`):

```sh
node web/tools/relay.mjs --port 8790 --allow 123.iwannaplay.online
```

GameMaker 8's own dialogs (show_message, show_question, get_string, get_integer) are not
drawn in the page: the player's browser asks instead (its alert, confirm and prompt, or
the embedding site's `onPrompt`).

## Embedding the player

A site can run games on its own pages. `web/src/player.mjs` exports `playGame(...)`, which:
- starts a game in a canvas;
- feeds it the page's keyboard, mouse and controllers;
- plays its sound;
- reports what happens through callbacks: the game's resolution, its log, saves stored, the game ending.

The page around it decides the rest: layout, buttons, where saves go. Other modules help:
- `keymap.mjs`: key remapping and its editor.
- `import.mjs`: importing an archive.
- `saves.mjs`: saves in IndexedDB, and exporting them.
- `mods.mjs`: mod choices.

The modules find their files by relative paths. `web/tools/bundle.sh <dir>` collects everything into one directory, which can be served under any path:
- the modules;
- the three browser builds;
- the site profiles;
- the system DLLs;
- the mods' files.

The page that loads it must be cross-origin isolated (`Cross-Origin-Opener-Policy: same-origin`, and `Cross-Origin-Embedder-Policy: require-corp` or `credentialless`). [fangame-archive](https://github.com/KurathSec/fangame-archive)'s `/play/` page is an example. It downloads a game from the catalog, imports it once, and keeps its saves in the player's account.

```sh
web/tools/bundle.sh dist/player && tar -czf gm-recomp-player.tar.gz -C dist/player .
```

## Translation profiles

The browser remembers which code each game needed. In the next session it translates
that code right after the first frame, so the game does not stall the first time it
reaches a room or effect; `?noprofile` turns this off. A game's first session starts
from what other games built with the same runner needed. If there are none yet, it uses
the site's profiles in `web/public/profiles`. To remake those:

```sh
# per run (any games, recorded sessions with --replay), then merged
node web/tools/run-node.mjs web/public/games/<slug> --frames 1000 --module-profiles runs/<slug>
node web/tools/module-profiles.mjs web/public/profiles runs/*/
```

## How it works

```
game.exe + DLLs ──discovery──► functions ──recomp (x86 → wasm, one module per batch)──┐
                                                                                        ▼
browser worker: runtime.wasm (Rust: engine, HLE APIs, x87 model) + cpu glue + x87 fast paths
   │ guest memory = its own wasm memory      │ D3D draw records ──► WebGL2 (gpu.mjs)
   │ virtual clock, deterministic threads   │ sound mix ──► SharedArrayBuffer ring ──► AudioWorklet
```

The translator (`crates/recomp`):
- computes flags lazily and keeps registers in wasm locals;
- lays out blocks so that loops are wasm loops and other jumps are forward branches;
- inlines small callees;
- runs SSE on wasm SIMD;
- runs common x87 instructions inline, with the exact 80-bit model behind them.

Code that static discovery missed is translated when it first runs.

## Repository layout

| Path | What it is |
|---|---|
| `crates/pe` | PE image parsing |
| `crates/x86` | x86 decoding and static code discovery |
| `crates/recomp` | the x86 → wasm translator |
| `crates/fpu` | the x87 model (rustc_apfloat; bit-exact in every precision and rounding mode) |
| `crates/runtime` | PE loader, process state, the HLE layer (`hle/`), the engine (dispatcher, SEH, API calls, threads) |
| `crates/web` | the browser build of the runtime |
| `crates/harness` | the same runtime run natively on wasmtime (games headless, state hashes, WAV output, traces) and the CPU differential tests |
| `crates/importer` | unpacks games, detects the engine, inventories modules and APIs |
| `crates/webimport` | the importer's unpacking and detection, for the page |
| `crates/gmmod` | the mods |
| `web/` | page, worker, WebGL2 renderer, audio worklet, the embeddable player (`src/player.mjs`), dev server and tools |
| `tests/` | the CPU reference program, a Direct3D 9 shader test game, data.win tools |
| `docs/` | design notes per milestone |

## Testing

```sh
cargo test --release                                                     # unit tests
cargo run --release -p harness -- cpu-diff --mode x87fast --count 5000   # CPU vs. host
cargo run --release -p harness -- run web/public/games/<slug> --frames 300
web/tools/run-browser.sh chrome 'game=<slug>&frames=300&epoch=1767225600&nopace=1'
```

There are also test tools:
- `tests/d3d9/make-test-game.sh` builds a Direct3D 9 shader test that runs like a game
  (`game=zz-test-d3d9-shaders`).
- `tests/gms/` has tools for GMS `data.win` files: a room lister, a rough GML bytecode
  lister, and test copies with patched bytecode.

## Documentation

`docs/README.md` indexes the design notes. Each milestone has its own: the corpus and
browser feasibility, the CPU and its tests, the native harness, then the browser
runtime with audio, threads, Direct3D 9, shaders and performance.

## Acknowledgements

- [iced-x86](https://github.com/icedland/iced) decodes x86. [rustc_apfloat](https://github.com/rust-lang/rustc_apfloat)
  is the core of the x87 model. [wasm-encoder](https://github.com/bytecodealliance/wasm-tools) builds
  the modules, which [wasmtime](https://wasmtime.dev/) runs natively.
- [Symphonia](https://github.com/pdeljanov/Symphonia) decodes MP3, and
  [RustySynth](https://github.com/sinshu/rustysynth) plays MIDI.
- [Wine](https://www.winehq.org/)'s PE DLLs stand in for Windows runtime libraries that
  games need but do not ship. They are loaded at run time and are not part of this
  repository.
- [UndertaleModTool](https://github.com/UnderminersTeam/UndertaleModTool) documents the
  GameMaker: Studio data format and bytecode.
- [gm8x_fix](https://github.com/skyfloogle/gm8x_fix) (MIT): DbgHelper's GameMaker 8
  patches are its patch table, converted.
- Fangame.ModLoader and patcher.rs are the mods reimplemented here.
- [I wanna play online 1.2.3](https://github.com/Pardocaninacting/i-wanna-play-online-1.2.3) (MIT):
  its converter runs in the page as Play online, and `hle/httpdll.rs` ports its
  http_dll_2_3 (Exports.cs).

## License

MIT; see [LICENSE](LICENSE). GameMaker is a trademark of YoYo Games; this project is not
affiliated with it.
