//! Per-function control-flow graph: straight-line blocks of decoded instructions
//! plus the branch structure the emitter turns into wasm control flow.

use iced_x86::{Decoder, DecoderOptions, FlowControl, Instruction, Mnemonic, OpKind, Register};
use crate::fasthash::HashMap;
use std::collections::{BTreeMap, VecDeque};

/// Read access to the guest image, by virtual address.
pub trait GuestMem {
    /// Copies bytes at `va` into `buf`; false if any byte is unmapped.
    fn read(&self, va: u32, buf: &mut [u8]) -> bool;
}

pub struct FuncInfo<'a> {
    /// Resolved `jmp [idx*4+table]` sites: site address -> targets.
    pub jump_tables: &'a std::collections::HashMap<u32, Vec<u32>>,
    /// Addresses known to start functions; direct jumps there are tail calls.
    pub is_function: &'a dyn Fn(u32) -> bool,
    /// The function table index of a function translated already: calls to it need
    /// not look it up.
    pub translated: &'a dyn Fn(u32) -> Option<u32>,
    /// 64-bit code: the upper half of its addresses. It is decoded at `high << 32 |
    /// address`, so RIP-relative operands and pushed return addresses are the real
    /// 64-bit values; blocks and functions are keyed by the low half (docs/x64.md).
    pub high: u32,
    /// Code that may change after it is translated (a packer's or protector's, code
    /// written at run time): its blocks check their first bytes (`abi::trap::STALE`).
    pub volatile: &'a dyn Fn(u32) -> bool,
    /// The batch being translated (1..2^23), for `abi::trap::STALE`.
    pub tag: u32,
}

/// How code is decoded: 32-bit, or 64-bit with the upper half of its addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bits {
    B32,
    B64 { high: u32 },
}

impl Bits {
    pub fn decoder_bits(self) -> u32 {
        match self {
            Bits::B32 => 32,
            Bits::B64 { .. } => 64,
        }
    }

    /// The instruction pointer of the code at (low) address `addr`.
    pub fn ip(self, addr: u32) -> u64 {
        match self {
            Bits::B32 => addr as u64,
            Bits::B64 { high } => (high as u64) << 32 | addr as u64,
        }
    }
}

/// A direct (relative) branch or call: its target is `near_branch_target`.
pub fn is_near(i: &Instruction) -> bool {
    matches!(i.op0_kind(), OpKind::NearBranch32 | OpKind::NearBranch64)
}

#[derive(Debug, Clone)]
pub enum Term {
    /// Falls into the block at this address.
    Fall(u32),
    Jump(u32),
    Cond { taken: u32, next: u32 },
    Table(Vec<u32>),
    /// Direct jump to another function (tail call).
    TailCall(u32),
    /// `jmp reg/mem`: target computed at run time, pushed by the last instruction.
    IndirectTail,
    Ret,
    Trap,
    /// Undecodable or unmapped bytes at this address: executing them faults (#UD).
    Bad(u32),
    /// `call` (the block's last instruction) to a small leaf function whose blocks are
    /// part of this one's (`entry`, an inline key): the return address is pushed and
    /// the callee's code follows.
    InlineCall { entry: u32 },
    /// `ret` of an inlined callee: back at `ret_to` (the block `ret_key`) when the
    /// popped address is the call's, else a plain `ret`.
    InlineRet { ret_to: u32, ret_key: u32 },
    /// An inlined callee's tail jump to `target`: called (the inlined call's return
    /// address is on the stack), then back at `ret_to` as after `InlineRet`.
    InlineTail { target: u32, ret_to: u32, ret_key: u32 },
}

/// Inlined callees' blocks have keys from here up (one per block); their
/// instructions keep their addresses.
pub const INLINE_KEYS: u32 = 0x8000_0000;
/// Inlining reaches this many calls deep (a callee's own small callees are inlined
/// into it before it is inlined).
const INLINE_DEPTH: u8 = 2;
/// Small functions are inlined at up to this many call sites per function, each of
/// up to `MAX_INLINE_INSTRS` instructions and `INLINE_BUDGET` in all.
const MAX_INLINE_SITES: u32 = 64;
const MAX_INLINE_INSTRS: usize = 96;
const INLINE_BUDGET: usize = 512;

