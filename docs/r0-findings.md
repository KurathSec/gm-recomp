# R0 findings: corpus, code discovery, browser feasibility

Inputs: the 9 imported games under `web/public/games/` (not committed).

## Corpus

| slug | runner | packaging | distinct code image |
|---|---|---|---|
| i-wanna-uhuhu-spike | GM8.0 (Delphi) | exe + overlay | `d4bf621b…` |
| not-another-needle-game-1-12, i-wanna-bread | GM8.1 (Delphi) | exe + overlay | `4ef0fff5…` (identical runner in both) |
| indigo-needle | GMS 1.4.1657 VM, bc14 | loose data.win | `d91c3a2c…` |
| i-wanna-run-the-marathon-v1-2 | GMS 1.4.1757 VM, bc15 | IExpress CAB | `558643a5…` |
| crimson-needle-3 | GMS 1.4.1804 VM, bc16 | IExpress CAB | `ee…` |
| i-wanna-be-the-vandal | GMS 1.4.9999 VM, bc16 | IExpress CAB | `9e41bb41…` |
| untitled-needle-game-v1-4 | GMS 1.4.9999 **YYC** | 7z (named .zip) → IExpress | game-specific |
| rng-dungeon-v1-0 | GMS 1.4.9999 **YYC** | RAR5 → IExpress | game-specific |

- Every GMS game ships the same `D3DX9_43.dll` (`904ea5fa…`); it is recompiled once.
- YYC builds embed the FORM/GEN8 blob in the exe's `.data` and add `gmsched.dll` (274 bytes of code).
- indigo-needle ships `msvcr100d.dll`/`msvcp100d.dll` (debug CRT) and the `GMFile.dll` extension.
- No packer or self-modifying-code hints in any module.
- API surface (static + delay-load + probable `GetProcAddress` targets): **1438 functions across 41 DLLs**; full table in `corpus-inventory.md`.

## Static code discovery (`importer discover`)

Method: recursive descent from entry/exports/TLS callbacks, then speculative roots
(relocation targets, immediates, pointer scan for images without relocations, a
`call rel32` sweep over undecoded bytes, prologue scan). A speculative root is only
committed when its whole function decodes cleanly and never lands inside an already
decoded instruction. Resolved `jmp [idx*4+table]` tables count as data.

| module | exec KB | discovered | notes |
|---|---|---|---|
| GMS 1.4 VM runners (4 builds) | 2426–2990 | **89.8–90.1 %** | 83–104 unresolved `jmp reg` / `jmp [global]` sites (virtual tail calls, function-pointer globals) |
| YYC exes | 4373 / 9407 | **97.1 % / 98.3 %** | LLVM output; gaps are small |
| D3DX9_43.dll | 1825 | 92.3 % | |
| GMFile.dll / supersound.dll | 242 / 117 | 97.0 % / 92.3 % | |
| GM8.0 runner | 1564 | 82.5 % | Delphi |
| GM8.1 runner | 2535 | 64.5 % | Delphi; `.text` holds VMTs, RTTI and string constants (top gaps: 62 % pointer-like, 18 % text) |
| msvcr100d / msvcp100d | 1399 / 683 | 71.7 % / 63.6 % | EH tables and float constants live in `.text` |

Corpus-wide: 38.8 MB of 43.7 MB executable bytes (88.9 %) are reached statically, with
zero bad decodes after the speculative-commit rule. The rest is a mix of data that compilers
place in code sections (Delphi, MSVC CRT) and code reachable only through register
jumps; the runtime JIT plus profile-guided AOT covers the latter.

Known limitation: the Delphi figure is a lower bound because VMT/RTTI data in `.text` is
not yet separated out, so "undiscovered" overstates the missing code.

## Browser feasibility (`web/memtest/`, headless, cross-origin isolated)

| capability | Firefox 158 | Chrome 155 |
|---|---|---|
| wasm32 memory, `maximum` 65536 pages (4 GB) | accepted | accepted |
| grown to (test stepped in 256 MB increments) | ≥ 3.75 GB, shared and unshared | ≥ 3.75 GB, shared and unshared |
| multi-memory | works | works |
| exception handling (legacy `throw`) | yes | yes |
| JSPI (`WebAssembly.Suspending`) | yes | yes |
| memory64 | yes | yes |
| SharedArrayBuffer, `Atomics.waitAsync`, OffscreenCanvas, AudioWorklet | yes | yes |
| WebGL2 via OffscreenCanvas | yes (software in headless) | yes (software in headless) |
| OPFS `createSyncAccessHandle` | not exposed on the main thread; verify inside a Worker in R3 | same |
| JS strided read over 1 GB shared memory | 3.6 GB/s | 7.0 GB/s |

## Decision: guest address space

**Multi-memory.** The runtime (Rust) keeps its own memory 0; the guest's 32-bit address space
is a separate shared memory with `maximum` 4 GB, imported by every recompiled module as *its*
memory 0 so guest loads/stores compile to plain `i32.load/store` with no base offset.

- The runtime reaches guest memory through a small hand-written glue module that imports both
  memories and exports typed accessors plus cross-memory `memory.copy`; Rust cannot address a
  second memory directly. Argument reads for API calls are a few direct wasm calls, bulk I/O is
  one `memory.copy`.
- Why not one memory with the runtime in the upper 2 GB: a wasm memory must be contiguous from
  0, so the runtime's data would force the memory to be ≥ 2 GB at instantiation, and Rust's
  linker layout at such a base is unverified. Separate memories avoid both issues.
- Guest memory is grown on demand as the guest commits regions (`VirtualAlloc`, module maps);
  Windows 32-bit user space is below 2 GB (3 GB with the large-address-aware flag; none of the
  runners set it), so the shared memory's reservation stays within what both browsers accepted.

## Toolchain notes

- Fedora's `7z` cannot read RAR5 and the `zip` crate rejects 7z-in-.zip; the importer identifies
  archives by signature and falls back to `bsdtar`.
- Wine 11.0 (staging) is installed natively; Wine's PE builds live in
  `/usr/lib64/wine-wow64/wine/i386-windows/` and supply export-name lists for the inventory.
- Headless Chrome runs via `flatpak run com.google.Chrome --headless=new`; Firefox via
  `firefox --headless --no-remote --profile <tmp>`.
