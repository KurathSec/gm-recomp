//! Instruction semantics: decoded x86 -> wasm, one guest function at a time.
//!
//! Registers are cached in wasm locals for the duration of a function and
//! spilled to the shared globals around calls and at exits. Arithmetic flags
//! are kept lazily (see `flags.rs`) and only computed when something reads
//! them. Control flow inside a function is a `br_table` dispatcher over the
//! function's basic blocks.

use crate::abi::{self, Global, RtFunc, trap, types};
use crate::cfg::{Block, Cfg, Term};
use crate::flags::{self, FlagOp};
use iced_x86::{ConditionCode, Instruction, Mnemonic, OpKind, Register};
use std::borrow::Cow;
use crate::fasthash::HashMap;
use wasm_encoder::{BlockType, Function, Instruction as W, MemArg, ValType};

// Local indices.
pub(crate) const L_REG: u32 = 0; // eax..edi = 0..7
pub(crate) const L_FLAG: u32 = 8; // cf,pf,af,zf,sf,of = 8..13
pub(crate) const L_BLK: u32 = 14;
pub(crate) const L_FA: u32 = 15;
pub(crate) const L_FB: u32 = 16;
pub(crate) const L_FR: u32 = 17;
pub(crate) const L_FC: u32 = 18;
pub(crate) const L_T0: u32 = 19;
pub(crate) const L_T1: u32 = 20;
pub(crate) const L_T2: u32 = 21;
pub(crate) const L_T3: u32 = 22;
pub(crate) const L_R0: u32 = 23; // scratch of the resolve-cache probe
pub(crate) const L_R1: u32 = 24;
pub(crate) const L_Q0: u32 = 25;
pub(crate) const L_Q1: u32 = 26;
pub(crate) const L_D0: u32 = 27;
pub(crate) const L_V0: u32 = 28;
pub(crate) const L_V1: u32 = 29;
pub(crate) const L_MM: u32 = 30; // MMX register access: x87 block address, mantissa
pub(crate) const L_MQ: u32 = 31;
// Inline x87 (`x87_inline_form`): the block's address, TOP and what may run inline
// (`fpu::block::FAST` bits, `X87_QUICK`; 0 after a fallback), as read at the last
// sync; f64 operands and result.
pub(crate) const L_XF: u32 = 32;
pub(crate) const L_XT: u32 = 33;
pub(crate) const L_XQ: u32 = 34;
pub(crate) const L_XA: u32 = 35;
pub(crate) const L_XB: u32 = 36;
pub(crate) const L_XR: u32 = 37;
/// The return address the function was entered with (functions with `jmp r/m32`).
pub(crate) const L_RA: u32 = 38;
/// Locals after the parameters: the eight registers are the function's parameters
/// (locals 0..7) and its results (`types::FUNC`).
pub const LOCALS: [(u32, ValType); 9] = [
    (17, ValType::I32),
    (2, ValType::I64),
    (1, ValType::F64),
    (2, ValType::V128),
    (1, ValType::I32),
    (1, ValType::I64),
    (3, ValType::I32),
    (3, ValType::F64),
    (1, ValType::I32),
];
/// `L_XQ` bit: the precision flag is set and rounding is to nearest (with C1 dead,
/// the x87 helpers' quick lane).
const X87_QUICK: u32 = 16;

/// A memory operand at a fixed offset (the x87 block's fields).
fn xma(off: u32) -> MemArg {
    MemArg { offset: off as u64, align: 0, memory_index: 0 }
}

pub(crate) fn mem0() -> MemArg {
    MemArg { offset: 0, align: 0, memory_index: 0 }
}

pub(crate) fn mask(w: u8) -> u32 {
    if w == 32 { 0xffff_ffff } else { (1u32 << w) - 1 }
}

/// Where an instruction's destination lives, after its address has been computed.
#[derive(Clone, Copy)]
enum Dst {
    Reg(Register),
    /// Address already stored in L_T0.
    Mem,
    None,
}

const X87_UNKNOWN: u8 = 0;
/// Holds an f64 (`fpu::block::VALID` without an extended image).
const X87_F64: u8 = 1;
const X87_EMPTY: u8 = 2;

/// A label around the code being emitted, by what a branch to it continues at.
enum Label {
    /// The end of a block: the code of the block at this position follows.
    Block(u32),
    /// A loop whose header block is at this position.
    Loop(u32),
    /// The dispatcher loop (`L_BLK` selects the block).
    Dispatch,
}

pub struct FuncEmit<'a> {
    pub f: Function,
    pub(crate) cfg: &'a Cfg,
    /// Block start address -> position in `order` (the dispatch index).
    index: HashMap<u32, u32>,
    pub(crate) order: Vec<u32>,
    /// Loops nested over `order` (taken by `emit`).
    items: Vec<crate::layout::Item>,
    /// Labels open around the current block's code, outermost first.
    labels: Vec<Label>,
    live_out: HashMap<u32, u8>,
    /// Flags the entry block reads before writing (loaded from the globals).
    entry_flags: u8,
    /// Direct-call targets available in this module: entry -> wasm function index.
    func_index: &'a HashMap<u32, u32>,
    pub(crate) pending: Option<(FlagOp, u8)>,
    /// Report entry to the runtime (debugging).
    pub probe: bool,
    /// Call the `x87` module's fast paths before `rt.fpu`.
    pub x87_inline: bool,
    /// The inline x87 locals need reading again (block start, after calls and x87
    /// instructions that ran out of line).
    x87_sync: bool,
    /// What the inline x87 code knows of each register since the last sync, by slot
    /// relative to TOP then (`X87_*`), and TOP relative to it.
    x87_known: [u8; 8],
    x87_top: u32,
    pub resolve_cache: bool,
    pub count: bool,
    pub count_blocks: bool,
    /// Flags some later instruction (or a successor) will read after the current one.
    pub(crate) needed: u8,
    pub(crate) ip: u32,
    pub(crate) next_ip: u32,
    /// Nesting depth of `if`/`block` constructs opened inside the current block's code.
    pub(crate) depth: u32,
    pub(crate) cur_block: usize,
    pub unimplemented: Vec<(u32, String)>,
    /// x86-64 code (`lift64.rs`): the registers are sixteen i64 parameters, and the
    /// locals above (`L_FLAG` on) sit `lbase` further on.
    pub(crate) x64: bool,
    pub(crate) lbase: u32,
    /// The full address of the next instruction (64-bit code pushes it).
    pub(crate) next_ip64: u64,
}

impl<'a> FuncEmit<'a> {
    pub(crate) fn ip(&self) -> u32 {
        self.ip
    }

    pub fn new(cfg: &'a Cfg, func_index: &'a HashMap<u32, u32>) -> Self {
        let crate::layout::Layout { order, items } = crate::layout::layout(cfg);
        let index = order.iter().enumerate().map(|(i, &a)| (a, i as u32)).collect();
        let (live_in, live_out) = liveness(cfg);
        let entry_flags = live_in.get(&cfg.entry).copied().unwrap_or(flags::ALL);
        FuncEmit {
            f: Function::new(LOCALS.to_vec()),
            cfg,
            index,
            order,
            items,
            labels: Vec::new(),
            live_out,
            entry_flags,
            func_index,
            pending: None,
            probe: false,
            x87_inline: true,
            x87_sync: true,
            x87_known: [X87_UNKNOWN; 8],
            x87_top: 0,
            resolve_cache: false,
            count: false,
            count_blocks: false,
            needed: 0,
            ip: 0,
            next_ip: 0,
            depth: 0,
            cur_block: 0,
            unimplemented: Vec::new(),
            x64: false,
            lbase: 0,
            next_ip64: 0,
        }
    }

    /// A 64-bit function (`types::FUNC64`).
    pub fn new64(cfg: &'a Cfg, func_index: &'a HashMap<u32, u32>) -> Self {
        let mut fe = Self::new(cfg, func_index);
        fe.f = Function::new(crate::lift64::locals64());
        fe.x64 = true;
        fe.lbase = crate::lift64::LBASE64;
        fe
    }

    pub(crate) fn i(&mut self, ins: W) {
        self.f.instruction(&ins);
    }

    // (the commonest instructions straight into the sink: encoding through
    // `Instruction` showed in translation profiles)
    pub(crate) fn c(&mut self, v: u32) {
        self.f.instructions().i32_const(v as i32);
    }

    pub(crate) fn get(&mut self, l: u32) {
        self.f.instructions().local_get(l + self.lbase);
    }

    pub(crate) fn set(&mut self, l: u32) {
        self.f.instructions().local_set(l + self.lbase);
    }

    pub(crate) fn tee(&mut self, l: u32) {
        self.f.instructions().local_tee(l + self.lbase);
    }

    /// Local `l` itself (64-bit code's registers and `lift64::X_*`), not moved by `lbase`.
    pub(crate) fn lget(&mut self, l: u32) {
        self.f.instructions().local_get(l);
    }

    pub(crate) fn lset(&mut self, l: u32) {
        self.f.instructions().local_set(l);
    }

    pub(crate) fn ltee(&mut self, l: u32) {
        self.f.instructions().local_tee(l);
    }

    /// The type of translated functions in this module.
    pub(crate) fn fty(&self) -> u32 {
        if self.x64 { types::FUNC64 } else { types::FUNC }
    }

    /// Number of register parameters.
    fn nregs(&self) -> u32 {
        if self.x64 { 16 } else { 8 }
    }

    /// The global of register parameter `r`.
    fn reg_global(&self, r: u32) -> u32 {
        if self.x64 { Global::Rax as u32 + r } else { Global::Eax as u32 + r }
    }

    // ---- prologue / epilogue -------------------------------------------------

    fn load_state(&mut self) {
        // the registers come as parameters; only the flags read before written are
        // loaded (a call does not hand flags over)
        for fl in 0..6 {
            if self.entry_flags & (1 << fl) != 0 {
                self.i(W::GlobalGet(Global::Cf as u32 + fl));
                self.set(L_FLAG + fl);
            }
        }
    }

    fn spill_regs(&mut self) {
        for r in 0..self.nregs() {
            self.lget(L_REG + r);
            self.i(W::GlobalSet(self.reg_global(r)));
        }
    }

    pub(crate) fn spill_flags(&mut self) {
        self.materialize(flags::ALL);
        for fl in 0..6 {
            self.get(L_FLAG + fl);
            self.i(W::GlobalSet(Global::Cf as u32 + fl));
        }
    }

    fn reload_regs(&mut self) {
        for r in 0..self.nregs() {
            self.i(W::GlobalGet(self.reg_global(r)));
            self.lset(L_REG + r);
        }
    }

    fn reload_flags(&mut self) {
        for fl in 0..6 {
            self.i(W::GlobalGet(Global::Cf as u32 + fl));
            self.set(L_FLAG + fl);
        }
        self.pending = None;
    }

    pub(crate) fn spill_all(&mut self) {
        self.spill_regs();
        self.spill_flags();
    }

    pub(crate) fn reload_all(&mut self) {
        self.reload_regs();
        self.reload_flags();
    }

    /// Pushes the registers: a call's arguments or the function's results.
    fn push_regs(&mut self) {
        for r in 0..self.nregs() {
            self.lget(L_REG + r);
        }
    }

    /// Returns the registers in the locals.
    pub(crate) fn ret_regs(&mut self) {
        self.push_regs();
        self.i(W::Return);
    }

    /// Returns the registers in the globals (the context the runtime holds).
    fn ret_globals(&mut self) {
        for r in 0..self.nregs() {
            self.i(W::GlobalGet(self.reg_global(r)));
        }
        self.i(W::Return);
    }

    /// Takes a call's results into the register locals.
    fn pop_regs(&mut self) {
        for r in (0..self.nregs()).rev() {
            self.lset(L_REG + r);
        }
    }

    /// Bumps the count of `addr` in `abi::ENTRY_COUNTS`: [address, count] in its slot.
    fn count_at(&mut self, addr: u32) {
        let slot = crate::abi::ENTRY_COUNTS + 8 * ((addr.wrapping_mul(crate::abi::RESOLVE_HASH) >> 19) & (crate::abi::ENTRY_SLOTS - 1));
        self.c(slot);
        self.c(addr);
        self.i(W::I32Store(mem0()));
        self.c(slot + 4);
        self.c(slot + 4);
        self.i(W::I32Load(mem0()));
        self.c(1);
        self.i(W::I32Add);
        self.i(W::I32Store(mem0()));
    }

    /// Bumps event counter `k` (`abi::COUNT_*`) when counting.
    fn count(&mut self, k: u32) {
        if self.count {
            let a = crate::abi::COUNTERS + 4 * k;
            self.c(a);
            self.c(a);
            self.i(W::I32Load(mem0()));
            self.c(1);
            self.i(W::I32Add);
            self.i(W::I32Store(mem0()));
        }
    }

    /// [guest address] -> [table index]. The runtime is asked only when the resolve
    /// cache misses; it may run an API there (the target is a stub), which works on
    /// the registers in the globals, so they go there and come back around it.
    fn resolve(&mut self) {
        self.count(crate::abi::COUNT_RESOLVES);
        if self.resolve_cache {
            use crate::abi::{RESOLVE_CACHE, RESOLVE_CACHE_BITS, RESOLVE_HASH};
            let at = |off: u32| MemArg { offset: off as u64, align: 2, memory_index: 0 };
            self.set(L_R0);
            self.get(L_R0);
            self.i(W::I32Const(RESOLVE_HASH as i32));
            self.i(W::I32Mul);
            self.c(32 - RESOLVE_CACHE_BITS);
            self.i(W::I32ShrU);
            self.c(3);
            self.i(W::I32Shl);
            self.tee(L_R1);
            self.i(W::I32Load(at(RESOLVE_CACHE)));
            self.get(L_R0);
            self.i(W::I32Eq);
            self.i(W::If(BlockType::Result(ValType::I32)));
            self.get(L_R1);
            self.i(W::I32Load(at(RESOLVE_CACHE + 4)));
            self.i(W::Else);
            self.spill_regs();
            self.get(L_R0);
            self.call_rt(RtFunc::Resolve);
            self.reload_regs();
            self.i(W::End);
        } else {
            self.set(L_R0);
            self.spill_regs();
            self.get(L_R0);
            self.call_rt(RtFunc::Resolve);
            self.reload_regs();
        }
    }

    pub(crate) fn call_rt(&mut self, f: RtFunc) {
        self.i(W::Call(f as u32));
    }

    pub(crate) fn trap(&mut self, kind: i32, arg: u32) {
        self.spill_all();
        // The faulting instruction's address is part of the context the runtime sees.
        self.c(self.ip);
        self.i(W::GlobalSet(Global::Eip as u32));
        self.i(W::I32Const(kind));
        self.c(arg);
        self.call_rt(RtFunc::Trap);
        self.return_if_ctl(false);
        self.reload_all();
    }

    /// `if (ctl) return;` — the runtime wants control back. `spill` first stores the
    /// registers held in locals (call sites that did not spill before the call);
    /// otherwise the globals hold the context to return.
    pub(crate) fn return_if_ctl(&mut self, spill: bool) {
        self.i(W::GlobalGet(Global::Ctl as u32));
        self.i(W::If(BlockType::Empty));
        if spill {
            // Materializing flags in this branch must not change what the code after
            // the `if` believes about them.
            let pending = self.pending.clone();
            self.spill_all();
            self.pending = pending;
            self.c(self.ip);
            self.i(W::GlobalSet(Global::Eip as u32));
            self.ret_regs();
        } else {
            self.ret_globals();
        }
        self.i(W::End);
    }