#[derive(Debug, Clone)]
pub struct Block {
    pub start: u32,
    pub instrs: Vec<Instruction>,
    pub term: Term,
}

impl Block {
    pub fn end(&self) -> u32 {
        self.instrs.last().map(|i| i.next_ip() as u32).unwrap_or(self.start)
    }

    pub fn successors(&self) -> Vec<u32> {
        match &self.term {
            Term::Fall(a) | Term::Jump(a) => vec![*a],
            Term::Cond { taken, next } => vec![*taken, *next],
            Term::Table(t) => t.clone(),
            Term::InlineCall { entry } => vec![*entry],
            Term::InlineRet { ret_key, .. } | Term::InlineTail { ret_key, .. } => vec![*ret_key],
            _ => Vec::new(),
        }
    }
}

pub struct Cfg {
    pub entry: u32,
    pub blocks: BTreeMap<u32, Block>,
    /// x87 instructions: ip -> 11-bit opcode (low 3 bits of the escape byte, then ModRM).
    pub fop: HashMap<u32, u16>,
    /// Addresses a `ret` here may go to without leaving the function: ones it pushes
    /// as immediates a little ahead of the push (Delphi's try/finally pushes the exit
    /// label, falls into the finally code and `ret`s to it).
    pub ret_targets: Vec<u32>,
    /// Direct call target -> the flags its entry reads before writing them (callers
    /// hand those over: the CRT's x87 wrappers test flags a previous call returned).
    pub call_flags: HashMap<u32, u8>,
    /// Direct call and jump targets translated already -> their table index.
    pub known: HashMap<u32, u32>,
    /// Volatile code (`FuncInfo::volatile`): the batch tag its checks report, else 0.
    pub guard: u32,
    /// Block key -> (address, length 1/2/4, bytes) of its first instruction as it was
    /// decoded, for the block's check.
    pub checks: HashMap<u32, (u32, u8, u32)>,
}

const MAX_INSTRS_PER_FUNCTION: usize = 200_000;

/// Callees considered for inlining, by (address, depth): what `inlinable` found (many
/// functions call the same few small ones). Kept across translations; code changing
/// under a cached entry is not noticed (as translated code itself is not).
pub type InlineCache = std::cell::RefCell<HashMap<(u32, u8), Option<std::rc::Rc<Cfg>>>>;

/// The function at `entry`, with small callees inlined when `inline`.
pub fn build(mem: &dyn GuestMem, entry: u32, info: &FuncInfo, inline: bool, cache: &InlineCache, bits: Bits) -> Result<Cfg, String> {
    build_with(mem, entry, info, if inline { INLINE_DEPTH } else { 0 }, cache, bits)
}

/// The callee at `target` as blocks to inline, if it is small: a few instructions
/// within 4 KiB after its entry, leaving by `ret` or direct jumps to functions (no
/// jump tables or computed jumps; calls are fine).
fn inlinable(mem: &dyn GuestMem, target: u32, info: &FuncInfo, depth: u8, cache: &InlineCache, bits: Bits) -> Option<std::rc::Rc<Cfg>> {
    if let Some(c) = cache.borrow().get(&(target, depth)) {
        return c.clone();
    }
    let c = inlinable_uncached(mem, target, info, depth, cache, bits).map(std::rc::Rc::new);
    cache.borrow_mut().insert((target, depth), c.clone());
    c
}

fn inlinable_uncached(mem: &dyn GuestMem, target: u32, info: &FuncInfo, depth: u8, cache: &InlineCache, bits: Bits) -> Option<Cfg> {
    let c = build_with(mem, target, info, depth, cache, bits).ok()?;
    let n: usize = c.blocks.values().map(|b| b.instrs.len()).sum();
    let simple = n <= MAX_INLINE_INSTRS
        && c.ret_targets.is_empty()
        && c.blocks.iter().all(|(&k, b)| {
            (k >= INLINE_KEYS || k.wrapping_sub(target) < 0x1000)
                && matches!(
                    b.term,
                    Term::Fall(_) | Term::Jump(_) | Term::Cond { .. } | Term::Ret | Term::TailCall(_) | Term::InlineCall { .. } | Term::InlineRet { .. } | Term::InlineTail { .. }
                )
                && b.successors().iter().all(|t| c.blocks.contains_key(t))
        });
    simple.then_some(c)
}

