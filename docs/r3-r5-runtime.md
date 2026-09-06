# R3–R5: the browser runtime, sound, saves, threads, Direct3D 9

The runtime that R2 ran in wasmtime now runs in the browser as well (`crates/web`,
`web/`), and covers what GameMaker: Studio 1.4 needs on top of GameMaker 8: guest
threads, Direct3D 9 with shaders, XInput. Same code in both hosts; no game-specific
code anywhere.

## Status

| Game | Engine | Verified |
|---|---|---|
| Not Another Needle Game | GM 8.1 | title screen in Chrome and Firefox; music through the recompiled supersound.dll |
| i wanna uhuhu spike | GM 8.0 | title screen; MP3 music via MCI (correlation 0.9999 with ffmpeg's decode); DirectMusic sound effects |
| I wanna run the Marathon | GMS 1.4 VM | title screen; title music from the GMS audio thread (correlation 0.999 with mus_Title.ogg) |
| Crimson Needle 3 | GMS 1.4 VM | title screen |
| indigo needle | GMS 1.4 VM | title screen; GMFile.dll runs on Wine's msvcp100 |
| untitled needle game | GMS 1.4 YYC | intro cutscene, tutorial: runs, jumps, falls into and climbs out of a pit; its shaders compile |
| RNG Dungeon | GMS 1.4 YYC | title screen; all 22 shaders translate and compile as GLSL in Chrome |

- All seven run 200 frames in the harness; the held-out games (Vandal, Bread) have not
  been run (that is R6).
- Harness, Node, Chrome and Firefox end the same run in the same state hash, threads
  and scripted input included. Every map in the runtime uses a fixed hasher, so
  iteration order (timers, windows) is the same in native and wasm builds. Parallel
  harness runs of one game need their own `--write-dir`.
- No shaded draw has been seen yet in the frames tested (the games draw their title
  screens fixed-function); the translator is checked by compiling every shader.

## Browser host

- The page (`web/index.html`) lists the games from `games/index.json` (written by
  `importer import` / `importer index`) and plays one in a worker
  (`web/src/worker.mjs`) that draws into an OffscreenCanvas through WebGL2.
- `web/dev/server.mjs` serves the page with COOP/COEP (SharedArrayBuffer needs them)
  and the runtime from `target/wasm/wasm32-unknown-unknown/release/`.
- Pacing: the guest runs on the virtual clock; `Present` suspends the wasm stack
  (JSPI, `WebAssembly.Suspending`) until real time reaches the frame's virtual time,
  so the game's own frame limiter decides the rate (50 fps games run at 50 fps,
  rooms with another speed at theirs). More than 250 ms behind (a slow frame, a
  hidden tab) re-bases instead of fast-forwarding.
- Input: key and mouse events go to the worker as they happen (`event.code` → VK and
  scan code, `web/src/keys.mjs`) and are applied at the next API that reads input;
  keys the browser would act on (F1–F12, Tab, arrows, Backspace, ...) are
  `preventDefault`ed. Gamepads are polled each animation frame.
- Scaling: largest integer multiple that fits (or any fit), fullscreen button.
- Test parameters on the page URL: `frames=N` (stop after N Presents), `epoch=S`
  (fixed clock start for reproducible runs), `nopace`, `capture=f1,f2` (PNG of those
  frames), `script=frame:code:1|0,frame:mouse:x:y:buttons`, `report` (summary for
  the tools), `audio` (start sound without a gesture), `nosave`, `log`.
  `web/tools/run-browser.sh chrome|firefox '<query>' [capture dir]` runs one headless;
  `web/tools/run-node.mjs` runs the same runtime in Node.

## Sound

- DirectSound (`hle/audio.rs`, `hle/dsound.rs`): every playing buffer is mixed into
  one 48 kHz stereo stream at the virtual time its play cursor passes the samples
  (rate conversion, volume, pan, frequency, looping, notification positions). At each
  Present the finished samples go to the host: a SharedArrayBuffer ring read by an
  AudioWorklet (`web/src/audio-worklet.js`, 50 ms jitter buffer) in the browser, a
  WAV file with `harness run --audio-out`.
- DirectMusic (`hle/dmusic.rs`), what GameMaker 8 plays its sounds through: loader,
  segments (wave and MIDI), segment states, standard audio paths with volume and the
  path buffer for pan, `PlaySegmentEx`/`StopEx`/`IsPlaying`.
- MCI (`hle/mci.rs`): command and string interfaces over one device table; wave, MP3
  (symphonia) and MIDI, positions and `MM_MCINOTIFY` on the virtual clock.
- MIDI (`hle/midi.rs`): rustysynth with a General MIDI SoundFont that the host hands
  over only when a game first plays MIDI (the browser fetches `/soundfont.sf2` then;
  the harness reads `GMR_SOUNDFONT` or `/usr/share/soundfonts/default.sf2`). Until it
  arrives MIDI is silent; output never feeds back into the guest, so this does not
  affect determinism.
- Sound starts on the page's first click or key press (autoplay policy); the mix runs
  regardless, so state does not depend on it.

## Saves

- The VFS records every file a game writes; outside `C:\temp` they are handed to the
  host every 25 Presents and at exit. The browser keeps them in IndexedDB
  (`web/src/saves.mjs`, key `<game>\0<lowercase path>`, strict durability) and lays
  them over the game's files at the next start.
- The game page exports them (one JSON file per game), imports and deletes them.

## Guest threads (`hle/threads.rs`)

- One thread runs at a time. The main thread runs as the host calls it; the others
  run from its blocking points (`Sleep`, waits, critical sections, idle message
  loops: whatever advances the virtual clock), each until it blocks, ends or uses up
  a budget of 200 000 dispatches.