    pub(crate) fn unimplemented(&mut self, instr: &Instruction) {
        let mut text = String::new();
        iced_x86::FastFormatter::new().format(instr, &mut text);
        self.unimplemented.push((self.ip, text));
        self.trap(trap::UNIMPLEMENTED, self.ip);
    }

    // ---- registers -------------------------------------------------------

    fn reg_local(r: Register) -> u32 {
        L_REG + (r.full_register32() as u32 - Register::EAX as u32)
    }

    fn is_high8(r: Register) -> bool {
        matches!(r, Register::AH | Register::CH | Register::DH | Register::BH)
    }

    /// Pushes the register's value zero-extended to i32.
    pub(crate) fn read_reg(&mut self, r: Register) {
        if self.x64 {
            return self.read_reg32_x64(r);
        }
        let l = Self::reg_local(r);
        self.get(l);
        match r.size() {
            4 => {}
            2 => {
                self.c(0xffff);
                self.i(W::I32And);
            }
            1 => {
                if Self::is_high8(r) {
                    self.c(8);
                    self.i(W::I32ShrU);
                }
                self.c(0xff);
                self.i(W::I32And);
            }
            n => self.bad_width(n as u8 * 8),
        }
    }

    /// Stores local `src` into the register, merging partial widths.
    pub(crate) fn write_reg_from(&mut self, r: Register, src: u32) {
        if self.x64 {
            return self.write_reg32_x64(r, src);
        }
        let l = Self::reg_local(r);
        match r.size() {
            4 => {
                self.get(src);
                self.set(l);
            }
            2 => {
                self.get(l);
                self.c(0xffff_0000);
                self.i(W::I32And);
                self.get(src);
                self.c(0xffff);
                self.i(W::I32And);
                self.i(W::I32Or);
                self.set(l);
            }
            1 => {
                let (keep, shift) = if Self::is_high8(r) { (0xffff_00ff, 8) } else { (0xffff_ff00, 0) };
                self.get(l);
                self.c(keep);
                self.i(W::I32And);
                self.get(src);
                self.c(0xff);
                self.i(W::I32And);
                if shift != 0 {
                    self.c(shift);
                    self.i(W::I32Shl);
                }
                self.i(W::I32Or);
                self.set(l);
            }
            n => self.bad_width(n as u8 * 8),
        }
    }

    // ---- memory ----------------------------------------------------------

    /// Pushes the effective address of the memory operand.
    pub(crate) fn addr(&mut self, instr: &Instruction) {
        if self.x64 {
            return self.addr_x64(instr);
        }
        let mut terms = 0;
        if instr.memory_base() != Register::None {
            let b = instr.memory_base();
            if b.is_gpr32() {
                self.get(Self::reg_local(b));
            } else {
                self.read_reg(b);
            }
            terms += 1;
        }
        if instr.memory_index() != Register::None {
            let ix = instr.memory_index();
            if ix.is_gpr32() {
                self.get(Self::reg_local(ix));
            } else {
                self.read_reg(ix);
            }
            let s = instr.memory_index_scale();
            if s > 1 {
                self.c(s.trailing_zeros());
                self.i(W::I32Shl);
            }
            if terms > 0 {
                self.i(W::I32Add);
            }
            terms += 1;
        }
        let disp = instr.memory_displacement32();
        if disp != 0 || terms == 0 {
            self.c(disp);
            if terms > 0 {
                self.i(W::I32Add);
            }
            terms += 1;
        }
        match instr.segment_prefix() {
            Register::FS => {
                self.i(W::GlobalGet(Global::FsBase as u32));
                self.i(W::I32Add);
            }
            Register::GS => {
                // GS is unused in 32-bit Windows; treat as zero base.
            }
            _ => {}
        }
        if instr.memory_base() == Register::ESP || instr.memory_base() == Register::EBP {
            // nothing special: the stack lives in guest memory like everything else
        }
        let _ = terms;
    }

    pub(crate) fn load(&mut self, w: u8) {
        self.i(match w {
            8 => W::I32Load8U(mem0()),
            16 => W::I32Load16U(mem0()),
            32 => W::I32Load(mem0()),
            _ => return self.bad_width(w),
        });
    }

    /// An operand width the integer paths do not handle (far pointers, or data that
    /// speculative discovery decoded as code): trap if it is ever executed.
    pub(crate) fn bad_width(&mut self, w: u8) {
        self.unimplemented.push((self.ip, format!("operand width {w}")));
        self.trap(trap::UNIMPLEMENTED, self.ip);
        self.i(W::Unreachable);
    }

    pub(crate) fn store(&mut self, w: u8) {
        self.i(match w {
            8 => W::I32Store8(mem0()),
            16 => W::I32Store16(mem0()),
            32 => W::I32Store(mem0()),
            _ => return self.bad_width(w),
        });
    }

    fn op_width(instr: &Instruction, n: u32) -> u8 {
        match instr.op_kind(n) {
            OpKind::Register => instr.op_register(n).size() as u8 * 8,
            OpKind::Memory => instr.memory_size().size() as u8 * 8,
            OpKind::Immediate8 | OpKind::Immediate8_2nd => 8,
            OpKind::Immediate16 | OpKind::Immediate8to16 => 16,
            _ => 32,
        }
    }

    /// Pushes operand `n` zero-extended. Memory operands use the address in L_T0 when
    /// `addr_in_t0`, otherwise compute it inline.
    pub(crate) fn read_op(&mut self, instr: &Instruction, n: u32, addr_in_t0: bool) {
        match instr.op_kind(n) {
            OpKind::Register => self.read_reg(instr.op_register(n)),
            OpKind::Memory => {
                if addr_in_t0 {
                    self.get(L_T0);
                } else {
                    self.addr(instr);
                }
                let w = (instr.memory_size().size() * 8) as u8;
                self.load(w);
            }
            OpKind::Immediate8 | OpKind::Immediate16 | OpKind::Immediate32 | OpKind::Immediate8to16 | OpKind::Immediate8to32
            | OpKind::Immediate8_2nd => {
                let w = Self::op_width(instr, 0).max(8);
                let v = instr.immediate(n) as u32 & mask(if instr.op_kind(n) == OpKind::Immediate8_2nd { 8 } else { w });
                self.c(v);
            }
            k => panic!("unsupported operand kind {k:?} at {:#x}", self.ip),
        }
    }

    /// For a destination operand: computes a memory address into L_T0 up front.
    fn prep_dst(&mut self, instr: &Instruction, n: u32) -> Dst {
        match instr.op_kind(n) {
            OpKind::Register => Dst::Reg(instr.op_register(n)),
            OpKind::Memory => {
                self.addr(instr);
                self.set(L_T0);
                Dst::Mem
            }
            _ => Dst::None,
        }
    }

    fn write_dst(&mut self, dst: Dst, w: u8, src: u32) {
        match dst {
            Dst::Reg(r) => self.write_reg_from(r, src),
            Dst::Mem => {
                self.get(L_T0);
                self.get(src);
                self.store(w);
            }
            Dst::None => {}
        }
    }

    fn read_dst(&mut self, dst: Dst, w: u8) {
        match dst {
            Dst::Reg(r) => self.read_reg(r),
            Dst::Mem => {
                self.get(L_T0);
                self.load(w);
            }
            Dst::None => self.c(0),
        }
    }

    // ---- flags -----------------------------------------------------------

    fn sext(&mut self, w: u8) {
        match w {
            8 => self.i(W::I32Extend8S),
            16 => self.i(W::I32Extend16S),
            _ => {}
        }
    }

    /// Pushes 0/1 for one lazily tracked flag.
    pub(crate) fn flag(&mut self, fl: u8) {
        let Some((op, w)) = self.pending else {
            self.get(L_FLAG + fl.trailing_zeros());
            return;
        };
        if self.x64 {
            return self.flag_x64(fl, op, w);
        }
        let w32 = w as u32;
        match fl {
            flags::ZF => {
                self.get(L_FR);
                self.i(W::I32Eqz);
            }
            flags::SF => {
                self.get(L_FR);
                self.c(w32 - 1);
                self.i(W::I32ShrU);
                self.c(1);
                self.i(W::I32And);
            }
            flags::PF => {
                self.get(L_FR);
                self.c(0xff);
                self.i(W::I32And);
                self.i(W::I32Popcnt);
                self.c(1);
                self.i(W::I32And);
                self.c(1);
                self.i(W::I32Xor);
            }
            flags::AF => match op {
                FlagOp::Add | FlagOp::Adc | FlagOp::Sub | FlagOp::Sbb | FlagOp::Inc | FlagOp::Dec => {
                    self.get(L_FA);
                    self.get(L_FB);
                    self.i(W::I32Xor);
                    self.get(L_FR);
                    self.i(W::I32Xor);
                    self.c(4);
                    self.i(W::I32ShrU);
                    self.c(1);
                    self.i(W::I32And);
                }
                _ => self.c(0),
            },
            flags::CF => match op {
                FlagOp::Add => {
                    self.get(L_FR);
                    self.get(L_FA);
                    self.i(W::I32LtU);
                }
                FlagOp::Adc => {
                    // carry-in ? res <= a : res < a
                    self.get(L_FR);
                    self.get(L_FA);
                    self.i(W::I32LeU);
                    self.get(L_FR);
                    self.get(L_FA);
                    self.i(W::I32LtU);
                    self.get(L_FC);
                    self.i(W::Select);
                }
                FlagOp::Sub => {
                    self.get(L_FA);
                    self.get(L_FB);
                    self.i(W::I32LtU);
                }
                FlagOp::Sbb => {
                    self.get(L_FA);
                    self.get(L_FB);
                    self.i(W::I32LeU);
                    self.get(L_FA);
                    self.get(L_FB);
                    self.i(W::I32LtU);
                    self.get(L_FC);
                    self.i(W::Select);
                }
                FlagOp::Logic => self.c(0),
                FlagOp::Inc | FlagOp::Dec => self.get(L_FLAG),
                FlagOp::Shl => {
                    // last bit shifted out = bit (w - n) of a, 0 when n > w
                    self.get(L_FA);
                    self.c(w32);
                    self.get(L_FB);
                    self.i(W::I32Sub);
                    self.i(W::I32ShrU);
                    self.c(1);
                    self.i(W::I32And);
                    self.c(0);
                    self.get(L_FB);
                    self.c(w32);
                    self.i(W::I32LeU);
                    self.i(W::Select);
                }
                FlagOp::Shr | FlagOp::Sar => {
                    // bit (n - 1) of the (sign- or zero-extended) source
                    self.get(L_FA);
                    if op == FlagOp::Sar {
                        self.sext(w);
                    }
                    self.get(L_FB);
                    self.c(1);
                    self.i(W::I32Sub);
                    self.i(W::I32ShrU);
                    self.c(1);
                    self.i(W::I32And);
                }
            },
            flags::OF => match op {
                FlagOp::Add | FlagOp::Adc => {
                    self.get(L_FA);
                    self.get(L_FR);
                    self.i(W::I32Xor);
                    self.get(L_FB);
                    self.get(L_FR);
                    self.i(W::I32Xor);
                    self.i(W::I32And);
                    self.c(w32 - 1);
                    self.i(W::I32ShrU);
                    self.c(1);
                    self.i(W::I32And);
                }
                FlagOp::Sub | FlagOp::Sbb => {
                    self.get(L_FA);
                    self.get(L_FB);
                    self.i(W::I32Xor);
                    self.get(L_FA);
                    self.get(L_FR);
                    self.i(W::I32Xor);
                    self.i(W::I32And);
                    self.c(w32 - 1);
                    self.i(W::I32ShrU);
                    self.c(1);
                    self.i(W::I32And);
                }
                FlagOp::Logic | FlagOp::Sar => self.c(0),
                FlagOp::Inc => {
                    self.get(L_FR);
                    self.c(1 << (w32 - 1));
                    self.i(W::I32Eq);
                }
                FlagOp::Dec => {
                    self.get(L_FR);
                    self.c((1 << (w32 - 1)) - 1);
                    self.i(W::I32Eq);
                }
                FlagOp::Shl => {
                    // defined for n == 1: msb(res) ^ msb(a); otherwise 0
                    self.get(L_FR);
                    self.get(L_FA);
                    self.i(W::I32Xor);
                    self.c(w32 - 1);
                    self.i(W::I32ShrU);
                    self.c(1);
                    self.i(W::I32And);
                    self.c(0);
                    self.get(L_FB);
                    self.c(1);
                    self.i(W::I32Eq);
                    self.i(W::Select);
                }
                FlagOp::Shr => {
                    self.get(L_FA);
                    self.c(w32 - 1);
                    self.i(W::I32ShrU);
                    self.c(1);
                    self.i(W::I32And);
                    self.c(0);
                    self.get(L_FB);
                    self.c(1);
                    self.i(W::I32Eq);
                    self.i(W::Select);
                }
            },
            _ => unreachable!(),
        }
    }

    /// Writes the given pending flags into their locals.
    pub(crate) fn materialize(&mut self, which: u8) {
        if self.pending.is_none() {
            return;
        }
        for fl in 0..6u32 {
            let bit = 1u8 << fl;
            if which & bit != 0 {
                self.flag(bit);
                self.set(L_FLAG + fl);
            }
        }
        // The others are dead (`which` comes from liveness): nothing reads them before
        // they are written again.
        self.pending = None;
    }

    /// Records a lazily evaluated flag result. The operands must already be in
    /// L_FA / L_FB (/ L_FC) and the masked result in L_FR.
    pub(crate) fn set_pending(&mut self, op: FlagOp, w: u8) {
        self.pending = Some((op, w));
    }

    pub(crate) fn set_flag_const(&mut self, fl: u8, v: u32) {
        self.materialize(flags::ALL);
        self.c(v);
        self.set(L_FLAG + fl.trailing_zeros());
    }

    /// Pushes 0/1 for a condition code.
    pub(crate) fn cond(&mut self, cc: ConditionCode) {
        use ConditionCode as CC;
        if self.x64 && self.cond_x64_fast(cc) {
            return;
        }
        if let Some((FlagOp::Sub, w)) = self.pending
            && !self.x64
        {
            // Compare fast paths straight from the operands.
            let unsigned = |me: &mut Self, op: W<'static>| { me.get(L_FA); me.get(L_FB); me.i(op); };
            let signed = |me: &mut Self, op: W<'static>| { me.get(L_FA); me.sext(w); me.get(L_FB); me.sext(w); me.i(op); };
            match cc {
                CC::b => return unsigned(self, W::I32LtU),
                CC::ae => return unsigned(self, W::I32GeU),
                CC::be => return unsigned(self, W::I32LeU),
                CC::a => return unsigned(self, W::I32GtU),
                CC::e => { self.get(L_FR); self.i(W::I32Eqz); return; }
                CC::ne => { self.get(L_FR); self.c(0); self.i(W::I32Ne); return; }
                CC::l => return signed(self, W::I32LtS),
                CC::ge => return signed(self, W::I32GeS),
                CC::le => return signed(self, W::I32LeS),
                CC::g => return signed(self, W::I32GtS),
                _ => {}
            }
        }
        match cc {
            CC::None => self.c(1),
            CC::o => self.flag(flags::OF),
            CC::no => { self.flag(flags::OF); self.i(W::I32Eqz); }
            CC::b => self.flag(flags::CF),
            CC::ae => { self.flag(flags::CF); self.i(W::I32Eqz); }
            CC::e => self.flag(flags::ZF),
            CC::ne => { self.flag(flags::ZF); self.i(W::I32Eqz); }
            CC::be => { self.flag(flags::CF); self.flag(flags::ZF); self.i(W::I32Or); }
            CC::a => { self.flag(flags::CF); self.flag(flags::ZF); self.i(W::I32Or); self.i(W::I32Eqz); }
            CC::s => self.flag(flags::SF),
            CC::ns => { self.flag(flags::SF); self.i(W::I32Eqz); }
            CC::p => self.flag(flags::PF),
            CC::np => { self.flag(flags::PF); self.i(W::I32Eqz); }
            CC::l => { self.flag(flags::SF); self.flag(flags::OF); self.i(W::I32Ne); }
            CC::ge => { self.flag(flags::SF); self.flag(flags::OF); self.i(W::I32Eq); }
            CC::le => { self.flag(flags::SF); self.flag(flags::OF); self.i(W::I32Ne); self.flag(flags::ZF); self.i(W::I32Or); }
            CC::g => { self.flag(flags::SF); self.flag(flags::OF); self.i(W::I32Ne); self.flag(flags::ZF); self.i(W::I32Or); self.i(W::I32Eqz); }
        }
    }

