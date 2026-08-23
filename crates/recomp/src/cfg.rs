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
}

const MAX_INSTRS_PER_FUNCTION: usize = 200_000;

pub fn build(mem: &dyn GuestMem, entry: u32, info: &FuncInfo) -> Result<Cfg, String> {
    let mut blocks: BTreeMap<u32, Block> = BTreeMap::new();
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
                let b = blocks.remove(&bstart).unwrap();
                let split = b.instrs.iter().position(|i| i.ip() as u32 == start);
                let Some(split) = split else {
                    return Err(format!("branch into the middle of an instruction at {start:#x}"));
                };
                let (head, tail) = b.instrs.split_at(split);
                blocks.insert(bstart, Block { start: bstart, instrs: head.to_vec(), term: Term::Fall(start) });
                blocks.insert(start, Block { start, instrs: tail.to_vec(), term: b.term.clone() });
                continue;
            }
        }
        let mut instrs = Vec::new();
        let mut ip = start;
        let term;
        loop {
            if blocks.contains_key(&ip) && ip != start {
                term = Term::Fall(ip);
                break;
            }
            let n = (0..16).rev().find(|&n| mem.read(ip, &mut buf[..n])).unwrap_or(0);
            if n == 0 {
                return Err(format!("unmapped code at {ip:#x}"));
            }
            let mut decoder = Decoder::with_ip(32, &buf[..n], ip as u64, DecoderOptions::NONE);
            let instr = decoder.decode();
            if instr.is_invalid() {
                return Err(format!("invalid instruction at {ip:#x}"));
            }
            total += 1;
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
            match instr.flow_control() {
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
    Ok(Cfg { entry, blocks })
}

/// Whether an instruction's memory operand is just `[disp32]` with a jump-table shape.
pub fn is_table_jump(i: &Instruction) -> bool {
    i.mnemonic() == Mnemonic::Jmp
        && i.op0_kind() == OpKind::Memory
        && i.memory_base() == Register::None
        && i.memory_index() != Register::None
        && i.memory_index_scale() == 4
}