- A thread has its own stack, TEB and x87 block (the page below the TEB) and keeps
  its registers, flags, XMM and MXCSR while switched out. It yields only at its own
  top level, not inside a callback the HLE made into it; a wait that cannot be
  satisfied there returns as timed out.
- Supported: CreateThread/ExitThread/Suspend/Resume, events, mutexes, semaphores,
  waitable timers, critical sections (and the Rtl* forms), WaitForMultipleObjects,
  thread exit codes. Scheduling depends only on the virtual clock and the guest, so
  runs stay deterministic. This is what plays GMS 1.4's audio (its mixer thread feeds
  a DirectSound buffer).

## Direct3D 9 (`hle/d3d9.rs`, `hle/d3d9_shader.rs`, `web/src/gpu.mjs`)

- COM objects in guest memory like the Direct3D 8 layer; resources live in guest
  memory and are uploaded when used. Draws without shaders go through the Direct3D 8
  fixed-function path (GMS draws that way with FVF vertices); with shaders they go to
  the host as `DrawCall9` records (vertex declaration, streams, constants, samplers,
  render state).
- Render targets are framebuffers; `StretchRect` is `blitFramebuffer`,
  `GetRenderTargetData` is `readPixels` into the guest's surface.
- Shader bytecode (vs/ps 2.0, 2.x, 3.0) is translated to GLSL ES 3.00 when created.
  Vertex inputs are named by D3D usage, stage varyings by semantic, so separately
  translated vertex and pixel shaders link; the vertex shader flips y, applies the
  half-pixel offset and maps depth to GL's range; D3DCOLOR inputs are swizzled per
  draw; alpha test runs in the pixel shader. SM2 pixel shaders declare `t#`/`v#`
  without usage: they are TEXCOORD and COLOR. The page compiles each shader when
  created and reports failures.
- GMS compiles its shaders' HLSL at run time with `D3DXCompileShader`. There is no
  compiler to emulate: the importer supplies Wine's d3dcompiler_43 (with the vkd3d
  HLSL compiler inside) and ucrtbase as guest DLLs, and they run recompiled like any
  other code.

## System DLLs the games do not ship

- The importer walks the game's modules and copies DLLs that are imported (or, for
  MSVC runtimes, merely named in strings, since GMS loads them by name) but neither
  shipped nor emulated, from Wine's 32-bit PE builds into the game's
  `files/C/Windows/system32/`, stripped with `objcopy`. d3dcompiler is supplied only
  when the game has shaders.
- `LoadLibrary` searches the game directory, then system32, and loads a DLL's
  dependencies before running its entry point.

## Game controllers (`hle/pads.rs`)

- The page polls the Gamepad API (standard layout = Xbox controller) and sends
  changes to the worker. XInput (xinput1_1–1_4, 9_1_0) for GMS and the winmm
  joystick API for GameMaker 8 read those states.

## Performance

Measured on the development machine (timings vary with its other load):

| Run | Before | After |
|---|---|---|
| Needle, 300 frames, harness | 40 s | 8.5 s |
| Needle, 300 frames, Chrome (`nopace`) | 44 s | 13 s |
| RNG Dungeon, Chrome | 34 s | 11.6 s |
| RNG Dungeon, harness | 138 s | 12 s |
| untitled needle game tutorial room, per frame, harness | 15.3 ms | 7.9 ms |

Frame cost in that tutorial room (YYC, the heaviest scene measured), guest time per
frame over the second half of a 1120-frame run:

| Host | median | p95 | p99 |
|---|---|---|---|
| Chrome | 9.5 ms | 14.0 ms | 16.8 ms |
| Firefox | 10.1 ms | 12.6 ms | 14.3 ms |

The budget at 50 fps is 20 ms. Room changes still take one long frame (~170 ms:
the new room's code is translated then).

- Calls between translated functions: almost every function is its own module
  (translated on first call), so a call is an indirect call through `rt.resolve`.
  Translated code first probes a direct-mapped cache in reserved guest memory
  (`abi::RESOLVE_CACHE`, 64 K entries of [address, table index]) and calls the
  runtime only on a miss; the runtime fills it with translated functions, never API
  stubs. In the tutorial room this took `rt.resolve` from 347 000 calls per frame to
  almost none. The state hash leaves the cache out.
- x87 fast paths cover every rounding mode (D3DX9 converts pixels in chop mode; the
  soft model had been running ~6 000 instructions per frame of it).
- x87 fast paths live in one shared `x87` module, not in each translated function
  (Cranelift compile times had exploded with them inline); the state hash is the same
  with them on or off (`GMR_X87_SLOW=1`).
- The guest heap is best-fit with coalescing; first-fit was O(n) per allocation and
  dominated GMS loading.
- The process state is boxed (it was copied, 4.4 KB, on every API call); the
  function map uses an address hasher; API names in logs are built only when logged.
- `GMR_FPU_PROFILE=1` lists the x87 instructions that still reach `rt.fpu`, by form
  and by address; `node --cpu-prof web/tools/run-node.mjs ...` profiles the browser
  build by translated function (`f_<address>`).

## Known gaps

- Shaded draws are translated and compiled but not yet verified against Windows
  output in a scene that uses them.
- CPUID does not report MMX (MMX is not lifted); code with an SSE or scalar fallback
  takes it.
- x87 deviations in rare edges: fsqrt of extended-precision denormals, fidivr with
  precision control 01 (see `r1-cpu.md`).
- No networking: ws2_32 reports the network as not ready, wininet fails to open; Steam is not loaded.
- Frame pacing needs JSPI; without it the game runs unpaced.