    // ---- control flow ----------------------------------------------------

    /// Jumps to another block of this function.
    fn goto(&mut self, target: u32) {
        self.goto_within(target, 0);
    }

    /// `goto` from inside `extra` more nested blocks than `depth` counts. A later block
    /// is a branch to the end of its label (its code follows), the header of a loop
    /// around this block a branch to the loop; anything else goes round the dispatcher
    /// loop. Branches straight to their target keep the engines' branch prediction
    /// apart, where the dispatcher's one indirect jump would share it.
    pub(crate) fn goto_within(&mut self, target: u32, extra: u32) {
        let idx = self.index[&target];
        self.count(crate::abi::COUNT_JUMPS);
        let found = self.labels.iter().rev().position(|l| matches!(l, Label::Block(p) | Label::Loop(p) if *p == idx));
        if let Some(d) = found {
            self.i(W::Br(d as u32 + self.depth + extra));
        } else {
            self.count(crate::abi::COUNT_LOOPS);
            self.c(idx);
            self.set(L_BLK);
            let top = self.labels.iter().rev().position(|l| matches!(l, Label::Dispatch)).expect("dispatcher label");
            self.i(W::Br(top as u32 + self.depth + extra));
        }
    }

    fn push32_from(&mut self, src: u32) {
        self.get(L_REG + 4);
        self.c(4);
        self.i(W::I32Sub);
        self.tee(L_REG + 4);
        self.get(src);
        self.store(32);
    }

    fn push32_const(&mut self, v: u32) {
        self.get(L_REG + 4);
        self.c(4);
        self.i(W::I32Sub);
        self.tee(L_REG + 4);
        self.c(v);
        self.store(32);
    }

    /// Pops a dword into local `dst`.
    fn pop32_to(&mut self, dst: u32) {
        self.get(L_REG + 4);
        self.load(32);
        self.set(dst);
        self.get(L_REG + 4);
        self.c(4);
        self.i(W::I32Add);
        self.set(L_REG + 4);
    }

    /// Calls the guest function at a static address.
    /// Leaves the table index of the function at `target` (not in this module) in
    /// L_R0: known when it was translated before this one, else looked up.
    fn static_index(&mut self, target: u32) {
        match self.cfg.known.get(&target) {
            Some(&i) => self.c(i),
            None => {
                self.c(target);
                self.resolve();
            }
        }
        self.set(L_R0);
    }

    pub(crate) fn call_static(&mut self, target: u32) {
        self.count(crate::abi::COUNT_CALLS);
        // the callee reads flags at entry: they go over in the globals
        if self.cfg.call_flags.get(&target).copied().unwrap_or(flags::ALL) != 0 {
            self.spill_flags();
        }
        self.push_ret();
        if let Some(&idx) = self.func_index.get(&target) {
            self.push_regs();
            self.i(W::Call(idx));
        } else {
            // resolved before the arguments are taken: an API run there changes them
            self.static_index(target);
            self.push_regs();
            self.get(L_R0);
            self.i(W::CallIndirect { type_index: self.fty(), table_index: 0 });
        }
        self.after_call();
    }

    /// Pushes the return address of the call being emitted (64-bit code: the full one).
    fn push_ret(&mut self) {
        if self.x64 {
            self.push64_const(self.next_ip64);
        } else {
            self.push32_const(self.next_ip);
        }
    }

    /// Pops a return address into L_T0 (its low 32 bits in 64-bit code).
    fn pop_ret(&mut self) {
        if self.x64 {
            self.pop64_lo_to(L_T0);
        } else {
            self.pop32_to(L_T0);
        }
    }

    /// esp (rsp) += n.
    fn sp_add(&mut self, n: u32) {
        if self.x64 {
            self.lget(L_REG + 4);
            self.i(W::I64Const(n as i64));
            self.i(W::I64Add);
            self.lset(L_REG + 4);
        } else {
            self.get(L_REG + 4);
            self.c(n);
            self.i(W::I32Add);
            self.set(L_REG + 4);
        }
    }

    /// Calls the guest function whose address is in L_T1.
    pub(crate) fn call_dynamic(&mut self) {
        self.count(crate::abi::COUNT_CALLS);
        self.push_ret();
        self.get(L_T1);
        self.resolve();
        self.set(L_R0);
        self.push_regs();
        self.get(L_R0);
        self.i(W::CallIndirect { type_index: self.fty(), table_index: 0 });
        self.after_call();
    }

    fn after_call(&mut self) {
        self.after_call_to(self.next_ip);
    }

    /// After a call that is to return to `expected`.
    fn after_call_to(&mut self, expected: u32) {
        // the callee's registers come back as results; its flags (those read before
        // written again) in the globals
        self.pop_regs();
        for fl in 0..6 {
            if self.needed & (1 << fl) != 0 {
                self.i(W::GlobalGet(Global::Cf as u32 + fl));
                self.set(L_FLAG + fl);
            }
        }
        self.pending = None;
        // The callee's `ret` left the continuation in eip; anything else means the
        // guest unwound past us, so keep returning until a frame matches.
        self.i(W::GlobalGet(Global::Eip as u32));
        self.c(expected);
        self.i(W::I32Ne);
        self.i(W::GlobalGet(Global::Ctl as u32));
        self.i(W::I32Or);
        self.i(W::If(BlockType::Empty));
        self.spill_regs();
        self.ret_regs();
        self.i(W::End);
    }

    fn ret(&mut self, extra: u32, live_out: u8) {
        self.pop_ret();
        if extra != 0 {
            self.sp_add(extra);
        }
        // to an address this function pushed: stay in it (returning would unwind every
        // caller to the dispatcher, which then re-enters each one at its return address)
        let targets: Vec<u32> = self.cfg.ret_targets.iter().copied().filter(|t| self.index.contains_key(t)).collect();
        if !targets.is_empty() {
            self.materialize(live_out);
            for t in targets {
                self.get(L_T0);
                self.c(t);
                self.i(W::I32Eq);
                self.i(W::If(BlockType::Empty));
                self.depth += 1;
                self.goto(t);
                self.depth -= 1;
                self.i(W::End);
            }
        }
        self.get(L_T0);
        self.i(W::GlobalSet(Global::Eip as u32));
        // flags in the globals (some callers read them: Delphi's string compares)
        self.spill_flags();
        self.ret_regs();
    }

    /// Tail-jumps to the guest address in L_T1 (flags may be live into the target).
    pub(crate) fn tail_dynamic(&mut self) {
        self.spill_flags();
        self.get(L_T1);
        self.resolve();
        self.set(L_R0);
        self.push_regs();
        self.get(L_R0);
        self.i(W::ReturnCallIndirect { type_index: self.fty(), table_index: 0 });
    }

    fn tail_static(&mut self, target: u32) {
        self.spill_flags();
        if let Some(&idx) = self.func_index.get(&target) {
            self.push_regs();
            self.i(W::ReturnCall(idx));
        } else {
            self.static_index(target);
            self.push_regs();
            self.get(L_R0);
            self.i(W::ReturnCallIndirect { type_index: self.fty(), table_index: 0 });
        }
    }

    // ---- function body ---------------------------------------------------

    pub fn emit(&mut self) {
        self.load_state();
        if self.cfg.blocks.values().any(|b| matches!(b.term, Term::IndirectTail)) {
            if self.x64 {
                self.lget(L_REG + 4);
                self.i(W::I32WrapI64);
                self.i(W::I64Load(mem0()));
                self.lset(crate::lift64::X_RA);
            } else {
                self.get(L_REG + 4);
                self.load(32);
                self.set(L_RA);
            }
        }
        self.count(crate::abi::COUNT_ENTRIES);
        if self.count && !self.count_blocks {
            self.count_at(self.cfg.entry);
        }
        if self.probe {
            self.ip = self.cfg.entry;
            self.trap(trap::PROBE, self.cfg.entry);
        }
        // Blocks are laid out by address; the entry need not be the first one.
        let entry_index = self.index[&self.cfg.entry];
        if entry_index != 0 {
            self.c(entry_index);
            self.set(L_BLK);
        }
        // block $fail; loop $dispatch; block i_{m-1} ... block i_0; br_table; end i_0;
        // item 0; end i_1; item 1; ... where a loop item is loop $head; block j_{k-1} ...
        // block j_1; header block; end j_1; item 1; ...; end. The dispatcher reaches the
        // outermost items (loops by their header); the layout leaves nothing else that
        // jumps back to an earlier block other than a loop's header.
        let items = std::mem::take(&mut self.items);
        let m = items.len() as u32;
        self.i(W::Block(BlockType::Empty));
        self.i(W::Loop(BlockType::Empty));
        self.labels.push(Label::Dispatch);
        for it in items.iter().rev() {
            self.i(W::Block(BlockType::Empty));
            self.labels.push(Label::Block(it.first()));
        }
        self.get(L_BLK);
        let mut targets = vec![m + 1; self.order.len()];
        for (j, it) in items.iter().enumerate() {
            targets[it.first() as usize] = j as u32;
        }
        self.i(W::BrTable(Cow::Owned(targets), m + 1));
        for it in &items {
            self.i(W::End);
            self.labels.pop();
            self.emit_item(it);
        }
        self.i(W::End); // dispatch
        self.labels.pop();
        self.i(W::End); // fail
        self.i(W::Unreachable);
        self.i(W::End);
    }

    fn emit_item(&mut self, item: &crate::layout::Item) {
        use crate::layout::Item;
        match item {
            Item::Block(p) => {
                self.cur_block = *p as usize;
                self.depth = 0;
                self.pending = None;
                let block = &self.cfg.blocks[&self.order[*p as usize]];
                if self.count_blocks {
                    if let Some(i) = block.instrs.first() {
                        self.count_at(i.ip() as u32);
                    }
                }
                self.emit_block(block);
            }
            Item::Loop(inner) => {
                self.i(W::Loop(BlockType::Empty));
                self.labels.push(Label::Loop(inner[0].first()));
                for it in inner[1..].iter().rev() {
                    self.i(W::Block(BlockType::Empty));
                    self.labels.push(Label::Block(it.first()));
                }
                self.emit_item(&inner[0]);
                for it in &inner[1..] {
                    self.i(W::End);
                    self.labels.pop();
                    self.emit_item(it);
                }
                self.i(W::End);
                self.labels.pop();
            }
        }
    }

    fn emit_block(&mut self, block: &Block) {
        let live_out = self.live_out[&block.start];
        // Flags each instruction must leave available for later readers.
        let mut needs = vec![0u8; block.instrs.len()];
        let mut need = live_out;
        for (k, _) in block.instrs.iter().enumerate().rev() {
            needs[k] = need;
            let (def, used) = effects_at(self.cfg, block, k);
            need = (need & !def) | used;
        }
        let last = block.instrs.len().saturating_sub(1);
        self.x87_sync = true;
        for (k, instr) in block.instrs.iter().enumerate() {
            self.ip = instr.ip() as u32;
            self.next_ip = instr.next_ip() as u32;
            self.next_ip64 = instr.next_ip();
            self.needed = needs[k];
            let is_term = k == last && !matches!(block.term, Term::Fall(_) | Term::Bad(_));
            if is_term {
                self.emit_terminator(instr, &block.term, live_out);
            } else {
                self.emit_instr(instr);
            }
            // calls (other code, APIs, other threads) and MMX change the x87 state
            if instr.mnemonic() == Mnemonic::Call || (0..instr.op_count()).any(|n| instr.op_kind(n) == OpKind::Register && instr.op_register(n).is_mm()) {
                self.x87_sync = true;
            }
        }
        if let Term::Fall(next) = block.term {
            self.materialize(live_out);
            if self.index[&next] != self.cur_block as u32 + 1 {
                self.goto(next);
            }
        }
        if let Term::Bad(ip) = block.term {
            self.materialize(live_out);
            self.ip = ip;
            self.trap(trap::UD2, ip);
            self.c(ip);
            self.i(W::GlobalSet(Global::Eip as u32));
            self.spill_all();
            self.ret_regs();
        }
    }