fn build_with(mem: &dyn GuestMem, entry: u32, info: &FuncInfo, depth: u8, cache: &InlineCache, bits: Bits) -> Result<Cfg, String> {
    let inline = depth > 0;
    let mut sites = 0u32;
    let mut next_key = INLINE_KEYS;
    let mut budget = INLINE_BUDGET;
    let mut blocks: BTreeMap<u32, Block> = BTreeMap::new();
    let mut fop: HashMap<u32, u16> = HashMap::default();
    let mut ret_targets = Vec::new();
    let mut call_flags = HashMap::default();
    let mut known = HashMap::default();
    let mut checks = HashMap::default();
    // (32-bit code only)
    let guard = matches!(bits, Bits::B32) && info.tag != 0 && (info.volatile)(entry);
    let mut queue = VecDeque::from([entry]);
    let mut total = 0usize;
    let mut buf = vec![0u8; 16];
    if zero_filled(mem, entry) {
        // nothing to translate yet: a packer has not unpacked it (or it is not code)
        return Err(format!("function at {entry:#x} is zero-filled memory"));
    }
    while let Some(start) = queue.pop_front() {
        if blocks.contains_key(&start) {
            continue;
        }
        // Landing inside an existing block: split it there.
        if let Some((&bstart, _)) = blocks.range(..start).next_back() {
            let b = &blocks[&bstart];
            if start < b.end() {
                // A branch into the middle of an instruction decodes an overlapping block.
                if let Some(split) = b.instrs.iter().position(|i| i.ip() as u32 == start) {
                    let b = blocks.remove(&bstart).unwrap();
                    let (head, tail) = b.instrs.split_at(split);
                    blocks.insert(bstart, Block { start: bstart, instrs: head.to_vec(), term: Term::Fall(start) });
                    blocks.insert(start, Block { start, instrs: tail.to_vec(), term: b.term.clone() });
                    continue;
                }
            }
        }
        let mut instrs = Vec::new();
        let mut ip = start;
        let mut zero_run = 0;
        let term;
        loop {
            if blocks.contains_key(&ip) && ip != start {
                term = Term::Fall(ip);
                break;
            }
            let n = (0..16).rev().find(|&n| mem.read(ip, &mut buf[..n])).unwrap_or(0);
            if n == 0 {
                term = Term::Bad(ip);
                break;
            }
            let mut decoder = Decoder::with_ip(bits.decoder_bits(), &buf[..n], bits.ip(ip), DecoderOptions::NONE);
            let instr = decoder.decode();
            if instr.is_invalid() {
                term = Term::Bad(ip);
                break;
            }
            total += 1;
            if is_x87(instr.mnemonic()) && instr.mnemonic() != Mnemonic::Femms {
                // skip legacy prefixes to find the D8..DF escape byte
                let mut k = 0;
                while k + 1 < instr.len() && matches!(buf[k], 0x26 | 0x2E | 0x36 | 0x3E | 0x64 | 0x65 | 0x66 | 0x67 | 0xF0 | 0xF2 | 0xF3) {
                    k += 1;
                }
                if (0xD8..=0xDF).contains(&buf[k]) && k + 1 < n {
                    fop.insert(ip, (((buf[k] & 7) as u16) << 8) | buf[k + 1] as u16);
                }
            }
            if total > MAX_INSTRS_PER_FUNCTION {
                return Err(format!("function at {entry:#x} exceeds {MAX_INSTRS_PER_FUNCTION} instructions"));
            }
            let next = instr.next_ip() as u32;
            if instr.code() == iced_x86::Code::Pushd_imm32 {
                let v = instr.immediate32();
                // (discovery may list such a label as a function: it is a pushed code
                // address; Delphi places nested routines before their parent, not after)
                if v > ip && v - ip <= 0x1000 && !ret_targets.contains(&v) {
                    ret_targets.push(v);
                    queue.push_back(v);
                }
            }
            instrs.push(instr);
            // iced treats hlt/int3/int n as ordinary instructions; for translated user code
            // they all hand control to the runtime, so they end the block.
            if matches!(instr.mnemonic(), Mnemonic::Hlt | Mnemonic::Int3 | Mnemonic::Int | Mnemonic::Ud2 | Mnemonic::Into | Mnemonic::Bound | Mnemonic::Int1) {
                term = Term::Trap;
                break;
            }
            // Runs of `add [eax],al` (00 00) are zero-filled memory, not code.
            if instr.code() == iced_x86::Code::Add_rm8_r8 && buf[0] == 0 && buf[1] == 0 {
                zero_run += 1;
                if zero_run >= 8 {
                    term = Term::Bad(ip);
                    break;
                }
            } else {
                zero_run = 0;
            }
            match instr.flow_control() {
                FlowControl::Call if is_near(&instr) && pops_return_address(mem, instr.near_branch_target() as u32, next) => {
                    // `call x` where x starts with `pop r32` (Delphi MakeObjectInstance and
                    // similar thunks): the bytes after the call are data the callee consumes.
                    term = Term::Bad(next);
                }
                FlowControl::Call
                    if inline
                        && sites < MAX_INLINE_SITES
                        && is_near(&instr)
                        && instr.near_branch_target() as u32 != entry
                        && let Some(callee) = inlinable(mem, instr.near_branch_target() as u32, info, depth - 1, cache, bits)
                        && callee.blocks.values().map(|b| b.instrs.len()).sum::<usize>() <= budget =>
                {
                    budget -= callee.blocks.values().map(|b| b.instrs.len()).sum::<usize>();
                    // a small function: its blocks join this function's under keys of
                    // their own; its returns and tail jumps come back here
                    let target = instr.near_branch_target() as u32;
                    sites += 1;
                    let mut keys = HashMap::default();
                    for &k in callee.blocks.keys() {
                        keys.insert(k, next_key);
                        next_key += 1;
                    }
                    let key = |a: u32| keys[&a];
                    for (&k, b) in &callee.blocks {
                        let t = match b.term.clone() {
                            Term::Fall(a) => Term::Fall(key(a)),
                            Term::Jump(a) => Term::Jump(key(a)),
                            Term::Cond { taken, next: n } => Term::Cond { taken: key(taken), next: key(n) },
                            Term::TailCall(t) => {
                                call_flags.entry(t).or_insert_with(|| entry_flag_reads(mem, t, bits));
                                if let Some(i) = (info.translated)(t) {
                                    known.insert(t, i);
                                }
                                Term::InlineTail { target: t, ret_to: next, ret_key: next }
                            }
                            Term::InlineCall { entry: e } => Term::InlineCall { entry: key(e) },
                            Term::InlineRet { ret_to, ret_key } => Term::InlineRet { ret_to, ret_key: key(ret_key) },
                            Term::InlineTail { target: t, ret_to, ret_key } => Term::InlineTail { target: t, ret_to, ret_key: key(ret_key) },
                            _ => Term::InlineRet { ret_to: next, ret_key: next },
                        };
                        blocks.insert(key(k), Block { start: key(k), instrs: b.instrs.clone(), term: t });
                        if let Some(&c) = callee.checks.get(&k) {
                            checks.insert(key(k), c);
                        }
                    }
                    fop.extend(callee.fop.iter().map(|(&a, &b)| (a, b)));
                    call_flags.extend(callee.call_flags.iter().map(|(&a, &b)| (a, b)));
                    known.extend(callee.known.iter().map(|(&a, &b)| (a, b)));
                    queue.push_back(next);
                    term = Term::InlineCall { entry: key(target) };
                }
                FlowControl::Next | FlowControl::Call | FlowControl::IndirectCall => {
                    if instr.flow_control() == FlowControl::Call && is_near(&instr) {
                        let t = instr.near_branch_target() as u32;
                        call_flags.entry(t).or_insert_with(|| entry_flag_reads(mem, t, bits));
                        if let Some(i) = (info.translated)(t) {
                            known.insert(t, i);
                        }
                    }
                    ip = next;
                    continue;
                }
                FlowControl::ConditionalBranch => {
                    let taken = instr.near_branch_target() as u32;
                    term = Term::Cond { taken, next };
                }
                FlowControl::UnconditionalBranch => {
                    let target = instr.near_branch_target() as u32;
                    // A jump into zero-filled memory (a packer's stub jumping to the code it
                    // unpacks there) goes through the dispatcher, so the target is
                    // translated when it runs, not from the bytes it has now.
                    term = if target != entry && ((info.is_function)(target) || zero_filled(mem, target)) { Term::TailCall(target) } else { Term::Jump(target) };
                    if let (Term::TailCall(t), Some(i)) = (&term, (info.translated)(target)) {
                        known.insert(*t, i);
                    }
                }
                FlowControl::IndirectBranch => {
                    term = match info.jump_tables.get(&(instr.ip() as u32)) {
                        Some(t) if !t.is_empty() => Term::Table(t.clone()),
                        _ => Term::IndirectTail,
                    };
                }
                FlowControl::Return => term = Term::Ret,
                FlowControl::Interrupt | FlowControl::Exception | FlowControl::XbeginXabortXend => {
                    // int3/int n/ud2/hlt: the runtime decides; translated code stops here.
                    term = Term::Trap;
                }
            }
            break;
        }
        let block = Block { start, instrs, term };
        if guard && let Some(first) = block.instrs.first() {
            // the bytes the block was decoded from, as far as its first instruction goes
            let at = first.ip() as u32;
            let len: u8 = match first.len() {
                1 => 1,
                2 | 3 => 2,
                _ => 4,
            };
            let mut b = [0u8; 4];
            if mem.read(at, &mut b[..len as usize]) {
                checks.insert(start, (at, len, u32::from_le_bytes(b)));
            }
        }
        queue.extend(block.successors());
        blocks.insert(start, block);
    }
    Ok(Cfg { entry, blocks, fop, ret_targets, call_flags, known, guard: if guard { info.tag } else { 0 }, checks })
}

