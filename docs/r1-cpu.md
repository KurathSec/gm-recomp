# R1: CPU core (x86 → wasm) and its differential tests

Crates: `recomp` (lifter), `fpu` (x87 model), `harness` (tests).

## What the lifter produces

- One wasm function per guest function (`abi::export_name(entry)`), type `() -> ()`.
  CPU state lives in mutable globals exported by a per-thread `cpu` instance (eax..edi, eip,
  the six arithmetic flags, DF, fs_base, mxcsr, xmm0-7 as v128) and imported by every code
  module; guest memory is the imported memory 0 so loads/stores are plain `i32.load/store`.
- Registers are cached in locals for the function and spilled around calls, traps and exits.
- Flags are lazy: an ALU result is kept as (op, operands, result) and individual bits are
  only computed when read; liveness over the function CFG decides what to materialize at
  block boundaries. Compares feed `jcc/setcc/cmov` directly from the operands.
- Control flow inside a function is a `br_table` dispatcher over its basic blocks; direct
  calls are wasm `call`s (same module) or `call_indirect` through `rt.resolve`; `ret` sets
  `eip` and returns, and each call site checks that `eip` is its continuation (an unwind
  keeps returning until a frame matches). `jmp reg` is a `return_call_indirect`.
- x87 instructions call `rt.fpu(code, a, b, ip, fop)`; SSE is emitted inline on wasm SIMD.
- Unsupported instructions become `rt.trap(UNIMPLEMENTED, ip)` and are listed by the
  translator, so a module always translates.

Runtime services (`abi::RtFunc`): `resolve`, `trap`, `fpu`, `cpuid`, `rdtsc`.

## Differential testing

`cargo run --release -p harness -- cpu-diff --mode <instr|flow|mixed|x87|x87trans|sse> --count N --seed S`

- Reference: `tests/cpu/refrun_linux.c`, a freestanding 32-bit Linux executable (no libc, no
  Wine). Each case runs on the host CPU until its code hits `hlt`; the signal handler records
  registers, EFLAGS, the kernel's FXSAVE image and memory. Wine is not used for CPU tests
  (its exception dispatcher crashes on these programs).
- Cases are generated from iced-x86 `Code` encodings with random operands; `flow` mode builds
  small programs with branches, loops, direct and register calls, stack traffic and `rep`
  string operations. The data block, stack window and code live at the same addresses on
  both sides.
- Comparison masks flags iced reports as undefined (path-aware over the program's CFG);
  cases where a later instruction reads an undefined flag are reported as indeterminate.
- `probe` runs one program with chosen ST registers / memory and prints both states;
  `translate` and `wat` print generated modules.

Results at commit time (5000 cases per run, two seeds each): integer `mixed`, `flow` and
`sse`: 0 failures; `x87`: ~0.15 % (7–9 per 5000, mostly random `fldenv`/`frstor` images); `x87trans`: ~0.7 % outside the
2^-44 tolerance.

## Semantics measured on hardware and reproduced

- x87: precision control (24/53/64) through a 112-bit sticky intermediate (exact single
  rounding in every rounding mode); C1 "rounded up" by magnitude; FCW bit 6 reads as 1;
  FOP/FDP only latch on unmasked exceptions, FCS/FDS read as 0; constants rounded by RC only;
  NaN ties go to the positive NaN; stack underflow produces the negative indefinite and takes
  precedence over NaN propagation and denormal checks; `fprem` reduces by 32·(D/32−1) bits
  when the exponent gap is ≥ 64; unmasked exceptions abort the instruction (no pop, no store)
  and fault on the next waiting instruction; `fninit`/`fnsave` keep register images; 32-bit
  environment images carry 0xFFFF in the reserved halves.
- SSE: `min/max` return the source on NaN or equal zeros; conversions give 0x80000000 for
  NaN/out-of-range; packed shifts saturate on counts ≥ lane width.

## Known deviations

- Transcendental x87 instructions (`fsin/fcos/fsincos/fptan/fpatan/fyl2x/fyl2xp1/f2xm1`) are
  evaluated through f64: ~53 significant bits instead of ~64.
- SSE NaN payloads follow wasm (canonical), not the x86 "first operand" rule; MXCSR exception
  flags are not updated; `rcpps/rsqrtps/rcpss/rsqrtss` are not implemented (hardware
  approximations differ between vendors anyway).
- MMX (`mm` registers), 3DNow!, SSSE3/SSE4 forms are not lifted; they only occur on CPUID-
  guarded paths in the corpus. The runtime's CPUID will not advertise them.
- `lock` prefixes are ignored (single-threaded until R5).