    fn emit_terminator(&mut self, instr: &Instruction, term: &Term, live_out: u8) {
        match term {
            Term::Fall(_) | Term::Bad(_) => unreachable!(),
            Term::Jump(target) => {
                self.materialize(live_out);
                if self.index[target] != self.cur_block as u32 + 1 {
                    self.goto(*target);
                }
            }
            Term::Cond { taken, next } => {
                match instr.mnemonic() {
                    Mnemonic::Jecxz | Mnemonic::Jrcxz | Mnemonic::Loop | Mnemonic::Loope | Mnemonic::Loopne if self.x64 => {
                        self.rcx_cond_x64(instr)
                    }
                    Mnemonic::Jecxz => {
                        self.get(L_REG + 1);
                        self.i(W::I32Eqz);
                    }
                    Mnemonic::Loop | Mnemonic::Loope | Mnemonic::Loopne => {
                        self.get(L_REG + 1);
                        self.c(1);
                        self.i(W::I32Sub);
                        self.tee(L_REG + 1);
                        self.c(0);
                        self.i(W::I32Ne);
                        if instr.mnemonic() != Mnemonic::Loop {
                            self.flag(flags::ZF);
                            if instr.mnemonic() == Mnemonic::Loopne {
                                self.i(W::I32Eqz);
                            }
                            self.i(W::I32And);
                        }
                    }
                    _ => self.cond(instr.condition_code()),
                }
                self.set(L_T2);
                self.materialize(live_out);
                self.get(L_T2);
                self.i(W::If(BlockType::Empty));
                self.depth += 1;
                self.goto(*taken);
                self.depth -= 1;
                self.i(W::End);
                if self.index[next] != self.cur_block as u32 + 1 {
                    self.goto(*next);
                }
            }
            Term::Table(targets) if self.x64 => {
                self.materialize(live_out);
                self.table_x64(instr, targets);
            }
            Term::Table(targets) => {
                // jmp [idx*4 + table]: the index register selects the case.
                self.materialize(live_out);
                let ix = instr.memory_index();
                // Each case branches to its block. Use a nested block ladder; the index is
                // pushed inside the innermost block (operands do not cross blocks).
                let n = targets.len() as u32;
                // Outermost block: indices outside the discovered table (Delphi's Move
                // indexes its table with negative values) jump through memory.
                self.i(W::Block(BlockType::Empty));
                for _ in 0..n {
                    self.i(W::Block(BlockType::Empty));
                }
                self.read_reg(ix);
                self.i(W::BrTable(Cow::Owned((0..n).collect()), n));
                for (k, case) in targets.iter().enumerate() {
                    self.i(W::End);
                    let remaining = n - 1 - k as u32;
                    self.goto_within(*case, remaining + 1);
                }
                self.i(W::End);
                self.read_op(instr, 0, false);
                self.set(L_T1);
                self.tail_dynamic();
            }
            Term::TailCall(target) => self.tail_static(*target),
            Term::IndirectTail if self.x64 => self.indirect_tail_x64(instr),
            Term::IndirectTail => {
                // jmp r/m32
                self.read_op(instr, 0, false);
                self.set(L_T1);
                // To the address the function was entered to return to: its return.
                // Delphi's string concatenation (_UStrCatN) ends `pop eax; lea esp,
                // [esp+edx*4]; jmp eax` to drop the caller's arguments; as a tail call
                // the continuation became a new function inside this one's frame, and a
                // loop around such calls nested a wasm frame per iteration until the
                // stack ran out.
                self.get(L_T1);
                self.get(L_RA);
                self.i(W::I32Eq);
                self.i(W::If(BlockType::Empty));
                self.get(L_T1);
                self.i(W::GlobalSet(Global::Eip as u32));
                self.spill_flags();
                self.ret_regs();
                self.i(W::End);
                self.tail_dynamic();
            }
            Term::Ret => {
                let extra = if instr.op_count() > 0 { instr.immediate16() as u32 } else { 0 };
                self.ret(extra, live_out);
            }
            Term::InlineCall { entry } => {
                // the call's effect on the guest: its return address on the stack
                self.push_ret();
                self.materialize(live_out);
                if self.index[entry] != self.cur_block as u32 + 1 {
                    self.goto(*entry);
                }
            }
            Term::InlineTail { target, ret_to, ret_key } => {
                // the inlined call's return address is on the stack: the target returns
                // through it
                if self.cfg.call_flags.get(target).copied().unwrap_or(flags::ALL) != 0 {
                    self.spill_flags();
                }
                if let Some(&idx) = self.func_index.get(target) {
                    self.push_regs();
                    self.i(W::Call(idx));
                } else {
                    self.static_index(*target);
                    self.push_regs();
                    self.get(L_R0);
                    self.i(W::CallIndirect { type_index: self.fty(), table_index: 0 });
                }
                self.after_call_to(*ret_to);
                if self.index[ret_key] != self.cur_block as u32 + 1 {
                    self.goto(*ret_key);
                }
            }
            Term::InlineRet { ret_to, ret_key } => {
                self.pop_ret();
                if instr.op_count() > 0 {
                    self.sp_add(instr.immediate16() as u32);
                }
                self.materialize(live_out);
                self.get(L_T0);
                self.c(*ret_to);
                self.i(W::I32Eq);
                self.i(W::If(BlockType::Empty));
                self.depth += 1;
                self.goto(*ret_key);
                self.depth -= 1;
                self.i(W::End);
                // the callee returned elsewhere: as a plain `ret`
                self.get(L_T0);
                self.i(W::GlobalSet(Global::Eip as u32));
                self.spill_flags();
                self.ret_regs();
            }
            Term::Trap => {
                let kind = match instr.mnemonic() {
                    Mnemonic::Int3 => trap::INT3,
                    Mnemonic::Int => trap::INT,
                    Mnemonic::Ud2 => trap::UD2,
                    Mnemonic::Hlt => trap::HLT,
                    _ => trap::UNIMPLEMENTED,
                };
                let arg = if instr.mnemonic() == Mnemonic::Int { instr.immediate8() as u32 } else { self.ip };
                self.trap(kind, arg);
                // Continue at the next instruction through the dispatcher.
                self.c(self.next_ip);
                self.i(W::GlobalSet(Global::Eip as u32));
                self.spill_all();
                self.ret_regs();
            }
        }
    }

    // ---- instructions ----------------------------------------------------

    /// Emits a binary ALU op `dst = dst <op> src` with lazy flags.
    fn alu(&mut self, instr: &Instruction, op: FlagOp, wop: fn(&mut Self), writes: bool) {
        let w = Self::op_width(instr, 0);
        if matches!(op, FlagOp::Adc | FlagOp::Sbb) {
            // The carry must be read before the operand locals of the pending op are reused.
            self.flag(flags::CF);
            self.set(L_FC);
            self.pending = None;
        }
        let dst = self.prep_dst(instr, 0);
        self.read_dst(dst, w);
        self.set(L_FA);
        self.read_op(instr, 1, false);
        self.set(L_FB);
        self.get(L_FA);
        self.get(L_FB);
        wop(self);
        if matches!(op, FlagOp::Adc | FlagOp::Sbb) {
            self.get(L_FC);
            if op == FlagOp::Adc { self.i(W::I32Add) } else { self.i(W::I32Sub) }
        }
        if w != 32 {
            self.c(mask(w));
            self.i(W::I32And);
        }
        self.set(L_FR);
        if writes {
            self.write_dst(dst, w, L_FR);
        }
        self.set_pending(op, w);
    }

    fn emit_instr(&mut self, instr: &Instruction) {
        use Mnemonic::*;
        if self.x64 {
            return self.emit_instr_x64(instr);
        }
        match instr.mnemonic() {
            Nop | Pause | Lfence | Mfence | Sfence | Prefetcht0 | Prefetcht1 | Prefetcht2 | Prefetchnta | Endbr32 => {}
            Mov => {
                if instr.op0_kind() == OpKind::Register && instr.op0_register().is_segment_register() {
                    return self.unimplemented(instr);
                }
                if instr.op1_kind() == OpKind::Register && instr.op1_register().is_segment_register() {
                    // the selectors of a 32-bit process on 64-bit Windows (crash reports
                    // save them into a CONTEXT)
                    let sel = match instr.op1_register() {
                        Register::CS => 0x23,
                        Register::FS => 0x53,
                        _ => 0x2B,
                    };
                    // a register destination takes the zero-extended selector, memory 16 bits
                    let w = if instr.op0_kind() == OpKind::Register { Self::op_width(instr, 0) } else { 16 };
                    let dst = self.prep_dst(instr, 0);
                    self.c(sel);
                    self.set(L_T1);
                    self.write_dst(dst, w, L_T1);
                    return;
                }
                let w = Self::op_width(instr, 0);
                let dst = self.prep_dst(instr, 0);
                self.read_op(instr, 1, false);
                self.set(L_T1);
                self.write_dst(dst, w, L_T1);
            }
            Movzx | Movsx => {
                let dst = self.prep_dst(instr, 0);
                let sw = Self::op_width(instr, 1);
                self.read_op(instr, 1, false);
                if instr.mnemonic() == Movsx {
                    self.sext(sw);
                }
                self.set(L_T1);
                self.write_dst(dst, Self::op_width(instr, 0), L_T1);
            }
            Lea => {
                self.addr(instr);
                self.set(L_T1);
                let r = instr.op0_register();
                self.write_reg_from(r, L_T1);
            }
            Push => {
                if instr.op0_kind() == OpKind::Register && instr.op0_register().is_segment_register() {
                    return self.unimplemented(instr);
                }
                let w = Self::op_width(instr, 0);
                if w == 16 {
                    self.read_op(instr, 0, false);
                    self.set(L_T1);
                    self.get(L_REG + 4);
                    self.c(2);
                    self.i(W::I32Sub);
                    self.tee(L_REG + 4);
                    self.get(L_T1);
                    self.store(16);
                } else {
                    // push esp pushes the value before the decrement: read first.
                    self.read_op(instr, 0, false);
                    self.set(L_T1);
                    self.push32_from(L_T1);
                }
            }
            Pop => {
                if instr.op0_kind() == OpKind::Register && instr.op0_register().is_segment_register() {
                    return self.unimplemented(instr);
                }
                let w = Self::op_width(instr, 0);
                if w == 16 {
                    self.get(L_REG + 4);
                    self.load(16);
                    self.set(L_T1);
                    self.get(L_REG + 4);
                    self.c(2);
                    self.i(W::I32Add);
                    self.set(L_REG + 4);
                } else {
                    self.pop32_to(L_T1);
                }
                // The address of a memory destination is computed after the pop (esp updated).
                let dst = self.prep_dst(instr, 0);
                self.write_dst(dst, w, L_T1);
            }
            Add => self.alu(instr, FlagOp::Add, |s| s.i(W::I32Add), true),
            Adc => self.alu(instr, FlagOp::Adc, |s| s.i(W::I32Add), true),
            Sub => self.alu(instr, FlagOp::Sub, |s| s.i(W::I32Sub), true),
            Sbb => self.alu(instr, FlagOp::Sbb, |s| s.i(W::I32Sub), true),
            Cmp => self.alu(instr, FlagOp::Sub, |s| s.i(W::I32Sub), false),
            And => self.alu(instr, FlagOp::Logic, |s| s.i(W::I32And), true),
            Or => self.alu(instr, FlagOp::Logic, |s| s.i(W::I32Or), true),
            Xor => self.alu(instr, FlagOp::Logic, |s| s.i(W::I32Xor), true),
            Test => self.alu(instr, FlagOp::Logic, |s| s.i(W::I32And), false),
            Inc | Dec => {
                let w = Self::op_width(instr, 0);
                // CF survives: make sure it is in its local before the result replaces the pending op.
                self.materialize(flags::ALL);
                let dst = self.prep_dst(instr, 0);
                self.read_dst(dst, w);
                self.set(L_FA);
                self.c(1);
                self.set(L_FB);
                self.get(L_FA);
                self.c(1);
                if instr.mnemonic() == Inc { self.i(W::I32Add) } else { self.i(W::I32Sub) }
                self.c(mask(w));
                self.i(W::I32And);
                self.set(L_FR);
                self.write_dst(dst, w, L_FR);
                self.set_pending(if instr.mnemonic() == Inc { FlagOp::Inc } else { FlagOp::Dec }, w);
            }
            Neg => {
                let w = Self::op_width(instr, 0);
                let dst = self.prep_dst(instr, 0);
                self.c(0);
                self.set(L_FA);
                self.read_dst(dst, w);
                self.set(L_FB);
                self.c(0);
                self.get(L_FB);
                self.i(W::I32Sub);
                self.c(mask(w));
                self.i(W::I32And);
                self.set(L_FR);
                self.write_dst(dst, w, L_FR);
                self.set_pending(FlagOp::Sub, w);
            }
            Not => {
                let w = Self::op_width(instr, 0);
                let dst = self.prep_dst(instr, 0);
                self.read_dst(dst, w);
                self.c(mask(w));
                self.i(W::I32Xor);
                self.set(L_T1);
                self.write_dst(dst, w, L_T1);
            }
            Shl | Sal | Shr | Sar => self.shift(instr),
            Rol | Ror | Rcl | Rcr => self.rotate(instr),
            Shld | Shrd => self.double_shift(instr),
            Imul => self.imul(instr),
            Mul => self.mul(instr),
            Div | Idiv => self.div(instr),
            Cbw => {
                self.read_reg(Register::AL);
                self.i(W::I32Extend8S);
                self.set(L_T1);
                self.write_reg_from(Register::AX, L_T1);
            }
            Cwde => {
                self.read_reg(Register::AX);
                self.i(W::I32Extend16S);
                self.set(L_T1);
                self.write_reg_from(Register::EAX, L_T1);
            }
            Cwd => {
                self.read_reg(Register::AX);
                self.i(W::I32Extend16S);
                self.c(31);
                self.i(W::I32ShrS);
                self.set(L_T1);
                self.write_reg_from(Register::DX, L_T1);
            }
            Cdq => {
                self.get(L_REG);
                self.c(31);
                self.i(W::I32ShrS);
                self.set(L_REG + 2);
            }
            Xchg => {
                let w = Self::op_width(instr, 0);
                let dst = self.prep_dst(instr, 0);
                self.read_dst(dst, w);
                self.set(L_T1);
                self.read_op(instr, 1, false);
                self.set(L_T2);
                self.write_dst(dst, w, L_T2);
                let r = instr.op1_register();
                self.write_reg_from(r, L_T1);
            }
            Cmovo | Cmovno | Cmovb | Cmovae | Cmove | Cmovne | Cmovbe | Cmova | Cmovs | Cmovns | Cmovp | Cmovnp | Cmovl
            | Cmovge | Cmovle | Cmovg => {
                let w = Self::op_width(instr, 0);
                let r = instr.op0_register();
                self.read_op(instr, 1, false);
                self.read_reg(r);
                self.cond(instr.condition_code());
                self.i(W::Select);
                self.set(L_T1);
                self.write_reg_from(r, L_T1);
                let _ = w;
            }
            Seto | Setno | Setb | Setae | Sete | Setne | Setbe | Seta | Sets | Setns | Setp | Setnp | Setl | Setge | Setle
            | Setg => {
                let dst = self.prep_dst(instr, 0);
                self.cond(instr.condition_code());
                self.set(L_T1);
                self.write_dst(dst, 8, L_T1);
            }
            Call => match instr.op0_kind() {
                OpKind::NearBranch32 | OpKind::NearBranch16 => {
                    let target = instr.near_branch_target() as u32;
                    self.call_static(target);
                }
                OpKind::Register | OpKind::Memory => {
                    self.read_op(instr, 0, false);
                    self.set(L_T1);
                    self.call_dynamic();
                }
                _ => self.unimplemented(instr),
            },
            Leave => {
                self.get(L_REG + 5);
                self.set(L_REG + 4);
                self.pop32_to(L_REG + 5);
            }
            Enter => {
                let size = instr.immediate16() as u32;
                let level = instr.immediate8_2nd();
                if level != 0 {
                    return self.unimplemented(instr);
                }
                self.push32_from(L_REG + 5);
                self.get(L_REG + 4);
                self.set(L_REG + 5);
                self.get(L_REG + 4);
                self.c(size);
                self.i(W::I32Sub);
                self.set(L_REG + 4);
            }
            Bt | Bts | Btr | Btc => self.bit_test(instr),
            // The CPUID we report has neither BMI1 nor LZCNT, so TZCNT/LZCNT (F3 0F BC/BD)
            // run as BSF/BSR, as on the CPUs before them (compilers emit `rep bsf` for that).
            Bsf | Bsr | Tzcnt | Lzcnt => {
                let w = Self::op_width(instr, 0);
                let r = instr.op0_register();
                self.read_op(instr, 1, false);
                self.set(L_T1);
                self.materialize(flags::ALL);
                self.get(L_T1);
                self.i(W::I32Eqz);
                self.set(L_FLAG + 3);
                self.get(L_T1);
                self.i(W::If(BlockType::Empty));
                self.depth += 1;
                if matches!(instr.mnemonic(), Bsf | Tzcnt) {
                    self.get(L_T1);
                    self.i(W::I32Ctz);
                } else {
                    self.c(31);
                    self.get(L_T1);
                    self.i(W::I32Clz);
                    self.i(W::I32Sub);
                }
                self.set(L_T2);
                self.write_reg_from(r, L_T2);
                self.depth -= 1;
                self.i(W::End);
                let _ = w;
            }
            Bswap => {
                let r = instr.op0_register();
                self.get(Self::reg_local(r));
                self.set(L_T1);
                // ((x << 24) | ((x & 0xff00) << 8) | ((x >> 8) & 0xff00) | (x >> 24))
                self.get(L_T1); self.c(24); self.i(W::I32Shl);
                self.get(L_T1); self.c(0xff00); self.i(W::I32And); self.c(8); self.i(W::I32Shl); self.i(W::I32Or);
                self.get(L_T1); self.c(8); self.i(W::I32ShrU); self.c(0xff00); self.i(W::I32And); self.i(W::I32Or);
                self.get(L_T1); self.c(24); self.i(W::I32ShrU); self.i(W::I32Or);
                self.set(Self::reg_local(r));
            }
            Xadd => {
                let w = Self::op_width(instr, 0);
                let dst = self.prep_dst(instr, 0);
                self.read_dst(dst, w);
                self.set(L_FA);
                self.read_op(instr, 1, false);
                self.set(L_FB);
                self.get(L_FA);
                self.get(L_FB);
                self.i(W::I32Add);
                self.c(mask(w));
                self.i(W::I32And);
                self.set(L_FR);
                let r = instr.op1_register();
                self.write_reg_from(r, L_FA);
                self.write_dst(dst, w, L_FR);
                self.set_pending(FlagOp::Add, w);
            }
            Cmpxchg => {
                let w = Self::op_width(instr, 0);
                let acc = match w { 8 => Register::AL, 16 => Register::AX, _ => Register::EAX };
                let dst = self.prep_dst(instr, 0);
                self.read_reg(acc);
                self.set(L_FA);
                self.read_dst(dst, w);
                self.set(L_FB);
                self.get(L_FA);
                self.get(L_FB);
                self.i(W::I32Sub);
                self.c(mask(w));
                self.i(W::I32And);
                self.set(L_FR);
                self.set_pending(FlagOp::Sub, w);
                self.get(L_FA);
                self.get(L_FB);
                self.i(W::I32Eq);
                self.i(W::If(BlockType::Empty));
                self.depth += 1;
                self.read_op(instr, 1, false);
                self.set(L_T1);
                self.write_dst(dst, w, L_T1);
                self.i(W::Else);
                self.write_reg_from(acc, L_FB);
                self.depth -= 1;
                self.i(W::End);
            }
            Cmpxchg8b => {
                self.addr(instr);
                self.set(L_T0);
                self.get(L_T0);
                self.i(W::I64Load(mem0()));
                self.set(L_Q0);
                // edx:eax == m64 ?
                self.get(L_REG + 2);
                self.i(W::I64ExtendI32U);
                self.i(W::I64Const(32));
                self.i(W::I64Shl);
                self.get(L_REG);
                self.i(W::I64ExtendI32U);
                self.i(W::I64Or);
                self.get(L_Q0);
                self.i(W::I64Eq);
                self.set(L_T1);
                self.materialize(flags::ALL);
                self.get(L_T1);
                self.set(L_FLAG + 3);
                self.get(L_T1);
                self.i(W::If(BlockType::Empty));
                self.depth += 1;
                self.get(L_T0);
                self.get(L_REG + 1);
                self.i(W::I64ExtendI32U);
                self.i(W::I64Const(32));
                self.i(W::I64Shl);
                self.get(L_REG + 3);
                self.i(W::I64ExtendI32U);
                self.i(W::I64Or);
                self.i(W::I64Store(mem0()));
                self.i(W::Else);
                self.get(L_Q0);
                self.i(W::I32WrapI64);
                self.set(L_REG);
                self.get(L_Q0);
                self.i(W::I64Const(32));
                self.i(W::I64ShrU);
                self.i(W::I32WrapI64);
                self.set(L_REG + 2);
                self.depth -= 1;
                self.i(W::End);
            }
            Lahf => {
                self.materialize(flags::ALL);
                // AH = SF:ZF:0:AF:0:PF:1:CF
                self.get(L_FLAG + 4); self.c(7); self.i(W::I32Shl);
                self.get(L_FLAG + 3); self.c(6); self.i(W::I32Shl); self.i(W::I32Or);
                self.get(L_FLAG + 2); self.c(4); self.i(W::I32Shl); self.i(W::I32Or);
                self.get(L_FLAG + 1); self.c(2); self.i(W::I32Shl); self.i(W::I32Or);
                self.get(L_FLAG); self.i(W::I32Or);
                self.c(2); self.i(W::I32Or);
                self.set(L_T1);
                self.write_reg_from(Register::AH, L_T1);
            }
            Sahf => {
                self.materialize(flags::ALL);
                self.read_reg(Register::AH);
                self.set(L_T1);
                for (bit, fl) in [(0u32, 0u32), (2, 1), (4, 2), (6, 3), (7, 4)] {
                    self.get(L_T1);
                    self.c(bit);
                    self.i(W::I32ShrU);
                    self.c(1);
                    self.i(W::I32And);
                    self.set(L_FLAG + fl);
                }
            }
            Pushfd | Pushf => {
                self.eflags_to(L_T1);
                if instr.mnemonic() == Pushf {
                    self.get(L_REG + 4); self.c(2); self.i(W::I32Sub); self.tee(L_REG + 4); self.get(L_T1); self.store(16);
                } else {
                    self.push32_from(L_T1);
                }
            }
            Popfd | Popf => {
                if instr.mnemonic() == Popf {
                    self.get(L_REG + 4); self.load(16); self.set(L_T1);
                    self.get(L_REG + 4); self.c(2); self.i(W::I32Add); self.set(L_REG + 4);
                } else {
                    self.pop32_to(L_T1);
                }
                self.eflags_from(L_T1);
            }
            Pushad => {
                self.get(L_REG + 4);
                self.set(L_T1);
                for r in 0..8u32 {
                    if r == 4 { self.push32_from(L_T1); } else { self.push32_from(L_REG + r); }
                }
            }
            Popad => {
                for r in (0..8u32).rev() {
                    if r == 4 {
                        self.get(L_REG + 4); self.c(4); self.i(W::I32Add); self.set(L_REG + 4);
                    } else {
                        self.pop32_to(L_REG + r);
                    }
                }
            }
            Clc => self.set_flag_const(flags::CF, 0),
            Stc => self.set_flag_const(flags::CF, 1),
            Cmc => {
                self.materialize(flags::ALL);
                self.get(L_FLAG);
                self.c(1);
                self.i(W::I32Xor);
                self.set(L_FLAG);
            }
            Cld => { self.c(0); self.i(W::GlobalSet(Global::Df as u32)); }
            Std => { self.c(1); self.i(W::GlobalSet(Global::Df as u32)); }
            Cpuid => {
                self.spill_all();
                self.call_rt(RtFunc::Cpuid);
                self.reload_all();
            }
            Rdtsc => {
                self.spill_all();
                self.call_rt(RtFunc::Rdtsc);
                self.reload_all();
            }
            Movsb | Movsw | Movsd if instr.op0_kind() == OpKind::MemoryESEDI => self.string_op(instr),
            Stosb | Stosw | Stosd | Lodsb | Lodsw | Lodsd | Scasb | Scasw | Scasd | Cmpsb | Cmpsw => self.string_op(instr),
            Cmpsd if instr.op0_kind() == OpKind::MemorySegESI => self.string_op(instr),
            Xlatb => {
                self.get(L_REG + 3);
                self.read_reg(Register::AL);
                self.i(W::I32Add);
                self.load(8);
                self.set(L_T1);
                self.write_reg_from(Register::AL, L_T1);
            }
            Int3 | Int | Ud2 | Hlt | Into | Bound => {
                // Only reached when not a block terminator (cannot happen); treat as trap.
                self.trap(trap::UNIMPLEMENTED, self.ip);
            }
            Femms | Emms | Wait => self.x87(instr),
            m if crate::cfg::is_x87(m) => self.x87(instr),
            _ => {
                if !self.sse(instr) {
                    self.unimplemented(instr);
                }
            }
        }
    }