/// The flags the code at `entry` reads before writing them, following its first
/// instructions (straight on and through direct jumps); everything not yet written
/// when the path branches or ends there.
pub fn entry_flag_reads(mem: &dyn GuestMem, entry: u32, bits: Bits) -> u8 {
    let (mut ip, mut written, mut reads) = (entry, 0u8, 0u8);
    let mut buf = [0u8; 16];
    for _ in 0..24 {
        let n = (0..16).rev().find(|&n| mem.read(ip, &mut buf[..n])).unwrap_or(0);
        let instr = Decoder::with_ip(bits.decoder_bits(), &buf[..n], bits.ip(ip), DecoderOptions::NONE).decode();
        if n == 0 || instr.is_invalid() {
            break;
        }
        let (def, used) = crate::flags::effects(&instr);
        reads |= used & !written;
        written |= def;
        if written == crate::flags::ALL {
            return reads;
        }
        match instr.flow_control() {
            FlowControl::Next => ip = instr.next_ip() as u32,
            FlowControl::UnconditionalBranch if is_near(&instr) => ip = instr.near_branch_target() as u32,
            _ => break,
        }
    }
    reads | (crate::flags::ALL & !written)
}

/// An x87 instruction (its mnemonic starts with F), by table.
pub fn is_x87(m: Mnemonic) -> bool {
    static TABLE: std::sync::LazyLock<Vec<bool>> =
        std::sync::LazyLock::new(|| Mnemonic::values().map(|m| format!("{m:?}").starts_with('F')).collect());
    TABLE.get(m as usize).copied().unwrap_or(false)
}

/// Four zero bytes at `va` (`add [eax],al` twice): memory, not code.
fn zero_filled(mem: &dyn GuestMem, va: u32) -> bool {
    let mut b = [0u8; 4];
    mem.read(va, &mut b) && b == [0; 4]
}

/// The call target pops its return address right away (and is not the `call $+5`
/// get-PC idiom, whose continuation is real code).
fn pops_return_address(mem: &dyn GuestMem, target: u32, next: u32) -> bool {
    let mut b = [0u8; 1];
    target != next && mem.read(target, &mut b) && (0x58..=0x5F).contains(&b[0])
}

/// Whether an instruction's memory operand is just `[disp32]` with a jump-table shape.
pub fn is_table_jump(i: &Instruction) -> bool {
    i.mnemonic() == Mnemonic::Jmp
        && i.op0_kind() == OpKind::Memory
        && i.memory_base() == Register::None
        && i.memory_index() != Register::None
        && i.memory_index_scale() == 4
}
