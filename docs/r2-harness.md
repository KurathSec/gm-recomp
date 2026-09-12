# R2: GameMaker 8 runners in the native harness

`harness run <web/public/games/slug>` loads the game's exe (and any game DLLs it
imports), recompiles code on demand (or everything discovered with `--aot`), and runs
it under wasmtime against the HLE runtime (`crates/runtime`). No game-specific code.

## Status

| Game | Engine | Result |
|---|---|---|
| Not Another Needle Game | GM8.1 (Delphi 2009+) | 300 frames, 50 fps virtual, 8 draw calls/frame; supersound.dll and the extracted D3DX8.dll recompiled and running |
| i wanna uhuhu spike | GM8.0 (Delphi 7) | 300 frames, 50 fps virtual |

- Repeated runs end in the same state hash (registers + all non-zero guest memory).
- `--aot` (all 20 095 discovered functions of the GM8.1 runner, 3.1 s to translate)
  and on-demand translation end in the identical state hash after 300 frames.
- CPU differential suites (instr/flow/mixed/sse/x87) still pass 100 %.

## Running

```
harness run <game dir> [--frames N] [--trace] [--aot] [--seconds S] [--recent K]
                       [--dump addr:len] [--keep-writes]
```

- `--trace` prints every API call with arguments, return values and nested guest
  callbacks; the summary lists Delphi exception classes/messages, text the game puts in
  edit controls (GM's error form) and unimplemented APIs.
- Files the game writes go to `target/gamedata/<slug>/` (copy-on-write over the
  imported files), cleared per run unless `--keep-writes`.
- `--seconds` is a wall-clock watchdog (wasmtime epoch interruption); the backtrace
  names translated functions (`f_<address>`).

## Execution model

- Each guest function is a wasm function; `ret` stores the continuation in `eip` and
  returns; callers check it and return further when it differs (non-local control
  flow such as Delphi's `push addr; ret` finally blocks, SEH continuation, longjmp).
  The dispatcher then resumes at `eip`, translating the target as a new function.
- Calls to code outside the current module go through `rt.resolve` (guest address ->
  function table index), behind an inline cache in guest memory (`abi::RESOLVE_CACHE`).
- HLE stubs live at `0x7F000000 + id*16`; `resolve` on a stub runs the API and returns
  a no-op function. Callbacks into the guest (`call_guest`) push a per-depth marker
  return address in the `0x7FFF0000` page and run a nested dispatcher.
- Static TLS (`fs:[2Ch]`), SEH (`fs:[0]` chain with RtlUnwind), and hardware faults
  raised from translated code (`#DE`, `#UD`, unmasked x87 exceptions) go through the
  guest's handlers.

## Virtual time

All clocks read one virtual clock. `Sleep`, waits and idle message loops advance it.
Clock reads issued back to back with no other API in between (a spin-wait on the time,
as GM8's frame limiter does) advance it in doubling steps of 1..4 µs, so polling
loops end deterministically and overshoot their deadline by at most 4 µs (GMS 1.4
spins the last ~11 ms of each frame and times the next one from where the spin
ended; 128 µs steps had it running at 49 fps).
DirectSound play cursors and notification events follow the same clock.

## Findings worth keeping

- Translated functions must start at their entry block, which need not be the lowest
  address (the dispatcher can enter mid-function at a continuation).
- Delphi's `Move` indexes its jump table with negative values; jump tables fall back
  to a memory-indirect jump outside the discovered range.
- Delphi `MakeObjectInstance` thunks (`call` into code that pops the return address)
  are followed by data; such calls end the block.
- GM8.1 extracts D3DX8.dll at run time and `LoadLibrary`s it: DLLs found on the guest
  file system are loaded and recompiled; only core system DLLs are always HLE.
- ImageList_Write puts only the header size in `bfSize`; the bitmap length must come
  from the BITMAPINFOHEADER.
- The GM8 runners instantiate a `TWebBrowser` (Shell.Explorer ActiveX) and DirectMusic
  at startup; both exist as minimal HLE COM objects.

## Not done in R2

- Lockstep comparison against an independent CPU and API-trace comparison with Wine
  (Wine is not used; see R1). The differential suites, determinism and AOT/JIT
  agreement are the current checks.
- Guest threads are created but not run (GM8.1 starts one at startup; supersound's
  streaming does not need it so far). MCI (GM8.0 media player) fails cleanly.
- No rendering, audio output or input yet: those are R3/R4 in the browser.