    pub(crate) fn eflags_to(&mut self, dst: u32) {
        self.materialize(flags::ALL);
        self.c(0x202); // bit 1 always set, IF set
        self.get(L_FLAG); self.i(W::I32Or);
        self.get(L_FLAG + 1); self.c(2); self.i(W::I32Shl); self.i(W::I32Or);
        self.get(L_FLAG + 2); self.c(4); self.i(W::I32Shl); self.i(W::I32Or);
        self.get(L_FLAG + 3); self.c(6); self.i(W::I32Shl); self.i(W::I32Or);
        self.get(L_FLAG + 4); self.c(7); self.i(W::I32Shl); self.i(W::I32Or);
        self.i(W::GlobalGet(Global::Df as u32)); self.c(10); self.i(W::I32Shl); self.i(W::I32Or);
        self.get(L_FLAG + 5); self.c(11); self.i(W::I32Shl); self.i(W::I32Or);
        self.set(dst);
    }

    pub(crate) fn eflags_from(&mut self, src: u32) {
        self.materialize(flags::ALL);
        for (bit, fl) in [(0u32, 0u32), (2, 1), (4, 2), (6, 3), (7, 4), (11, 5)] {
            self.get(src);
            self.c(bit);
            self.i(W::I32ShrU);
            self.c(1);
            self.i(W::I32And);
            self.set(L_FLAG + fl);
        }
        self.get(src);
        self.c(10);
        self.i(W::I32ShrU);
        self.c(1);
        self.i(W::I32And);
        self.i(W::GlobalSet(Global::Df as u32));
    }

    fn shift(&mut self, instr: &Instruction) {
        let w = Self::op_width(instr, 0);
        let dst = self.prep_dst(instr, 0);
        let is_sar = instr.mnemonic() == Mnemonic::Sar;
        let is_shr = instr.mnemonic() == Mnemonic::Shr;
        let op = if is_sar { FlagOp::Sar } else if is_shr { FlagOp::Shr } else { FlagOp::Shl };
        let static_count = match instr.op1_kind() {
            OpKind::Immediate8 => Some(instr.immediate8() as u32 & 31),
            OpKind::Register => None,
            _ => Some(1),
        };
        // The pending flag op still owns L_FA/L_FB/L_FR until it is replaced or materialized,
        // so a count of zero must not touch them and a run-time count materializes first.
        match static_count {
            Some(0) => {}
            Some(n) => {
                self.read_dst(dst, w);
                self.set(L_FA);
                self.c(n);
                self.set(L_FB);
                self.get(L_FA);
                if is_sar { self.sext(w); }
                self.c(n);
                self.i(if is_sar { W::I32ShrS } else if is_shr { W::I32ShrU } else { W::I32Shl });
                self.c(mask(w));
                self.i(W::I32And);
                self.set(L_FR);
                self.write_dst(dst, w, L_FR);
                self.set_pending(op, w);
            }
            None => {
                // Count in CL: a zero count leaves flags untouched, so decide at run time;
                // either way only the flags read later need computing.
                let needed = self.needed;
                self.materialize(needed);
                self.read_dst(dst, w);
                self.set(L_FA);
                self.read_reg(Register::CL);
                self.c(31);
                self.i(W::I32And);
                self.tee(L_FB);
                self.i(W::If(BlockType::Empty));
                self.depth += 1;
                self.get(L_FA);
                if is_sar { self.sext(w); }
                self.get(L_FB);
                self.i(if is_sar { W::I32ShrS } else if is_shr { W::I32ShrU } else { W::I32Shl });
                self.c(mask(w));
                self.i(W::I32And);
                self.set(L_FR);
                self.write_dst(dst, w, L_FR);
                self.pending = Some((op, w));
                self.materialize(needed);
                self.depth -= 1;
                self.i(W::End);
                self.pending = None;
            }
        }
    }

    fn rotate(&mut self, instr: &Instruction) {
        let w = Self::op_width(instr, 0);
        let dst = self.prep_dst(instr, 0);
        let count_static = match instr.op1_kind() {
            OpKind::Immediate8 => Some(instr.immediate8() as u32 & 31),
            OpKind::Register => None,
            _ => Some(1),
        };
        self.materialize(flags::ALL);
        self.read_dst(dst, w);
        self.set(L_FA);
        match count_static {
            Some(n) => { self.c(n); }
            None => { self.read_reg(Register::CL); self.c(31); self.i(W::I32And); }
        }
        self.set(L_FB);
        self.get(L_FB);
        self.i(W::If(BlockType::Empty));
        self.depth += 1;
        match instr.mnemonic() {
            Mnemonic::Rol | Mnemonic::Ror => {
                let left = instr.mnemonic() == Mnemonic::Rol;
                // count mod width
                self.get(L_FB);
                self.c(w as u32 - 1);
                self.i(W::I32And);
                self.set(L_T1);
                if w == 32 {
                    self.get(L_FA);
                    self.get(L_T1);
                    self.i(if left { W::I32Rotl } else { W::I32Rotr });
                } else {
                    // (a << n) | (a >> (w - n)) within width
                    self.get(L_FA);
                    self.get(L_T1);
                    self.i(if left { W::I32Shl } else { W::I32ShrU });
                    self.get(L_FA);
                    self.c(w as u32);
                    self.get(L_T1);
                    self.i(W::I32Sub);
                    self.c(31);
                    self.i(W::I32And);
                    self.i(if left { W::I32ShrU } else { W::I32Shl });
                    self.i(W::I32Or);
                    self.c(mask(w));
                    self.i(W::I32And);
                }
                self.set(L_FR);
                self.write_dst(dst, w, L_FR);
                // CF = bit rotated into the low (rol) or high (ror) position
                self.get(L_FR);
                if left { self.c(1); self.i(W::I32And); } else { self.c(w as u32 - 1); self.i(W::I32ShrU); }
                self.set(L_FLAG);
                // OF (count == 1): msb(res) ^ CF for rol; msb(res) ^ bit(w-2) of res for ror
                self.get(L_FR);
                self.c(w as u32 - 1);
                self.i(W::I32ShrU);
                self.c(1);
                self.i(W::I32And);
                if left {
                    self.get(L_FLAG);
                } else {
                    self.get(L_FR);
                    self.c(w as u32 - 2);
                    self.i(W::I32ShrU);
                    self.c(1);
                    self.i(W::I32And);
                }
                self.i(W::I32Xor);
                self.set(L_FLAG + 5);
            }
            _ => {
                // rcl/rcr: rotate through carry, count mod (w + 1), done bit by bit (rare instructions).
                let left = instr.mnemonic() == Mnemonic::Rcl;
                self.get(L_FB);
                self.c(w as u32 + 1);
                self.i(W::I32RemU);
                self.tee(L_T1);
                self.i(W::If(BlockType::Empty));
                self.depth += 1;
                self.get(L_FA);
                self.set(L_FR);
                self.i(W::Block(BlockType::Empty));
                self.i(W::Loop(BlockType::Empty));
                self.get(L_T1);
                self.i(W::I32Eqz);
                self.i(W::BrIf(1));
                if left {
                    // new_cf = msb(res); res = (res << 1 | cf) & mask
                    self.get(L_FR); self.c(w as u32 - 1); self.i(W::I32ShrU); self.c(1); self.i(W::I32And); self.set(L_T2);
                    self.get(L_FR); self.c(1); self.i(W::I32Shl); self.get(L_FLAG); self.i(W::I32Or); self.c(mask(w)); self.i(W::I32And); self.set(L_FR);
                } else {
                    self.get(L_FR); self.c(1); self.i(W::I32And); self.set(L_T2);
                    self.get(L_FR); self.c(1); self.i(W::I32ShrU); self.get(L_FLAG); self.c(w as u32 - 1); self.i(W::I32Shl); self.i(W::I32Or); self.set(L_FR);
                }
                self.get(L_T2);
                self.set(L_FLAG);
                self.get(L_T1); self.c(1); self.i(W::I32Sub); self.set(L_T1);
                self.i(W::Br(0));
                self.i(W::End);
                self.i(W::End);
                self.write_dst(dst, w, L_FR);
                // OF (count == 1): msb(res) ^ cf (rcl) / msb(res) ^ bit(w-2)(res) (rcr)
                self.get(L_FR); self.c(w as u32 - 1); self.i(W::I32ShrU); self.c(1); self.i(W::I32And);
                if left { self.get(L_FLAG); } else { self.get(L_FR); self.c(w as u32 - 2); self.i(W::I32ShrU); self.c(1); self.i(W::I32And); }
                self.i(W::I32Xor);
                self.set(L_FLAG + 5);
                self.depth -= 1;
                self.i(W::End);
            }
        }
        self.depth -= 1;
        self.i(W::End);
    }

