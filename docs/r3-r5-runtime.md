# R3–R5: the browser runtime, sound, saves, threads, Direct3D 9

The runtime that R2 ran in wasmtime now runs in the browser as well (`crates/web`,
`web/`), and covers what GameMaker: Studio 1.4 needs on top of GameMaker 8: guest
threads, Direct3D 9 with shaders, XInput. Same code in both hosts; no game-specific
code anywhere.

## Status

| Game | Engine | Verified (scripted input in Chrome, frames captured) |
|---|---|---|
| Not Another Needle Game | GM 8.1 | mouse menus, New Game starts the game; music through the recompiled supersound.dll |
| i wanna uhuhu spike | GM 8.0 | hub room: run, jump, double jump, warp to the first stage; MP3 music via MCI (correlation 0.9999 with ffmpeg's decode); DirectMusic sound effects |
| I wanna run the Marathon | GMS 1.4 VM | file select, hub, warp, the marathon starts ("On your mark... Go!!"); title music from the GMS audio thread (correlation 0.999 with mus_Title.ogg) |
| Crimson Needle 3 | GMS 1.4 VM | title, Shift to the save-file screen (needed the D3D9 binding references below) |
| indigo needle | GMS 1.4 VM | title room, the player walks and falls; GMFile.dll runs on Wine's msvcp100 |
| untitled needle game | GMS 1.4 YYC | intro cutscene, tutorial: runs, jumps, falls into and climbs out of a pit; its shaders compile |
| RNG Dungeon | GMS 1.4 YYC | intro, title, file select, a save chosen; all 22 shaders translate and compile as GLSL in Chrome |

- All seven run 300 frames in the harness.
- R6, the held-out games, run for the first time after everything above was done and
  with no change for them: I Wanna be the Vandal (GMS 1.4.9999) goes from its title to
  the save select and a new game, I wanna Bread (GM 8.1, a Release Candidate runner)
  from its title to the data select and its hub room. Chrome and Firefox end in the
  same state hash; both hold 50.0 fps with frame intervals within ~1 ms of 20.
- Harness, Node, Chrome and Firefox end the same run in the same state hash, threads
  and scripted input included. Every map in the runtime uses a fixed hasher, so
  iteration order (timers, windows) is the same in native and wasm builds. Parallel
  harness runs of one game need their own `--write-dir`.
- No shaded draw has been seen yet in the frames tested (the games draw their title
  screens fixed-function); the translator is checked by compiling every shader.

## The R5 items that need a game situation

Reached with test copies of the games (`tests/gms/testcopy.py`: the imported files
symlinked, one bytecode word of `data.win` changed; recipes in the script) or a test
program, rather than by playing there:

- Box2D (Marathon's golf rooms, physics worlds with gravity 50 at 0.1 m/px): a copy
  whose `startRoom` is `rGolfWond` (the golf autosave skipped; it needs state from
  the stages before). In Chrome the hole loads, the power bar charges, the shot
  counts, the ball flies in a spinning arc, lands on the hill and rolls back down
  the slope.
- Room speed changes: Crimson Needle 3's opening rooms say 30 fps, but its
  `objWorld` sets `room_speed = 50` at every room start, so the game itself runs at
  50 throughout (on Windows too). A copy that skips that assignment walks from the
  50 fps menu into the 30 fps opening: frames go from 20 ms to 33/34 ms at the room
  change, the game's timer ticks every 30 frames, and the browser paces it at 30.0
  fps. A copy that sets 30 everywhere runs at 33.36 ms per frame.
- Shaded draws: neither shader game uses one before you play some way in, so
  `tests/d3d9/shader_test.c` (built and laid out as a game by
  `tests/d3d9/make-test-game.sh`) compiles HLSL at run time with Wine's d3dcompiler,
  draws through vs/ps 2.0 and 3.0 shaders (a mirroring matrix in constants, vertex
  colour, point-sampled texture, dot, lerp, frac, a branch) into a render target,
  reads it back and compares 16 pixels per pair with the CPU: PASS in Chrome and
  Firefox. With a wrong expectation it reports FAIL (the check is live).

Found on the way: GMS runners with OpenAL Soft's DirectSound backend (Crimson Needle
3) found no sound device, since DirectSoundEnumerate listed only the primary driver
(NULL GUID), which that backend skips; Windows lists the device itself too, and so do
we now. mingw-built programs needed msvcrt's startup functions and run TZCNT/LZCNT as
BSF/BSR (our CPUID has neither BMI1 nor LZCNT).

## Browser host

- The page (`web/index.html`) lists the games from `games/index.json` (written by
  `importer import` / `importer index`) and plays one in a worker
  (`web/src/worker.mjs`) that draws into an OffscreenCanvas through WebGL2.
- `web/dev/server.mjs` serves the page with COOP/COEP (SharedArrayBuffer needs them)
  and the runtime from `target/wasm/wasm32-unknown-unknown/release/`.
- Pacing: the guest runs on the virtual clock, and real time follows it. Wherever the
  main thread's clock moves on (Sleep, waits, waitable timers, clock polls; checked
  once per millisecond), `pace` suspends the wasm stack (JSPI) until real time gets
  there, and Present shows each frame at once. So the game's own frame limiter sets
  the rate (50 fps games at 50, rooms with another speed at theirs), secondary
  threads that wake while the main thread sleeps (GMS's audio decoder) run in that
  idle time instead of inside a frame, and input is read right after the wait. More
  than 250 ms behind (a long load, a hidden tab) re-bases instead of fast-forwarding.
  Waits use `Atomics.waitAsync` (nested `setTimeout`s are clamped to 4 ms).
  `?pacing=jit` (hold each frame until its time, resume the guest just in time) and
  `?pacing=asap` remain for comparison. Marathon in Chrome: 50.0 fps, frame interval
  20.0 ms (longest 21–27 ms), no late frames; waits end 0.4 ms late on average.
- Stats (the Stats button, or `?stats`): frame rate, real frame interval, guest time
  per frame, late frames, timer lateness, and input latency from the key event to the
  Present of the frame that read it (the display adds up to one refresh).
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
- The device holds a reference to everything bound to it (render targets, depth
  surface, textures, streams, index buffer, shaders, declaration; state-block Apply
  moves them) and to its back buffer, as Direct3D does. Without that, Crimson Needle
  3 released its last reference to the back buffer surface on a room change, the
  heap reused the address, and the game drew off screen from then on.
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

## Game controllers and DirectInput (`hle/pads.rs`, `hle/dinput.rs`)

- The page polls the Gamepad API (standard layout = Xbox controller) and sends
  changes to the worker. XInput (xinput1_1–1_4, 9_1_0) for GMS and the winmm
  joystick API for GameMaker 8 read those states.
- DirectInput 8 (A and W): the system keyboard (by DIK scan code), the system mouse
  (relative motion) and each connected controller as a joystick, as Windows lists an
  Xbox controller (triggers on Z, D-pad as POV); standard data formats, axis ranges,
  names, object enumeration. Buffered data is not kept. GMS 1.4 enumerates its
  joysticks through it.

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
- Code the game has not reached yet is translated while it waits for real time
  (`engine::translate_idle`, a few functions per wait, sized by the measured cost):
  the first run of new code no longer pays for its translation in that frame. Room
  changes: untitled needle game's tutorial 171 -> 125 ms, Bread's first stage 124 ->
  108 ms (the rest is the game's own loading); `?noahead` turns it off.
- Modules hold several functions. Firefox runs out of executable memory after a
  bounded number of modules (`web/memtest/modlimit.html`: 16 349 small ones; a GM8
  game stopped at ~7 600, its title-to-file-select startup with ahead translation),
  however few functions each holds, so one module per function stopped Not Another
  Needle Game at its file select. A call to untranslated code now translates up to
  16 functions together (`engine::CALL_BATCH`): the called one, those it calls
  directly, then its untranslated neighbours in the image; a return address reached
  after an unwound stack takes the return addresses above it on the stack; ahead
  translation makes modules of 8 to 32.
- `ret` to an address the same function pushed (`cfg::Cfg::ret_targets`: Delphi's
  try/finally pushes its exit label, falls into the finally code and `ret`s there)
  continues in the function. It used to return to the dispatcher, which unwound every
  caller and re-entered each at its return address as a new one-off function: Bread,
  900 frames, 5.0 M dispatches -> 2 887 and 7 133 modules -> 697; frame cost median
  Firefox 4.32 -> 2.10 ms, Chrome 2.28 -> 1.94 ms; translation Firefox 4.4 -> 0.8 s.
- Translated functions take the eight registers as parameters and return them
  (`types::FUNC`); before, every call stored them in the CPU globals and every
  callee loaded them, and back. The runtime still sees the globals: they are
  written around a resolve-cache miss (an API may run there), a trap, cpuid/rdtsc
  and whenever control goes back to the dispatcher, whose `call` (the browser's
  glue, the harness) passes them in and out. Calls hand over no flags (no calling
  convention passes them; the callee's come back through the globals at `ret`, for
  Delphi's string compares), except those the callee's entry reads before writing
  (`cfg::Cfg::call_flags`: the CRT's x87 wrappers test the ZF a previous call left),
  so most flag computations before calls are dead code, and a function loads only
  the flags its entry block reads. Counting with
  `GMR_COUNT=1` (per frame with `GMR_FRAME_MS=0`) showed what dominates: a GMS
  death (YoYoYo engine, blood) runs ~830 000 guest calls per frame.
- x87 fast paths for 64-bit precision when the result is exact (the CRT's modf and
  floor switch to it: `x - trunc(x)`), `fxam`, and `fsin`/`fcos` (their value is
  `fpu::sin`/`cos`, which the x87 module imports from the runtime so both paths
  give the same bits). A GMS death, rt.fpu calls per frame ~6 000 -> ~1 800.
  Same death, guest time per frame at its peak: Firefox 10.2 -> 6.7 ms, Chrome
  14.4 -> 6.6 ms (harness 22.8 -> 18.1 ms).
- MCI decodes MP3s as they play (a whole song used to be decoded in the frame its room
  started: 361 ms in Bread); Delphi's resource-string lookups use a parsed resource
  directory per module.
- `GMR_FPU_PROFILE=1` lists the x87 instructions that still reach `rt.fpu`, by form
  and by address; `node --cpu-prof web/tools/run-node.mjs ...` profiles the browser
  build by translated function (`f_<address>`).

Measured on recorded play sessions (`?replay=`: 3055 with a death,
Vandal's seventh floor):

- Block layout (`recomp::layout`): blocks are emitted in a loop-aware topological
  order (reverse postorder, each natural loop contiguous behind its header) instead
  of by address, so a forward jump is a `br` out of nested blocks and a back edge a
  `br` to a wasm `loop`; inlined callees follow their call site. Before, every taken
  jump went through the function's `br_table` dispatcher (one indirect branch shared
  by all of them). Irreducible functions keep the address order. A 3055 death frame:
  dispatcher jumps 2.17 M -> ~100, harness 11.9 -> 9.1 ms.
- Inline x87 (`lift::x87_inline_form`): loads, stores, fist/fistp, fxch, fchs/fabs,
  the four operations, compares and fnstsw run the helpers' fast path in the
  translated function. The block address, TOP and the fast-path conditions are read
  once per run of x87 code; registers written earlier in the run skip their tag
  checks; anything a check rejects runs out of line as before, and so does the rest
  of the run. Exact zero products and quotients stay inline (silent audio samples
  sent the mixer out of line). Out-of-line x87 per audio frame 307 k -> 73 k of 1 M;
  GMS's audio thread per frame Chrome 3.45 -> 2.35 ms, Firefox 3.2 -> 2.2 ms.
- `fldpi` and the other load constants were parsed from decimal strings on every
  execution (5 % of a Vandal death frame in V8); they are parsed once.
- Translation profiles: the page keeps, per game (IndexedDB), the functions sessions
  translated when they ran, with a hash of their first 32 bytes and the frame they
  were first needed in; the next session translates those of the first 300 frames at
  its first wait after the first frame (packers have unpacked by then), at most 2 s,
  skipping any whose code differs (`engine::profile_batch`), and the rest while the
  game waits, before any other ahead translation.
  3055's room-load frames 76 and 104: ~70 ms -> ~25 ms (the game's own loading),
  for a ~75 ms burst at frame 10. Entries needed before the first frame whose code
  already matches before any guest code runs (a packer's code cannot match until it
  unpacks) are translated then, in full batches (`engine::profile_prefetch`): 3055's
  first frame 4.5 -> 1.9 s in Firefox. `?noprofile` turns it off.
- Ahead translation fits a batch's cost as fixed + per byte (`fit_idle_cost`); the
  per-byte rate alone had folded in each module's fixed compile cost and overstated
  small batches threefold, so waits rarely fit a module. 3055 first session: ahead
  translation 0.19 -> 1.25 s (Firefox), 1.1 k -> 7.6 k functions; frame 76 72 -> 26 ms.
- Host work per frame: the renderer compares a draw's state word by word (it built a
  ~60-number string key per draw); `run_threads` reads the host clock only when a
  thread will run (it was ~1 100 calls out of wasm per frame); the pacing estimate
  is sorted once per frame; memcpy copies inside guest memory in one step (glue
  `gcopy`). Firefox, Vandal steady state: busy time -16 %, median 2.50 -> 2.08 ms.
- Steady state now (second half of the Vandal replay): Firefox median 1.9-2.1 ms,
  p95 6.2-6.5 ms; Chrome median 1.3 ms, p95 4.6 ms; frame intervals 20.13 ms mean,
  1.4 ms standard deviation in both. A 3055 death frame: Chrome ~6.4 ms, Firefox
  ~7.1 ms, of which the audio thread ~2.2-2.4 ms.
- Diagnostics: `GMR_COUNT=blocks` (hot blocks, and out-of-line x87 instructions with
  the top address bit flipped), `GMR_API_COUNT=1`, monitor lines with API calls and
  dispatches per frame, `run-node.mjs --profile from:to` (a CPU profile of a frame
  range), `run-browser.sh` with `FFPREFS`, `web/memtest/callbench.html` (call
  costs by kind).

## Importing in the browser

`crates/webimport` (built to `/import/webimport.wasm`) does what the native importer
does, in memory, for a zip, 7z or RAR (`sevenz-rust2`, `rars`): unpack (names in UTF-8,
Shift-JIS or cp437; a single top folder stripped), unwrap an IExpress package (its
cabinet, MSZIP or LZX, each folder decompressed once), choose the entry exe, detect the runner, and name the system DLLs
to add, which the page fetches from `/system32/` (`web/tools/system32.sh`: Wine's PE
builds, debug sections stripped, PE timestamp and checksum kept so imports are
reproducible) until nothing new is needed. `web/src/import.mjs` drives it; the game
is stored in IndexedDB (`saves.mjs`, store `games`) and plays with
`?game=<slug>&local`; `?zip=<url>` imports and starts one. Against the natively
imported corpus every zip, 7z and RAR source gives the same entry, runner and files
(byte for byte where checked), and 3055, 8309, Crimson Needle 3 and Campaign (RAR)
the same state hashes. NSIS installers still need the native importer (7z).

## Known gaps

- Shaded draws are verified with the test program above, not yet in a game scene.
- CPUID does not report MMX (MMX is lifted, for code that uses it without asking);
  code with an SSE or scalar fallback takes that.
- GMS 1.4 runs at 49.7-49.9 fps instead of 50: it measures each frame from a
  timestamp taken after the frame's own clock reads, and every
  QueryPerformanceCounter advances the virtual clock 1-4 µs (so spin-waits end), so
  20.03 ms per frame in menus, ~20.13 ms in play. Sub-microsecond steps for reads
  outside a spin would fix it; it changes every GMS game's state hashes.
- x87 deviations in rare edges: fsqrt of extended-precision denormals, fidivr with
  precision control 01 (see `r1-cpu.md`).
- No networking: ws2_32 reports the network as not ready, wininet fails to open; Steam is not loaded.
- Frame pacing needs JSPI; without it the game runs unpaced.
