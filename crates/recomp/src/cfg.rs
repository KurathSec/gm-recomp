//! Per-function control-flow graph: straight-line blocks of decoded instructions
//! plus the branch structure the emitter turns into wasm control flow.

use iced_x86::{Decoder, DecoderOptions, FlowControl, Instruction, Mnemonic, OpKind, Register};
use std::collections::{BTreeMap, HashMap, VecDeque};

/// Read access to the guest image, by virtual address.
pub trait GuestMem {
    /// Copies bytes at `va` into `buf`; false if any byte is unmapped.
    fn read(&self, va: u32, buf: &mut [u8]) -> bool;
}

pub struct FuncInfo<'a> {
    /// Resolved `jmp [idx*4+table]` sites: site address -> targets.
    pub jump_tables: &'a HashMap<u32, Vec<u32>>,
    /// Addresses known to start functions; direct jumps there are tail calls.
    pub is_function: &'a dyn Fn(u32) -> bool,
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
}

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
            _ => Vec::new(),
        }
    }
}

pub struct Cfg {
    pub entry: u32,
    pub blocks: BTreeMap<u32, Block>,
    /// x87 instructions: ip -> 11-bit opcode (low 3 bits of the escape byte, then ModRM).
    pub fop: HashMap<u32, u16>,
}

const MAX_INSTRS_PER_FUNCTION: usize = 200_000;

pub fn build(mem: &dyn GuestMem, entry: u32, info: &FuncInfo) -> Result<Cfg, String> {
    let mut blocks: BTreeMap<u32, Block> = BTreeMap::new();
    let mut fop: HashMap<u32, u16> = HashMap::new();
    let mut queue = VecDeque::from([entry]);
    let mut total = 0usize;
    let mut buf = vec![0u8; 16];
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
            let mut decoder = Decoder::with_ip(32, &buf[..n], ip as u64, DecoderOptions::NONE);
            let instr = decoder.decode();
            if instr.is_invalid() {
                term = Term::Bad(ip);
                break;
            }
            total += 1;
            if format!("{:?}", instr.mnemonic()).starts_with('F') && instr.mnemonic() != Mnemonic::Femms {
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
                FlowControl::Call if instr.op0_kind() == OpKind::NearBranch32 && pops_return_address(mem, instr.near_branch_target() as u32, next) => {
                    // `call x` where x starts with `pop r32` (Delphi MakeObjectInstance and
                    // similar thunks): the bytes after the call are data the callee consumes.
                    term = Term::Bad(next);
                }
                FlowControl::Next | FlowControl::Call | FlowControl::IndirectCall => {
                    ip = next;
                    continue;
                }
                FlowControl::ConditionalBranch => {
                    let taken = instr.near_branch_target() as u32;
                    term = Term::Cond { taken, next };
                }
                FlowControl::UnconditionalBranch => {
                    let target = instr.near_branch_target() as u32;
                    term = if target != entry && (info.is_function)(target) { Term::TailCall(target) } else { Term::Jump(target) };
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
        queue.extend(block.successors());
        blocks.insert(start, block);
    }
    Ok(Cfg { entry, blocks, fop })
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