    fn double_shift(&mut self, instr: &Instruction) {
        // shld/shrd dst, src, count: 32-bit (and 16-bit) forms.
        let w = Self::op_width(instr, 0);
        let left = instr.mnemonic() == Mnemonic::Shld;
        let dst = self.prep_dst(instr, 0);
        self.materialize(flags::ALL);
        self.read_dst(dst, w);
        self.set(L_FA);
        self.read_op(instr, 1, false);
        self.set(L_T1);
        match instr.op2_kind() {
            OpKind::Immediate8 => self.c(instr.immediate8() as u32 & 31),
            _ => { self.read_reg(Register::CL); self.c(31); self.i(W::I32And); }
        }
        self.set(L_FB);
        self.get(L_FB);
        self.i(W::If(BlockType::Empty));
        self.depth += 1;
        // 64-bit concatenation makes both widths uniform: for shld, hi=dst lo=src; shrd: hi=src lo=dst.
        let (hi, lo) = if left { (L_FA, L_T1) } else { (L_T1, L_FA) };
        self.get(hi); self.i(W::I64ExtendI32U); self.i(W::I64Const(w as i64)); self.i(W::I64Shl);
        self.get(lo); self.i(W::I64ExtendI32U); self.i(W::I64Or);
        self.set(L_Q0);
        if left {
            self.get(L_Q0); self.get(L_FB); self.i(W::I64ExtendI32U); self.i(W::I64Shl);
            self.i(W::I64Const(w as i64)); self.i(W::I64ShrU); self.i(W::I32WrapI64);
        } else {
            self.get(L_Q0); self.get(L_FB); self.i(W::I64ExtendI32U); self.i(W::I64ShrU); self.i(W::I32WrapI64);
        }
        self.c(mask(w));
        self.i(W::I32And);
        self.set(L_FR);
        self.write_dst(dst, w, L_FR);
        // CF: last bit shifted out
        if left {
            self.get(L_FA); self.c(w as u32); self.get(L_FB); self.i(W::I32Sub); self.i(W::I32ShrU); self.c(1); self.i(W::I32And);
        } else {
            self.get(L_FA); self.get(L_FB); self.c(1); self.i(W::I32Sub); self.i(W::I32ShrU); self.c(1); self.i(W::I32And);
        }
        self.set(L_FLAG);
        // ZF/SF/PF from result, OF = msb changed (count == 1), AF undefined
        self.get(L_FR); self.i(W::I32Eqz); self.set(L_FLAG + 3);
        self.get(L_FR); self.c(w as u32 - 1); self.i(W::I32ShrU); self.c(1); self.i(W::I32And); self.set(L_FLAG + 4);
        self.get(L_FR); self.c(0xff); self.i(W::I32And); self.i(W::I32Popcnt); self.c(1); self.i(W::I32And); self.c(1); self.i(W::I32Xor); self.set(L_FLAG + 1);
        self.get(L_FR); self.get(L_FA); self.i(W::I32Xor); self.c(w as u32 - 1); self.i(W::I32ShrU); self.c(1); self.i(W::I32And); self.set(L_FLAG + 5);
        self.depth -= 1;
        self.i(W::End);
    }

    fn imul(&mut self, instr: &Instruction) {
        let w = Self::op_width(instr, 0);
        self.materialize(flags::ALL);
        match instr.op_count() {
            1 => {
                // edx:eax = eax * r/m (signed); CF=OF = result does not fit the low half
                self.read_op(instr, 0, false);
                self.sext(w);
                self.i(W::I64ExtendI32S);
                let acc = match w { 8 => Register::AL, 16 => Register::AX, _ => Register::EAX };
                self.read_reg(acc);
                self.sext(w);
                self.i(W::I64ExtendI32S);
                self.i(W::I64Mul);
                self.set(L_Q0);
                self.wide_result(w, true);
            }
            2 | 3 => {
                let r = instr.op0_register();
                let src_idx = if instr.op_count() == 3 { 1 } else { 1 };
                self.read_op(instr, src_idx, false);
                self.sext(w);
                self.i(W::I64ExtendI32S);
                if instr.op_count() == 3 {
                    self.read_op(instr, 2, false);
                } else {
                    self.read_reg(r);
                }
                self.sext(w);
                self.i(W::I64ExtendI32S);
                self.i(W::I64Mul);
                self.set(L_Q0);
                self.get(L_Q0);
                self.i(W::I32WrapI64);
                self.c(mask(w));
                self.i(W::I32And);
                self.set(L_T1);
                self.write_reg_from(r, L_T1);
                // CF = OF = (full result != sign-extended low half)
                self.get(L_Q0);
                self.get(L_T1);
                self.sext(w);
                self.i(W::I64ExtendI32S);
                self.i(W::I64Ne);
                self.tee(L_FLAG);
                self.set(L_FLAG + 5);
            }
            _ => unreachable!(),
        }
    }

    fn mul(&mut self, instr: &Instruction) {
        let w = Self::op_width(instr, 0);
        self.materialize(flags::ALL);
        self.read_op(instr, 0, false);
        self.i(W::I64ExtendI32U);
        let acc = match w { 8 => Register::AL, 16 => Register::AX, _ => Register::EAX };
        self.read_reg(acc);
        self.i(W::I64ExtendI32U);
        self.i(W::I64Mul);
        self.set(L_Q0);
        self.wide_result(w, false);
    }

    /// Writes the 2w-bit product in L_Q0 to its destination and sets CF/OF.
    fn wide_result(&mut self, w: u8, signed: bool) {
        match w {
            8 => {
                self.get(L_Q0); self.i(W::I32WrapI64); self.c(0xffff); self.i(W::I32And); self.set(L_T1);
                self.write_reg_from(Register::AX, L_T1);
            }
            16 => {
                self.get(L_Q0); self.i(W::I32WrapI64); self.c(0xffff); self.i(W::I32And); self.set(L_T1);
                self.write_reg_from(Register::AX, L_T1);
                self.get(L_Q0); self.i(W::I64Const(16)); self.i(W::I64ShrU); self.i(W::I32WrapI64); self.c(0xffff); self.i(W::I32And); self.set(L_T1);
                self.write_reg_from(Register::DX, L_T1);
            }
            _ => {
                self.get(L_Q0); self.i(W::I32WrapI64); self.set(L_REG);
                self.get(L_Q0); self.i(W::I64Const(32)); self.i(W::I64ShrU); self.i(W::I32WrapI64); self.set(L_REG + 2);
            }
        }
        // CF = OF = upper half is not the sign/zero extension of the lower half
        self.get(L_Q0);
        self.get(L_Q0);
        self.i(W::I32WrapI64);
        self.c(mask(w));
        self.i(W::I32And);
        if signed { self.sext(w); self.i(W::I64ExtendI32S); } else { self.i(W::I64ExtendI32U); }
        self.i(W::I64Ne);
        self.tee(L_FLAG);
        self.set(L_FLAG + 5);
    }

    fn div(&mut self, instr: &Instruction) {
        let w = Self::op_width(instr, 0);
        let signed = instr.mnemonic() == Mnemonic::Idiv;
        self.materialize(flags::ALL);
        // dividend in L_Q0, divisor in L_Q1
        match w {
            8 => { self.read_reg(Register::AX); if signed { self.sext(16); self.i(W::I64ExtendI32S) } else { self.i(W::I64ExtendI32U) } }
            16 => {
                self.read_reg(Register::DX); self.i(W::I64ExtendI32U); self.i(W::I64Const(16)); self.i(W::I64Shl);
                self.read_reg(Register::AX); self.i(W::I64ExtendI32U); self.i(W::I64Or);
                if signed { self.i(W::I64Const(32)); self.i(W::I64Shl); self.i(W::I64Const(32)); self.i(W::I64ShrS); }
            }
            _ => {
                self.get(L_REG + 2); self.i(W::I64ExtendI32U); self.i(W::I64Const(32)); self.i(W::I64Shl);
                self.get(L_REG); self.i(W::I64ExtendI32U); self.i(W::I64Or);
            }
        }
        self.set(L_Q0);
        self.read_op(instr, 0, false);
        if signed { self.sext(w); self.i(W::I64ExtendI32S) } else { self.i(W::I64ExtendI32U) }
        self.set(L_Q1);
        // #DE on zero divisor or quotient overflow
        self.get(L_Q1);
        self.i(W::I64Eqz);
        self.i(W::If(BlockType::Empty));
        self.depth += 1;
        self.trap(trap::DIVIDE_ERROR, self.ip);
        self.depth -= 1;
        self.i(W::End);
        self.get(L_Q0);
        self.get(L_Q1);
        self.i(if signed { W::I64DivS } else { W::I64DivU });
        self.set(L_Q0); // quotient (temporarily reuse: remainder recomputed below)
        // remainder = dividend - quotient * divisor: recompute dividend
        match w {
            8 => { self.read_reg(Register::AX); if signed { self.sext(16); self.i(W::I64ExtendI32S) } else { self.i(W::I64ExtendI32U) } }
            16 => {
                self.read_reg(Register::DX); self.i(W::I64ExtendI32U); self.i(W::I64Const(16)); self.i(W::I64Shl);
                self.read_reg(Register::AX); self.i(W::I64ExtendI32U); self.i(W::I64Or);
                if signed { self.i(W::I64Const(32)); self.i(W::I64Shl); self.i(W::I64Const(32)); self.i(W::I64ShrS); }
            }
            _ => {
                self.get(L_REG + 2); self.i(W::I64ExtendI32U); self.i(W::I64Const(32)); self.i(W::I64Shl);
                self.get(L_REG); self.i(W::I64ExtendI32U); self.i(W::I64Or);
            }
        }
        self.get(L_Q0);
        self.get(L_Q1);
        self.i(W::I64Mul);
        self.i(W::I64Sub);
        self.set(L_Q1); // remainder
        // overflow check: quotient must fit in w bits (signed or unsigned)
        self.get(L_Q0);
        if signed {
            self.i(W::I64Const(-(1i64 << (w - 1))));
            self.i(W::I64LtS);
            self.get(L_Q0);
            self.i(W::I64Const((1i64 << (w - 1)) - 1));
            self.i(W::I64GtS);
            self.i(W::I32Or);
        } else {
            self.i(W::I64Const(mask(w) as i64));
            self.i(W::I64GtU);
        }
        self.i(W::If(BlockType::Empty));
        self.depth += 1;
        self.trap(trap::DIVIDE_ERROR, self.ip);
        self.depth -= 1;
        self.i(W::End);
        match w {
            8 => {
                self.get(L_Q0); self.i(W::I32WrapI64); self.set(L_T1); self.write_reg_from(Register::AL, L_T1);
                self.get(L_Q1); self.i(W::I32WrapI64); self.set(L_T1); self.write_reg_from(Register::AH, L_T1);
            }
            16 => {
                self.get(L_Q0); self.i(W::I32WrapI64); self.set(L_T1); self.write_reg_from(Register::AX, L_T1);
                self.get(L_Q1); self.i(W::I32WrapI64); self.set(L_T1); self.write_reg_from(Register::DX, L_T1);
            }
            _ => {
                self.get(L_Q0); self.i(W::I32WrapI64); self.set(L_REG);
                self.get(L_Q1); self.i(W::I32WrapI64); self.set(L_REG + 2);
            }
        }
    }

    fn bit_test(&mut self, instr: &Instruction) {
        let w = Self::op_width(instr, 0);
        self.materialize(flags::ALL);
        // bit offset
        match instr.op1_kind() {
            OpKind::Immediate8 => self.c(instr.immediate8() as u32),
            _ => { self.read_op(instr, 1, false); self.sext(w); }
        }
        self.set(L_T1);
        let dst = match instr.op0_kind() {
            OpKind::Memory if instr.op1_kind() == OpKind::Register => {
                // register bit offsets address memory beyond the operand: addr += (off >> log2(w)) * bytes
                self.addr(instr);
                self.get(L_T1);
                self.c((w as u32).trailing_zeros());
                self.i(W::I32ShrS);
                self.c((w as u32 / 8).trailing_zeros());
                self.i(W::I32Shl);
                self.i(W::I32Add);
                self.set(L_T0);
                Dst::Mem
            }
            _ => self.prep_dst(instr, 0),
        };
        self.get(L_T1);
        self.c(w as u32 - 1);
        self.i(W::I32And);
        self.set(L_T1);
        self.read_dst(dst, w);
        self.set(L_T2);
        self.get(L_T2);
        self.get(L_T1);
        self.i(W::I32ShrU);
        self.c(1);
        self.i(W::I32And);
        self.set(L_FLAG);
        let modify: Option<fn(&mut Self)> = match instr.mnemonic() {
            Mnemonic::Bts => Some(|s| s.i(W::I32Or)),
            Mnemonic::Btr => Some(|s| { s.c(0xffff_ffff); s.i(W::I32Xor); s.i(W::I32And); }),
            Mnemonic::Btc => Some(|s| s.i(W::I32Xor)),
            _ => None,
        };
        if let Some(m) = modify {
            self.get(L_T2);
            self.c(1);
            self.get(L_T1);
            self.i(W::I32Shl);
            m(self);
            self.c(mask(w));
            self.i(W::I32And);
            self.set(L_T2);
            self.write_dst(dst, w, L_T2);
        }
    }

    fn string_op(&mut self, instr: &Instruction) {
        use Mnemonic::*;
        let m = instr.mnemonic();
        let w: u8 = match m { Movsb | Stosb | Lodsb | Scasb | Cmpsb => 8, Movsw | Stosw | Lodsw | Scasw | Cmpsw => 16, _ => 32 };
        let step = (w / 8) as u32;
        let rep = instr.has_rep_prefix() || instr.has_repe_prefix() || instr.has_repne_prefix();
        let compare = matches!(m, Scasb | Scasw | Scasd | Cmpsb | Cmpsw | Cmpsd);
        if compare { self.materialize(flags::ALL); }
        // delta = df ? -step : step  -> L_T3   (select keeps the first operand when the condition is set)
        self.c(step.wrapping_neg());
        self.c(step);
        self.i(W::GlobalGet(Global::Df as u32));
        self.i(W::Select);
        self.set(L_T3);
        if rep {
            self.i(W::Block(BlockType::Empty));
            self.i(W::Loop(BlockType::Empty));
            self.depth += 2;
            self.get(L_REG + 1);
            self.i(W::I32Eqz);
            self.i(W::BrIf(1));
        }
        match m {
            Movsb | Movsw | Movsd => {
                self.get(L_REG + 7);
                self.get(L_REG + 6);
                self.load(w);
                self.store(w);
                self.get(L_REG + 6); self.get(L_T3); self.i(W::I32Add); self.set(L_REG + 6);
                self.get(L_REG + 7); self.get(L_T3); self.i(W::I32Add); self.set(L_REG + 7);
            }
            Stosb | Stosw | Stosd => {
                self.get(L_REG + 7);
                self.read_reg(match w { 8 => Register::AL, 16 => Register::AX, _ => Register::EAX });
                self.store(w);
                self.get(L_REG + 7); self.get(L_T3); self.i(W::I32Add); self.set(L_REG + 7);
            }
            Lodsb | Lodsw | Lodsd => {
                self.get(L_REG + 6);
                self.load(w);
                self.set(L_T1);
                self.write_reg_from(match w { 8 => Register::AL, 16 => Register::AX, _ => Register::EAX }, L_T1);
                self.get(L_REG + 6); self.get(L_T3); self.i(W::I32Add); self.set(L_REG + 6);
            }
            Scasb | Scasw | Scasd => {
                self.read_reg(match w { 8 => Register::AL, 16 => Register::AX, _ => Register::EAX });
                self.set(L_FA);
                self.get(L_REG + 7);
                self.load(w);
                self.set(L_FB);
                self.get(L_REG + 7); self.get(L_T3); self.i(W::I32Add); self.set(L_REG + 7);
                self.cmp_flags(w);
            }
            Cmpsb | Cmpsw | Cmpsd => {
                self.get(L_REG + 6);
                self.load(w);
                self.set(L_FA);
                self.get(L_REG + 7);
                self.load(w);
                self.set(L_FB);
                self.get(L_REG + 6); self.get(L_T3); self.i(W::I32Add); self.set(L_REG + 6);
                self.get(L_REG + 7); self.get(L_T3); self.i(W::I32Add); self.set(L_REG + 7);
                self.cmp_flags(w);
            }
            _ => unreachable!(),
        }
        if rep {
            self.get(L_REG + 1); self.c(1); self.i(W::I32Sub); self.set(L_REG + 1);
            if compare {
                // repe stops when ZF == 0, repne when ZF == 1
                self.get(L_FLAG + 3);
                if instr.has_repne_prefix() { self.i(W::BrIf(1)); } else { self.i(W::I32Eqz); self.i(W::BrIf(1)); }
            }
            self.i(W::Br(0));
            self.depth -= 2;
            self.i(W::End);
            self.i(W::End);
        }
    }

    /// Materialized compare flags from L_FA - L_FB (used by string compares).
    fn cmp_flags(&mut self, w: u8) {
        self.get(L_FA);
        self.get(L_FB);
        self.i(W::I32Sub);
        self.c(mask(w));
        self.i(W::I32And);
        self.set(L_FR);
        self.pending = Some((FlagOp::Sub, w));
        self.materialize(flags::ALL);
    }

    pub(crate) fn x87(&mut self, instr: &Instruction) {
        self.count(crate::abi::COUNT_X87);
        if self.x87_inline && self.x87_inline_form(instr) {
            return;
        }
        self.x87_out_of_line(instr);
        self.x87_sync = true;
    }

    /// The x87 helpers' fast paths of common instructions, inline: no call, and the
    /// block address, TOP and the fast-path conditions read once per run of x87 code
    /// (`x87_sync`). An instruction a check rejects runs out of line as before, and
    /// the rest of the run then does too (`L_XQ` = 0), so what the code knows of the
    /// registers (`x87_known`) holds whenever it runs inline. The inline paths are the
    /// helpers' own (precision 53 bits, quick lane only for rounding results), so
    /// both give the same state. False for instructions without one.
    fn x87_inline_form(&mut self, instr: &Instruction) -> bool {
        use crate::x87::{ASrc, Arg, CSrc, Form, Op, Src};
        use fpu::block as fb;
        let Some((k, arg)) = crate::x87::form_of(instr) else { return false };
        let form = crate::x87::FORMS[k as usize];
        let c1_dead = self.c1_dead();
        let mem_ok = |s: Src| matches!(s, Src::M32 | Src::M64 | Src::I16 | Src::I32);
        let need = match form {
            Form::Load(s) if mem_ok(s) => fb::FAST_MOVE,
            Form::LoadSt | Form::Const(_) | Form::Store(Src::M64, _) | Form::StoreSt(_) | Form::Chs | Form::Abs | Form::Xch => fb::FAST_MOVE,
            Form::Store(Src::M32, _) => fb::FAST_INEXACT | X87_QUICK,
            Form::Arith(_, _, src, _) if c1_dead && (matches!(src, ASrc::Mem(s) if mem_ok(s)) || !matches!(src, ASrc::Mem(_))) => fb::FAST_ARITH | X87_QUICK,
            Form::Cmp(src, _, _) if matches!(src, CSrc::Mem(s) if mem_ok(s)) || !matches!(src, CSrc::Mem(_)) => fb::FAST_MOVE,
            Form::StoreInt(Src::I16 | Src::I32, crate::x87::IntMode::Fist | crate::x87::IntMode::Fistp) => fb::FAST_INEXACT | X87_QUICK,
            Form::Fnstsw(_) => 0,
            _ => return false,
        };
        let st = match arg {
            Arg::St(i) => i,
            _ => 0,
        };
        if self.x87_sync {
            self.x87_sync = false;
            self.x87_known = [X87_UNKNOWN; 8];
            self.x87_top = 0;
            self.i(W::GlobalGet(Global::FsBase as u32));
            self.c(fb::OFFSET);
            self.i(W::I32Sub);
            self.set(L_XF);
            self.get(L_XF);
            self.i(W::I32Load(xma(fb::TOP)));
            self.set(L_XT);
            // XQ = FAST | quick (PE set and RC nearest) << 4
            self.get(L_XF);
            self.i(W::I32Load(xma(fb::FAST)));
            self.get(L_XF);
            self.i(W::I32Load16U(xma(fb::SW)));
            self.c(5);
            self.i(W::I32ShrU);
            self.get(L_XF);
            self.i(W::I32Load16U(xma(fb::CW)));
            self.c(0xC00);
            self.i(W::I32And);
            self.i(W::I32Eqz);
            self.i(W::I32And);
            self.c(1);
            self.i(W::I32And);
            self.c(X87_QUICK.trailing_zeros());
            self.i(W::I32Shl);
            self.i(W::I32Or);
            self.set(L_XQ);
        }
        if let Form::Fnstsw(mem) = form {
            // the status word with TOP (no conditions: it reads the state as it is)
            self.get(L_XF);
            self.i(W::I32Load16U(xma(fb::SW)));
            self.get(L_XF);
            self.i(W::I32Load(xma(fb::TOP)));
            self.c(11);
            self.i(W::I32Shl);
            self.i(W::I32Or);
            self.set(L_T1);
            if mem {
                self.addr(instr);
                self.get(L_T1);
                self.i(W::I32Store16(mem0()));
            } else {
                self.write_reg_from(Register::AX, L_T1);
            }
            return true;
        }
        let eflags = matches!(form, Form::Cmp(_, true, _));
        if eflags {
            self.materialize(flags::ALL);
        }
        self.i(W::Block(BlockType::Empty)); // done
        self.i(W::Block(BlockType::Empty)); // out of line
        self.depth += 2;
        self.get(L_XQ);
        self.c(need);
        self.i(W::I32And);
        self.c(need);
        self.i(W::I32Ne);
        self.i(W::BrIf(0));
        match form {
            Form::Load(s) => {
                self.x_mem_value(instr, s, L_XA);
                self.x_need_empty(7);
                self.x_push(L_XA);
                self.x_c1_fip(c1_dead, true);
            }
            Form::LoadSt => {
                self.x_need_f64(st);
                self.x_load(st, L_XA);
                self.x_need_empty(7);
                self.x_push(L_XA);
                self.x_c1_fip(c1_dead, true);
            }
            Form::Const(one) => {
                self.x_need_empty(7);
                self.i(W::F64Const(if one { 1.0f64 } else { 0.0 }.into()));
                self.set(L_XA);
                self.x_push(L_XA);
                self.x_c1_fip(c1_dead, true);
            }
            Form::Store(s, pop) => {
                self.x_need_f64(0);
                self.x_load(0, L_XA);
                if s == Src::M32 {
                    // beyond the largest single, or near or below its underflow threshold
                    self.get(L_XA);
                    self.i(W::F64Abs);
                    self.i(W::F64Const(crate::x87::F32_MAX.into()));
                    self.i(W::F64Gt);
                    self.get(L_XA);
                    self.i(W::F64Abs);
                    self.i(W::F64Const(f64::INFINITY.into()));
                    self.i(W::F64Ne);
                    self.i(W::I32And);
                    self.i(W::BrIf(0));
                    self.get(L_XA);
                    self.i(W::F64Abs);
                    self.i(W::F64Const(crate::x87::F32_TINY.into()));
                    self.i(W::F64Lt);
                    self.get(L_XA);
                    self.i(W::F64Const(0.0f64.into()));
                    self.i(W::F64Ne);
                    self.i(W::I32And);
                    self.i(W::BrIf(0));
                    self.addr(instr);
                    self.get(L_XA);
                    self.i(W::F32DemoteF64);
                    self.i(W::F32Store(mem0()));
                    if !c1_dead {
                        // C1: rounded up in magnitude (PE is set already)
                        self.get(L_XF);
                        self.get(L_XF);
                        self.i(W::I32Load16U(xma(fb::SW)));
                        self.c(!crate::x87::SW_C1 & 0xffff);
                        self.i(W::I32And);
                        self.get(L_XA);
                        self.i(W::F32DemoteF64);
                        self.i(W::F64PromoteF32);
                        self.i(W::F64Abs);
                        self.get(L_XA);
                        self.i(W::F64Abs);
                        self.i(W::F64Gt);
                        self.c(9);
                        self.i(W::I32Shl);
                        self.i(W::I32Or);
                        self.i(W::I32Store16(xma(fb::SW)));
                        self.x_c1_fip(c1_dead, false);
                    }
                } else {
                    self.addr(instr);
                    self.get(L_XA);
                    self.i(W::F64Store(mem0()));
                    self.x_c1_fip(c1_dead, true);
                }
                if pop {
                    self.x_pop();
                }
            }
            Form::StoreSt(pop) => {
                self.x_need_f64(0);
                self.x_load(0, L_XA);
                self.x_store(st, L_XA);
                self.x_set_kind(st, fb::VALID);
                self.x87_known[((self.x87_top + st) & 7) as usize] = X87_F64;
                self.x_c1_fip(c1_dead, true);
                if pop {
                    self.x_pop();
                }
            }
            Form::StoreInt(s, mode) => {
                // rounded to nearest (with PE set already: an inexact result leaves it)
                self.x_need_f64(0);
                self.x_load(0, L_XA);
                self.get(L_XA);
                self.i(W::F64Nearest);
                self.set(L_XB);
                let (lo, hi) = if s == Src::I16 { (-32768.0f64, 32767.0f64) } else { (-2147483648.0, 2147483647.0) };
                self.get(L_XB);
                self.i(W::F64Const(lo.into()));
                self.i(W::F64Ge);
                self.get(L_XB);
                self.i(W::F64Const(hi.into()));
                self.i(W::F64Le);
                self.i(W::I32And);
                self.i(W::I32Eqz);
                self.i(W::BrIf(0));
                self.addr(instr);
                self.get(L_XB);
                self.i(W::I32TruncF64S);
                self.i(if s == Src::I16 { W::I32Store16(mem0()) } else { W::I32Store(mem0()) });
                if !c1_dead {
                    // C1: rounded up in magnitude
                    self.get(L_XF);
                    self.get(L_XF);
                    self.i(W::I32Load16U(xma(fb::SW)));
                    self.c(!crate::x87::SW_C1 & 0xffff);
                    self.i(W::I32And);
                    self.get(L_XB);
                    self.get(L_XA);
                    self.i(W::F64Ne);
                    self.get(L_XB);
                    self.i(W::F64Abs);
                    self.get(L_XA);
                    self.i(W::F64Abs);
                    self.i(W::F64Gt);
                    self.i(W::I32And);
                    self.c(9);
                    self.i(W::I32Shl);
                    self.i(W::I32Or);
                    self.i(W::I32Store16(xma(fb::SW)));
                }
                if mode == crate::x87::IntMode::Fistp {
                    self.x_pop();
                }
                self.x_c1_fip(c1_dead, false);
            }
            Form::Chs | Form::Abs => {
                self.x_need_f64(0);
                self.x_load(0, L_XA);
                self.get(L_XA);
                self.i(if form == Form::Chs { W::F64Neg } else { W::F64Abs });
                self.set(L_XA);
                self.x_store(0, L_XA);
                self.x_c1_fip(c1_dead, true);
            }
            Form::Xch => {
                self.x_need_f64(0);
                self.x_need_f64(st);
                self.x_load(0, L_XA);
                self.x_load(st, L_XB);
                self.x_store(0, L_XB);
                self.x_store(st, L_XA);
                self.x_c1_fip(c1_dead, true);
            }
            Form::Arith(op, rev, src, pop) => {
                // XA = the destination's value, XB = the other operand
                let dest = match src {
                    ASrc::Mem(s) => {
                        self.x_need_f64(0);
                        self.x_mem_value(instr, s, L_XB);
                        0
                    }
                    ASrc::St0Sti => {
                        self.x_need_f64(0);
                        self.x_need_f64(st);
                        self.x_load(st, L_XB);
                        0
                    }
                    ASrc::StiSt0 => {
                        self.x_need_f64(st);
                        self.x_need_f64(0);
                        self.x_load(0, L_XB);
                        st
                    }
                };
                self.x_load(dest, L_XA);
                let (x, y) = if rev { (L_XB, L_XA) } else { (L_XA, L_XB) };
                self.get(x);
                self.get(y);
                self.i(match op {
                    Op::Add => W::F64Add,
                    Op::Sub => W::F64Sub,
                    Op::Mul => W::F64Mul,
                    Op::Div => W::F64Div,
                });
                self.set(L_XR);
                // a normal result, or an exact zero: a sum, a product with a zero
                // factor, a quotient of zero or by an infinity (any other zero
                // underflowed, where the 80-bit unit's wider exponent keeps a value)
                self.get(L_XR);
                self.i(W::F64Abs);
                self.i(W::F64Const(crate::x87::MIN_NORMAL.into()));
                self.i(W::F64Ge);
                self.get(L_XR);
                self.i(W::F64Abs);
                self.i(W::F64Const(f64::INFINITY.into()));
                self.i(W::F64Lt);
                self.i(W::I32And);
                self.get(L_XR);
                self.i(W::F64Const(0.0f64.into()));
                self.i(W::F64Eq);
                match op {
                    Op::Add | Op::Sub => {}
                    Op::Mul => {
                        self.get(x);
                        self.i(W::F64Const(0.0f64.into()));
                        self.i(W::F64Eq);
                        self.get(y);
                        self.i(W::F64Const(0.0f64.into()));
                        self.i(W::F64Eq);
                        self.i(W::I32Or);
                        self.i(W::I32And);
                    }
                    Op::Div => {
                        self.get(x);
                        self.i(W::F64Const(0.0f64.into()));
                        self.i(W::F64Eq);
                        self.get(y);
                        self.i(W::F64Abs);
                        self.i(W::F64Const(f64::INFINITY.into()));
                        self.i(W::F64Eq);
                        self.i(W::I32Or);
                        self.i(W::I32And);
                    }
                }
                self.i(W::I32Or);
                self.i(W::I32Eqz);
                self.i(W::BrIf(0));
                self.x_store(dest, L_XR);
                if pop {
                    self.x_pop();
                }
            }
            Form::Cmp(src, eflags, pops) => {
                self.x_need_f64(0);
                match src {
                    CSrc::Mem(s) => self.x_mem_value(instr, s, L_XB),
                    CSrc::Zero => {
                        self.i(W::F64Const(0.0f64.into()));
                        self.set(L_XB);
                    }
                    CSrc::Sti => {
                        self.x_need_f64(st);
                        self.x_load(st, L_XB);
                    }
                }
                self.x_load(0, L_XA);
                // T2 = st0 < other, T3 = equal (no NaNs here)
                self.get(L_XA);
                self.get(L_XB);
                self.i(W::F64Lt);
                self.set(L_T2);
                self.get(L_XA);
                self.get(L_XB);
                self.i(W::F64Eq);
                self.set(L_T3);
                if !eflags {
                    self.get(L_XF);
                    self.get(L_XF);
                    self.i(W::I32Load16U(xma(fb::SW)));
                    self.c(!(crate::x87::SW_CC | crate::x87::SW_C1) & 0xffff);
                    self.i(W::I32And);
                    self.get(L_T2);
                    self.c(8);
                    self.i(W::I32Shl);
                    self.get(L_T3);
                    self.c(14);
                    self.i(W::I32Shl);
                    self.i(W::I32Or);
                    self.i(W::I32Or);
                    self.i(W::I32Store16(xma(fb::SW)));
                }
                for _ in 0..pops {
                    self.x_pop();
                }
                self.x_c1_fip(c1_dead, false);
                if eflags {
                    // (fcomi leaves the condition codes, C1 too, as they are)
                    self.get(L_T2);
                    self.set(L_FLAG);
                    self.get(L_T3);
                    self.set(L_FLAG + 3);
                    for f in [1, 2, 4, 5] {
                        self.c(0);
                        self.set(L_FLAG + f);
                    }
                }
            }
            _ => unreachable!(),
        }
        self.i(W::Br(1));
        self.i(W::End);
        self.depth -= 1;
        self.x87_out_of_line(instr);
        self.c(0);
        self.set(L_XQ);
        self.i(W::End);
        self.depth -= 1;
        true
    }

    /// Pushes the slot of ST(i) as the inline code sees TOP.
    fn x_slot(&mut self, i: u32) {
        self.get(L_XT);
        if i != 0 {
            self.c(i);
            self.i(W::I32Add);
            self.c(7);
            self.i(W::I32And);
        }
    }

    fn x_rel(&self, i: u32) -> usize {
        ((self.x87_top + i) & 7) as usize
    }

    /// Goes out of line unless ST(i) holds an f64.
    fn x_need_f64(&mut self, i: u32) {
        if self.x87_known[self.x_rel(i)] == X87_F64 {
            return;
        }
        self.get(L_XF);
        self.x_slot(i);
        self.i(W::I32Add);
        self.i(W::I32Load8U(xma(fpu::block::KIND)));
        self.c(fpu::block::VALID as u32);
        self.i(W::I32Ne);
        self.i(W::BrIf(0));
        let r = self.x_rel(i);
        self.x87_known[r] = X87_F64;
    }

    /// Goes out of line unless ST(i) is empty (ST(7): where a push goes).
    fn x_need_empty(&mut self, i: u32) {
        if self.x87_known[self.x_rel(i)] == X87_EMPTY {
            return;
        }
        self.get(L_XF);
        self.x_slot(i);
        self.i(W::I32Add);
        self.i(W::I32Load8U(xma(fpu::block::KIND)));
        self.c(fpu::block::VALID as u32);
        self.i(W::I32And);
        self.i(W::BrIf(0));
    }

    fn x_reg_addr(&mut self, i: u32) {
        self.get(L_XF);
        self.x_slot(i);
        self.c(3);
        self.i(W::I32Shl);
        self.i(W::I32Add);
    }

    fn x_load(&mut self, i: u32, into: u32) {
        self.x_reg_addr(i);
        self.i(W::F64Load(xma(fpu::block::REGS)));
        self.set(into);
    }

    fn x_store(&mut self, i: u32, from: u32) {
        self.x_reg_addr(i);
        self.get(from);
        self.i(W::F64Store(xma(fpu::block::REGS)));
    }

    fn x_set_kind(&mut self, i: u32, kind: u8) {
        self.get(L_XF);
        self.x_slot(i);
        self.i(W::I32Add);
        self.c(kind as u32);
        self.i(W::I32Store8(xma(fpu::block::KIND)));
    }

    /// Pushes the f64 in `from` (the target was checked empty).
    fn x_push(&mut self, from: u32) {
        self.x_slot(7);
        self.set(L_XT);
        self.x_store(0, from);
        self.x_set_kind(0, fpu::block::VALID);
        self.get(L_XF);
        self.get(L_XT);
        self.i(W::I32Store(xma(fpu::block::TOP)));
        self.x87_top = (self.x87_top + 7) & 7;
        let r = self.x_rel(0);
        self.x87_known[r] = X87_F64;
    }

    /// Pops ST(0), which holds an f64 (its kind becomes empty).
    fn x_pop(&mut self) {
        self.x_set_kind(0, 0);
        self.x_slot(1);
        self.set(L_XT);
        self.get(L_XF);
        self.get(L_XT);
        self.i(W::I32Store(xma(fpu::block::TOP)));
        let r = self.x_rel(0);
        self.x87_known[r] = X87_EMPTY;
        self.x87_top = (self.x87_top + 1) & 7;
    }

    /// The memory operand as a register value into `into`; out of line for NaNs and
    /// denormals (they raise IE or DE).
    fn x_mem_value(&mut self, instr: &Instruction, src: crate::x87::Src, into: u32) {
        use crate::x87::Src;
        self.addr(instr);
        match src {
            Src::M32 | Src::M64 => {
                if src == Src::M32 {
                    self.i(W::F32Load(mem0()));
                    self.i(W::F64PromoteF32);
                } else {
                    self.i(W::F64Load(mem0()));
                }
                self.set(into);
                self.get(into);
                self.i(W::F64Abs);
                self.i(W::F64Const(if src == Src::M32 { crate::x87::F32_MIN_NORMAL } else { crate::x87::MIN_NORMAL }.into()));
                self.i(W::F64Ge);
                self.get(into);
                self.i(W::F64Const(0.0f64.into()));
                self.i(W::F64Eq);
                self.i(W::I32Or);
                self.i(W::I32Eqz);
                self.i(W::BrIf(0));
            }
            Src::I16 => {
                self.i(W::I32Load16S(mem0()));
                self.i(W::F64ConvertI32S);
                self.set(into);
            }
            Src::I32 => {
                self.i(W::I32Load(mem0()));
                self.i(W::F64ConvertI32S);
                self.set(into);
            }
            _ => unreachable!(),
        }
    }

    /// C1 = 0 (when `clear`) and FIP = this instruction, unless the next x87
    /// instruction overwrites both first.
    fn x_c1_fip(&mut self, c1_dead: bool, clear: bool) {
        use fpu::block as fb;
        if c1_dead {
            return;
        }
        if clear {
            self.get(L_XF);
            self.get(L_XF);
            self.i(W::I32Load16U(xma(fb::SW)));
            self.c(!crate::x87::SW_C1 & 0xffff);
            self.i(W::I32And);
            self.i(W::I32Store16(xma(fb::SW)));
        }
        self.get(L_XF);
        self.c(self.ip);
        self.i(W::I32Store(xma(fb::FIP)));
    }

    /// An x87 instruction through the `x87` module's helper for its form, or `rt.fpu`.
    fn x87_out_of_line(&mut self, instr: &Instruction) {
        use Mnemonic::*;
        self.count(crate::abi::COUNT_X87_HELPER);
        if self.count_blocks {
            // (as blocks, with the top bit flipped)
            self.count_at(self.ip ^ 0x8000_0000);
        }
        let writes_flags = matches!(instr.mnemonic(), Fcomi | Fcomip | Fucomi | Fucomip);
        let writes_ax = instr.mnemonic() == Fnstsw && !(0..instr.op_count()).any(|n| instr.op_kind(n) == OpKind::Memory);
        if writes_flags {
            self.materialize(flags::ALL);
        }
        match crate::x87::form_of(instr).filter(|_| self.x87_inline) {
            Some((k, arg)) => {
                // block (result i32) { x87.f<k>(a, ip); != -1: done; rt.fpu }
                self.depth += 1;
                self.i(W::Block(BlockType::Result(ValType::I32)));
                match arg {
                    crate::x87::Arg::Mem => self.addr(instr),
                    crate::x87::Arg::St(i) => self.c(i),
                    crate::x87::Arg::None => self.c(0),
                }
                self.c(self.ip | if self.c1_dead() { crate::x87::IP_C1_DEAD } else { 0 });
                self.i(W::Call(abi::RT_FUNCS.len() as u32 + k));
                self.tee(L_T1);
                self.get(L_T1);
                self.c(u32::MAX);
                self.i(W::I32Ne);
                self.i(W::BrIf(0));
                self.i(W::Drop);
                self.x87_call(instr);
                self.i(W::End);
                self.depth -= 1;
            }
            None => self.x87_call(instr),
        }
        if writes_ax && self.x64 {
            self.i(W::I64ExtendI32U);
            self.lset(crate::lift64::X_T1);
            self.rset_from(Register::AX, crate::lift64::X_T1);
        } else if writes_ax {
            self.set(L_T1);
            self.write_reg_from(Register::AX, L_T1);
        } else if writes_flags {
            self.set(L_T1);
            // return value uses EFLAGS bit positions: CF=0, PF=2, ZF=6
            self.get(L_T1); self.c(1); self.i(W::I32And); self.set(L_FLAG);
            self.get(L_T1); self.c(2); self.i(W::I32ShrU); self.c(1); self.i(W::I32And); self.set(L_FLAG + 1);
            self.get(L_T1); self.c(6); self.i(W::I32ShrU); self.c(1); self.i(W::I32And); self.set(L_FLAG + 3);
            self.c(0); self.set(L_FLAG + 2);
            self.c(0); self.set(L_FLAG + 4);
            self.c(0); self.set(L_FLAG + 5);
        } else {
            self.i(W::Drop);
        }
    }

    /// Whether the C1 this x87 instruction leaves is overwritten before anything can
    /// read the status word: a later x87 instruction in the block that writes C1 comes
    /// first (non-x87 instructions in between leave the x87 state alone).
    fn c1_dead(&self) -> bool {
        use Mnemonic::*;
        let block = &self.cfg.blocks[&self.order[self.cur_block]];
        let Some(k) = block.instrs.iter().position(|i| i.ip() as u32 == self.ip) else { return false };
        for i in &block.instrs[k + 1..] {
            match i.mnemonic() {
                Fld | Fild | Fst | Fstp | Fist | Fistp | Fisttp | Fadd | Faddp | Fiadd | Fsub | Fsubp | Fisub | Fsubr | Fsubrp | Fisubr
                | Fmul | Fmulp | Fimul | Fdiv | Fdivp | Fidiv | Fdivr | Fdivrp | Fidivr | Fxch | Fchs | Fabs | Fcom | Fcomp | Fcompp
                | Fucom | Fucomp | Fucompp | Ftst | Fxam | Fld1 | Fldz | Fldpi | Fldl2e | Fldl2t
                | Fldlg2 | Fldln2 | Frndint | Fsqrt | Fsin | Fcos | Fsincos | Fptan | Fpatan | Fscale | Fprem | Fprem1 | Fyl2x | Fyl2xp1
                | F2xm1 | Fxtract | Ficom | Ficomp => return true,
                m if crate::cfg::is_x87(m) => return false,
                Emms | Femms | Wait => return false,
                _ if i.flow_control() != iced_x86::FlowControl::Next => return false,
                _ => {}
            }
        }
        false
    }

    /// `rt.fpu` for one instruction; leaves its i32 result on the stack.
    fn x87_call(&mut self, instr: &Instruction) {
        use Mnemonic::*;
        // a = memory address or ST index of op0; b packs op1's ST index, the memory operand
        // size and which operands are ST registers (see fpu::args).
        let has_mem = (0..instr.op_count()).any(|n| instr.op_kind(n) == OpKind::Memory);
        let op0_st = instr.op_count() > 0 && instr.op0_kind() == OpKind::Register && instr.op0_register().is_st();
        let op1_st = instr.op_count() > 1 && instr.op1_kind() == OpKind::Register && instr.op1_register().is_st();
        if has_mem {
            self.addr(instr);
        } else if op0_st {
            self.c(instr.op0_register().number() as u32);
        } else {
            self.c(0);
        }
        self.set(L_T1);
        let mut b: u32 = 0;
        if op1_st {
            b |= instr.op1_register().number() as u32;
        }
        if has_mem {
            b |= (instr.memory_size() as u32) << 8;
        }
        if op0_st {
            b |= 1 << 16;
        }
        if op1_st {
            b |= 1 << 17;
        }
        let is_fcmov = matches!(instr.mnemonic(), Fcmovb | Fcmove | Fcmovbe | Fcmovu | Fcmovnb | Fcmovne | Fcmovnbe | Fcmovnu);
        if is_fcmov {
            // The condition lives in the integer flags: evaluate it here and pass it in bit 24.
            let cc = match instr.mnemonic() {
                Fcmovb => ConditionCode::b,
                Fcmove => ConditionCode::e,
                Fcmovbe => ConditionCode::be,
                Fcmovu => ConditionCode::p,
                Fcmovnb => ConditionCode::ae,
                Fcmovne => ConditionCode::ne,
                Fcmovnbe => ConditionCode::a,
                _ => ConditionCode::np,
            };
            self.c(b);
            self.cond(cc);
            self.c(24);
            self.i(W::I32Shl);
            self.i(W::I32Or);
        } else {
            self.c(b);
        }
        self.set(L_T2);
        self.i(W::I32Const(instr.code() as i32));
        self.get(L_T1);
        self.get(L_T2);
        self.c(self.ip);
        self.c(self.cfg.fop.get(&self.ip).copied().unwrap_or(0) as u32);
        self.call_rt(RtFunc::Fpu);
        // A pending unmasked x87 exception faults here (#MF): the runtime raises it once
        // the registers are back in the globals.
        self.return_if_ctl(true);
    }
}

/// Flags live into each block (and out, via successors). Conservatively assumes
/// returns and tail/indirect jumps need every flag.
/// Flags instruction `k` of `block` writes and reads. A direct call reads what its
/// callee's entry reads (the callee's writes count as the call's); an inlined call
/// touches none (the callee's blocks do).
fn effects_at(cfg: &Cfg, block: &Block, k: usize) -> (u8, u8) {
    let i = &block.instrs[k];
    if i.flow_control() == iced_x86::FlowControl::Call {
        if k + 1 == block.instrs.len() && matches!(block.term, Term::InlineCall { .. }) {
            return (0, 0);
        }
    }
    if k + 1 == block.instrs.len()
        && let Term::InlineTail { target, .. } = block.term
    {
        return (flags::ALL, cfg.call_flags.get(&target).copied().unwrap_or(flags::ALL));
    }
    if i.flow_control() == iced_x86::FlowControl::Call {
        if crate::cfg::is_near(i) {
            return (flags::ALL, cfg.call_flags.get(&(i.near_branch_target() as u32)).copied().unwrap_or(flags::ALL));
        }
    }
    flags::effects(i)
}

/// Flags live into and out of each block.
fn liveness(cfg: &Cfg) -> (HashMap<u32, u8>, HashMap<u32, u8>) {
    let mut usedef: HashMap<u32, (u8, u8)> = HashMap::default();
    for (&start, b) in &cfg.blocks {
        let (mut used, mut def) = (0u8, 0u8);
        for k in 0..b.instrs.len() {
            let (d, u) = effects_at(cfg, b, k);
            used |= u & !def;
            def |= d;
        }
        usedef.insert(start, (used, def));
    }
    let mut live_in: HashMap<u32, u8> = cfg.blocks.keys().map(|&k| (k, 0)).collect();
    let mut live_out: HashMap<u32, u8> = cfg.blocks.keys().map(|&k| (k, 0)).collect();
    loop {
        let mut changed = false;
        for (&start, b) in cfg.blocks.iter().rev() {
            let out = match b.term {
                Term::Ret | Term::TailCall(_) | Term::IndirectTail | Term::Trap | Term::Bad(_) => flags::ALL,
                _ => b.successors().iter().map(|s| live_in[s]).fold(0, |a, x| a | x),
            };
            let (used, def) = usedef[&start];
            let inn = used | (out & !def);
            if live_out[&start] != out || live_in[&start] != inn {
                live_out.insert(start, out);
                live_in.insert(start, inn);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    (live_in, live_out)
}

pub fn global_index(g: Global) -> u32 {
    g as u32
}

pub fn rt_index(f: RtFunc) -> u32 {
    f as u32
}

pub const _ASSERT_GLOBALS: () = {
    assert!(abi::GLOBALS32 == 27);
    // each global's import index is its enum value
    let mut i = 0;
    while i < abi::GLOBALS.len() {
        assert!(abi::GLOBALS[i].0 as usize == i);
        i += 1;
    }
};
