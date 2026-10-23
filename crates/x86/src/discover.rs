use iced_x86::{Decoder, DecoderOptions, FlowControl, Instruction, Mnemonic, OpKind, Register};
use pe::Pe;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum RootKind {
    Entry,
    Export,
    TlsCallback,
    /// A function the exception directory (`.pdata`) of a 64-bit image lists.
    Pdata,
    /// Direct `call rel32` target reached from committed code.
    Call,
    /// Target of a direct jump out of an already-known function (tail call or shared epilogue).
    Jump,
    /// Entry of a `jmp [idx*4+table]` switch table.
    JumpTable,
    /// Code address stored at a base-relocation site (vtables, function pointers, SEH handlers).
    Reloc,
    /// Immediate or displacement inside an instruction that points at code.
    Immediate,
    /// Aligned dword in a section that looks like a code pointer (images without relocations).
    PointerScan,
    /// `call rel32` found by a linear sweep over undecoded bytes.
    SweepCall,
    /// Common prologue found in an undecoded gap.
    Prologue,
}

impl RootKind {
    /// Roots that may point at data as easily as at code (Delphi keeps VMTs, RTTI and
    /// strings in `.text`, so relocation sites are not proof of code either). These are
    /// decoded speculatively and only committed when the whole function is clean.
    fn is_speculative(self) -> bool {
        matches!(
            self,
            RootKind::Reloc | RootKind::Immediate | RootKind::PointerScan | RootKind::SweepCall | RootKind::Prologue
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Function {
    pub rva: u32,
    pub kind: RootKind,
}

#[derive(Debug, Clone, Serialize)]
pub struct JumpTable {
    pub site_rva: u32,
    pub table_rva: u32,
    pub entries: u32,
    /// The cases' RVAs, for tables of 32-bit offsets (64-bit code: `mov r32, [b + x*4 +
    /// t]; add r, b; jmp r`, the offsets from `b`). Empty for tables of addresses,
    /// which are read from the (relocated) image.
    #[serde(skip)]
    pub targets: Vec<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Gap {
    pub rva: u32,
    pub bytes: u32,
    pub section: String,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct Discovery {
    pub exec_bytes: u64,
    /// Bytes of instructions reached from strong roots only.
    pub strong_bytes: u64,
    /// Bytes reached once speculative roots are added (includes resolved jump-table data).
    pub total_bytes: u64,
    pub functions: u32,
    pub functions_by_kind: BTreeMap<RootKind, u32>,
    pub basic_blocks: u32,
    pub instructions: u64,
    pub direct_calls: u32,
    pub indirect_calls: u32,
    pub indirect_jumps: u32,
    pub jump_tables: Vec<JumpTable>,
    pub unresolved_indirect_jumps: u32,
    pub invalid_decodes: u32,
    pub rejected_roots: u32,
    pub padding_bytes: u64,
    pub gap_count: u32,
    pub gap_bytes: u64,
    pub largest_gaps: Vec<Gap>,
    #[serde(skip)]
    pub function_list: Vec<Function>,
    /// Human-readable notes on unresolved jumps and rejected roots (capped).
    #[serde(skip)]
    pub diagnostics: Vec<String>,
    /// RVA of every discovered instruction, ascending.
    #[serde(skip)]
    pub instruction_starts: Vec<u32>,
}

const MAX_DIAGNOSTICS: usize = 400;
/// Speculative functions larger than this are accepted without finishing the walk.
const SPECULATION_LIMIT: usize = 4000;

const INSTR_START: u8 = 1;
const INSTR_BODY: u8 = 2;
/// Bytes known to be data (resolved jump tables) rather than instructions.
const DATA: u8 = 3;

/// Everything one straight-line block contributes, computed without touching shared state.
#[derive(Default)]
struct Block {
    marks: Vec<(u32, u32)>,
    successors: Vec<u32>,
    calls: Vec<u32>,
    immediates: Vec<u32>,
    tables: Vec<JumpTable>,
    instructions: u32,
    direct_calls: u32,
    indirect_calls: u32,
    indirect_jumps: u32,
    unresolved_jumps: Vec<(u32, String)>,
    /// Where decoding stopped and why, when it did not end in control flow.
    failure: Option<(u32, &'static str)>,
    /// The block ran into an already-committed instruction start (fine) or body (conflict).
    hit_body: bool,
}

struct Walker<'a> {
    pe: &'a Pe<'a>,
    /// Image laid out at RVA offsets so decoding can run straight over it.
    image: Vec<u8>,
    exec_ranges: Vec<(u32, u32)>,
    /// Per RVA: 0 unknown, INSTR_START, INSTR_BODY, DATA.
    state: Vec<u8>,
    queue: VecDeque<(u32, RootKind)>,
    functions: BTreeMap<u32, RootKind>,
    block_starts: BTreeSet<u32>,
    jump_tables: Vec<JumpTable>,
    instructions: u64,
    direct_calls: u32,
    indirect_calls: u32,
    indirect_jumps: u32,
    unresolved_indirect_jumps: u32,
    invalid_decodes: u32,
    rejected_roots: u32,
    diagnostics: Vec<String>,
    formatter: iced_x86::FastFormatter,
    /// x86-64 code (PE32+): decoded at bitness 64 with RVAs as addresses, so
    /// RIP-relative operands and branches give RVAs.
    x64: bool,
}

impl<'a> Walker<'a> {
    fn new(pe: &'a Pe<'a>) -> Self {
        let size = pe.size_of_image as usize;
        let mut image = vec![0u8; size];
        let hdr = (pe.size_of_headers as usize).min(pe.data.len()).min(size);
        image[..hdr].copy_from_slice(&pe.data[..hdr]);
        let mut exec_ranges = Vec::new();
        for s in &pe.sections {
            let (va, raw_off, raw_len) = (s.virtual_address as usize, s.raw_offset as usize, s.raw_size as usize);
            let copy = raw_len.min(s.mapped_size() as usize).min(size.saturating_sub(va));
            if let Some(src) = pe.data.get(raw_off..raw_off + copy) {
                image[va..va + copy].copy_from_slice(src);
            }
            if s.is_executable() {
                exec_ranges.push((s.virtual_address, s.virtual_address + s.mapped_size()));
            }
        }
        Walker {
            pe,
            image,
            exec_ranges,
            state: vec![0; size],
            queue: VecDeque::new(),
            functions: BTreeMap::new(),
            block_starts: BTreeSet::new(),
            jump_tables: Vec::new(),
            instructions: 0,
            direct_calls: 0,
            indirect_calls: 0,
            indirect_jumps: 0,
            unresolved_indirect_jumps: 0,
            invalid_decodes: 0,
            rejected_roots: 0,
            diagnostics: Vec::new(),
            formatter: iced_x86::FastFormatter::new(),
            x64: pe.pe64,
        }
    }

    /// The RVA of a 64-bit image's address (`image_base64`-based) inside the image.
    fn va64_to_rva(&self, va: u64) -> Option<u32> {
        va.checked_sub(self.pe.image_base64).filter(|&r| r < self.pe.size_of_image as u64).map(|r| r as u32)
    }

    fn read_u64(&self, rva: u32) -> Option<u64> {
        let o = rva as usize;
        self.image.get(o..o + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    }

    fn note(&mut self, msg: String) {
        if self.diagnostics.len() < MAX_DIAGNOSTICS {
            self.diagnostics.push(msg);
        }
    }

    fn in_exec(&self, rva: u32) -> bool {
        self.exec_ranges.iter().any(|&(lo, hi)| rva >= lo && rva < hi)
    }

    fn va_in_exec(&self, va: u32) -> Option<u32> {
        self.pe.va_to_rva(va).filter(|&r| self.in_exec(r))
    }

    fn read_u32(&self, rva: u32) -> Option<u32> {
        let o = rva as usize;
        self.image.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn add_function(&mut self, rva: u32, kind: RootKind) {
        if !self.in_exec(rva) {
            return;
        }
        let known = self.functions.get(&rva).copied();
        // Keep the strongest evidence for a function entry.
        if known.is_none_or(|k| kind < k) {
            self.functions.insert(rva, kind);
        }
        if known.is_none() {
            self.queue.push_back((rva, kind));
        }
    }

    fn add_block(&mut self, rva: u32) {
        if self.in_exec(rva) && self.state[rva as usize] == 0 {
            self.block_starts.insert(rva);
            self.queue.push_back((rva, RootKind::Jump));
        }
    }

    /// Decodes one straight run of instructions. `overlay` holds marks made by the
    /// current speculative walk that are not committed to `state` yet.
    fn decode_block(&mut self, start: u32, overlay: &HashMap<u32, u8>, is_function_start: bool) -> Block {
        let mut b = Block::default();
        let mut decoder = Decoder::with_ip(if self.x64 { 64 } else { 32 }, &self.image, 0, DecoderOptions::NONE);
        let mut instr = Instruction::default();
        let mut rva = start;
        // 64-bit code: the block's last instructions, for switch tables
        let mut recent: Vec<Instruction> = Vec::new();
        loop {
            if !self.in_exec(rva) {
                b.failure = Some((rva, "runs off section"));
                break;
            }
            let st = overlay.get(&rva).copied().unwrap_or(self.state[rva as usize]);
            if st != 0 {
                b.hit_body = st != INSTR_START;
                break;
            }
            decoder.set_position(rva as usize).unwrap();
            decoder.set_ip(rva as u64);
            decoder.decode_out(&mut instr);
            if instr.is_invalid() {
                b.failure = Some((rva, "invalid opcode"));
                break;
            }
            let len = instr.len() as u32;
            if !self.in_exec(rva + len - 1) {
                b.failure = Some((rva, "runs off section"));
                break;
            }
            match instr.mnemonic() {
                // Privileged or nonsensical in user code: a strong sign we are decoding data.
                Mnemonic::In | Mnemonic::Out | Mnemonic::Hlt | Mnemonic::Cli | Mnemonic::Sti | Mnemonic::Lgdt
                | Mnemonic::Lidt | Mnemonic::Ud2 | Mnemonic::Insb | Mnemonic::Insd | Mnemonic::Outsb | Mnemonic::Outsd
                | Mnemonic::Iretd | Mnemonic::Sysenter | Mnemonic::Sysexit | Mnemonic::Sysret | Mnemonic::Rsm
                // Never emitted by compilers for user code; they only show up when data is decoded.
                | Mnemonic::Arpl | Mnemonic::Sldt | Mnemonic::Lldt | Mnemonic::Ltr | Mnemonic::Str | Mnemonic::Invd
                | Mnemonic::Wbinvd | Mnemonic::Verr | Mnemonic::Verw | Mnemonic::Les | Mnemonic::Lds | Mnemonic::Lss
                | Mnemonic::Lfs | Mnemonic::Lgs | Mnemonic::Salc | Mnemonic::Into | Mnemonic::Int1 | Mnemonic::Bound
                | Mnemonic::Aam | Mnemonic::Aad | Mnemonic::Aaa | Mnemonic::Aas | Mnemonic::Daa | Mnemonic::Das
                | Mnemonic::Retf | Mnemonic::Lar | Mnemonic::Lsl | Mnemonic::Clts | Mnemonic::Lmsw | Mnemonic::Smsw => {
                    b.failure = Some((rva, "privileged instruction"));
                    break;
                }
                // (Windows makes system calls in ntdll only)
                Mnemonic::Syscall | Mnemonic::Sysretq | Mnemonic::Swapgs | Mnemonic::Iretq if self.x64 => {
                    b.failure = Some((rva, "privileged instruction"));
                    break;
                }
                _ => {}
            }
            // Win32 user code never uses GS (Win64 code addresses its TEB with it); an SS
            // override is legal but never emitted by compilers.
            if instr.segment_prefix() == Register::SS || instr.segment_prefix() == Register::GS && !self.x64 {
                b.failure = Some((rva, "gs/ss prefix"));
                break;
            }
            b.marks.push((rva, len));
            b.instructions += 1;
            self.collect_immediates(&instr, &mut b.immediates);
            if self.x64 {
                if recent.len() == SWITCH_WINDOW {
                    recent.remove(0);
                }
                recent.push(instr);
            }
            let next = rva + len;
            match instr.flow_control() {
                FlowControl::Next => rva = next,
                FlowControl::Call => {
                    b.direct_calls += 1;
                    let target = instr.near_branch_target() as u32;
                    // `call $+5` is a get-pc idiom, not a function.
                    if target != next {
                        b.calls.push(target);
                    }
                    rva = next;
                }
                FlowControl::IndirectCall => {
                    b.indirect_calls += 1;
                    rva = next;
                }
                FlowControl::ConditionalBranch => {
                    b.successors.push(instr.near_branch_target() as u32);
                    b.successors.push(next);
                    break;
                }
                FlowControl::UnconditionalBranch => {
                    let target = instr.near_branch_target() as u32;
                    // A backward jump at a function's very first instruction, or a jump into
                    // a known function, is a tail call rather than part of this function.
                    if self.functions.contains_key(&target) || (is_function_start && target < start) {
                        b.calls.push(target);
                    } else {
                        b.successors.push(target);
                    }
                    break;
                }
                FlowControl::IndirectBranch => {
                    b.indirect_jumps += 1;
                    let table = if self.x64 { self.jump_table64(rva, &recent, overlay) } else { self.jump_table_entries(&instr, overlay).map(|(t, e)| (t, e, false)) };
                    match table {
                        Some((table_rva, entries, offsets)) => {
                            b.successors.extend(entries.iter().copied());
                            let targets = if offsets { entries.clone() } else { Vec::new() };
                            b.tables.push(JumpTable { site_rva: rva, table_rva, entries: entries.len() as u32, targets });
                        }
                        None => {
                            let mut text = String::new();
                            self.formatter.format(&instr, &mut text);
                            b.unresolved_jumps.push((rva, text));
                        }
                    }
                    break;
                }
                FlowControl::Return | FlowControl::Interrupt | FlowControl::Exception | FlowControl::XbeginXabortXend => {
                    break
                }
            }
        }
        b
    }

    /// `push imm32` / `mov reg, imm32` / `lea`-style displacements that land in code
    /// are callbacks, SEH handlers or vtable fills.
    fn collect_immediates(&self, instr: &Instruction, out: &mut Vec<u32>) {
        if self.x64 {
            // 64-bit addresses, and RIP-relative operands (`lea rcx, [rip+callback]`)
            for i in 0..instr.op_count() {
                let candidate = match instr.op_kind(i) {
                    OpKind::Immediate64 | OpKind::Immediate32to64 => self.va64_to_rva(instr.immediate(i)),
                    OpKind::Memory if instr.memory_base() == Register::RIP => Some(instr.memory_displacement64() as u32),
                    _ => None,
                };
                if let Some(rva) = candidate.filter(|&r| self.in_exec(r)) {
                    out.push(rva);
                }
            }
            return;
        }
        for i in 0..instr.op_count() {
            let candidate = match instr.op_kind(i) {
                OpKind::Immediate32 => Some(instr.immediate32()),
                OpKind::Memory if instr.memory_base() == Register::None && instr.memory_index() == Register::None => {
                    Some(instr.memory_displacement32())
                }
                _ => None,
            };
            if let Some(rva) = candidate.and_then(|va| self.va_in_exec(va)) {
                out.push(rva);
            }
        }
    }

    /// Resolves `jmp dword ptr [idx*4 + table]` by reading entries until one leaves the code sections.
    fn jump_table_entries(&self, instr: &Instruction, overlay: &HashMap<u32, u8>) -> Option<(u32, Vec<u32>)> {
        if instr.op0_kind() != OpKind::Memory
            || instr.memory_base() != Register::None
            || instr.memory_index() == Register::None
            || instr.memory_index_scale() != 4
        {
            return None;
        }
        let table_rva = self.pe.va_to_rva(instr.memory_displacement32())?;
        let mut entries = Vec::new();
        for i in 0..4096u32 {
            let Some(va) = self.read_u32(table_rva + i * 4) else { break };
            let Some(rva) = self.va_in_exec(va) else { break };
            // Tables are usually followed by other data; stop when an entry hits a known instruction body.
            let st = overlay.get(&rva).copied().unwrap_or(self.state[rva as usize]);
            if matches!(st, INSTR_BODY | DATA) {
                break;
            }
            entries.push(rva);
        }
        (!entries.is_empty()).then_some((table_rva, entries))
    }

    /// A 64-bit switch's table (`switch64`), the base register set by a `lea b,
    /// [rip+...]` before the load (MSVC: the image base, the table holding RVAs; GCC: the
    /// table itself, holding offsets from it). MSVC often loads the base before the
    /// bounds check, a block earlier: with no `lea` in the block, a table at a nonzero
    /// displacement is taken to be the image base's when every case lies near the jump.
    /// Returns the table's RVA and the cases' RVAs, read until one leaves the code.
    fn jump_table64(&self, site: u32, recent: &[Instruction], overlay: &HashMap<u32, u8>) -> Option<(u32, Vec<u32>, bool)> {
        let sw = switch64(recent)?;
        let known = recent[..sw.load]
            .iter()
            .rev()
            .find(|i| writes(i, sw.base))
            .filter(|i| i.mnemonic() == Mnemonic::Lea && i.memory_base() == Register::RIP)
            .map(|i| i.memory_displacement64() as u32);
        let disp = sw.disp as u32;
        let base_rva = known.or((disp != 0).then_some(0))?;
        let table_rva = base_rva.wrapping_add(disp);
        let mut entries = Vec::new();
        for i in 0..4096u32 {
            let Some(off) = self.read_u32(table_rva.wrapping_add(i * 4)) else { break };
            let rva = base_rva.wrapping_add(off);
            if !self.in_exec(rva) || known.is_none() && rva.abs_diff(site) > 0x10_0000 {
                break;
            }
            let st = overlay.get(&rva).copied().unwrap_or(self.state[rva as usize]);
            if matches!(st, INSTR_BODY | DATA) {
                break;
            }
            entries.push(rva);
        }
        (!entries.is_empty()).then_some((table_rva, entries, true))
    }

    fn commit(&mut self, b: Block, kind: RootKind, start: u32) {
        for &(r, len) in &b.marks {
            self.state[r as usize] = INSTR_START;
            for x in r + 1..r + len {
                self.state[x as usize] = INSTR_BODY;
            }
        }
        for t in &b.tables {
            // MSVC and Delphi place switch tables in the code section; account for them as data.
            for x in t.table_rva..t.table_rva + t.entries * 4 {
                if let Some(s) = self.state.get_mut(x as usize) {
                    if *s == 0 {
                        *s = DATA;
                    }
                }
            }
        }
        self.instructions += b.instructions as u64;
        self.direct_calls += b.direct_calls;
        self.indirect_calls += b.indirect_calls;
        self.indirect_jumps += b.indirect_jumps;
        self.unresolved_indirect_jumps += b.unresolved_jumps.len() as u32;
        for (rva, text) in b.unresolved_jumps {
            self.note(format!("unresolved jump @{rva:#x}: {text}"));
        }
        if let Some((at, why)) = b.failure {
            self.invalid_decodes += 1;
            self.note(format!("bad decode in {kind:?} root @{start:#x} ({why} @{at:#x})"));
        }
        self.jump_tables.extend(b.tables);
        for s in b.successors {
            self.add_block(s);
        }
        for c in b.calls {
            self.add_function(c, RootKind::Call);
        }
        for i in b.immediates {
            self.add_function(i, RootKind::Immediate);
        }
    }

    /// Strong roots: decode and commit block by block, recording anything odd.
    fn walk_strong(&mut self, start: u32, kind: RootKind) {
        if self.state[start as usize] != 0 {
            return;
        }
        let b = self.decode_block(start, &HashMap::new(), kind != RootKind::Jump);
        self.commit(b, kind, start);
    }

    /// Speculative roots: decode the whole function into an overlay first and commit only
    /// when every block ends in control flow and nothing lands inside committed instructions.
    fn walk_speculative(&mut self, start: u32, kind: RootKind) -> bool {
        if self.state[start as usize] != 0 {
            return false;
        }
        let mut overlay: HashMap<u32, u8> = HashMap::new();
        let mut pending = vec![start];
        let mut blocks: Vec<Block> = Vec::new();
        let mut decoded = 0usize;
        while let Some(rva) = pending.pop() {
            if !self.in_exec(rva) || overlay.contains_key(&rva) || self.state[rva as usize] != 0 {
                if self.in_exec(rva) && overlay.get(&rva).copied().unwrap_or(self.state[rva as usize]) == INSTR_BODY {
                    self.reject(kind, start, rva, "branch into instruction body");
                    return false;
                }
                continue;
            }
            let b = self.decode_block(rva, &overlay, rva == start);
            if let Some((at, why)) = b.failure {
                self.reject(kind, start, at, why);
                return false;
            }
            if b.hit_body {
                self.reject(kind, start, rva, "falls into instruction body");
                return false;
            }
            for &(r, len) in &b.marks {
                overlay.insert(r, INSTR_START);
                for x in r + 1..r + len {
                    overlay.insert(x, INSTR_BODY);
                }
            }
            decoded += b.instructions as usize;
            pending.extend(b.successors.iter().copied());
            blocks.push(b);
            if decoded > SPECULATION_LIMIT {
                break;
            }
        }
        for b in blocks {
            let first = b.marks.first().map(|m| m.0).unwrap_or(start);
            if first != start {
                self.block_starts.insert(first);
            }
            self.commit(b, kind, start);
        }
        true
    }

    fn reject(&mut self, kind: RootKind, start: u32, at: u32, why: &str) {
        self.rejected_roots += 1;
        let head = &self.image[start as usize..(start as usize + 12).min(self.image.len())];
        let hex: String = head.iter().map(|b| format!("{b:02x}")).collect();
        self.note(format!("rejected {kind:?} root @{start:#x} ({why} @{at:#x}): {hex}"));
    }

    fn drain(&mut self) {
        while let Some((rva, kind)) = self.queue.pop_front() {
            if kind.is_speculative() {
                if !self.walk_speculative(rva, kind) && self.functions.get(&rva) == Some(&kind) {
                    self.functions.remove(&rva);
                }
            } else {
                self.walk_strong(rva, kind);
            }
        }
    }

    fn covered_bytes(&self) -> u64 {
        self.state.iter().filter(|&&s| s != 0).count() as u64
    }

    /// Candidates whose target sits right after padding or another instruction are
    /// far more likely to be real entries; process those first so a stray value
    /// cannot eat a real function's bytes.
    fn confidence(&self, rva: u32) -> u8 {
        let prev_state = rva.checked_sub(1).map(|p| self.state[p as usize]).unwrap_or(0);
        let prev_byte = rva.checked_sub(1).map(|p| self.image[p as usize]).unwrap_or(0xCC);
        let head = &self.image[rva as usize..(rva as usize + 3).min(self.image.len())];
        let prologue = if self.x64 { prologue64(&self.image[rva as usize..]) } else { head == [0x55, 0x8B, 0xEC] || head == [0x8B, 0xFF, 0x55] || head.first() == Some(&0x53) };
        let after_pad = matches!(prev_byte, 0xCC | 0xC3 | 0x90 | 0x00 | 0xC0) || prev_state == INSTR_START || prev_state == INSTR_BODY;
        (prologue as u8) * 2 + (after_pad as u8) + (rva % 16 == 0) as u8
    }

    fn run_candidates(&mut self, mut found: Vec<u32>, kind: RootKind) {
        found.sort_unstable();
        found.dedup();
        let mut scored: Vec<(u8, u32)> = found.into_iter().map(|r| (self.confidence(r), r)).collect();
        scored.sort_by_key(|&(c, r)| (std::cmp::Reverse(c), r));
        for (_, t) in scored {
            if self.state[t as usize] == 0 {
                self.add_function(t, kind);
                self.drain();
            }
        }
    }

    /// Dword-aligned values anywhere in the image that point at undecoded code.
    fn pointer_scan(&mut self) {
        let mut found = Vec::new();
        if self.x64 {
            // qword-aligned 64-bit addresses
            for s in &self.pe.sections {
                let lo = s.virtual_address as usize;
                let hi = (lo + s.mapped_size() as usize).min(self.image.len());
                for rva in (lo.next_multiple_of(8)..hi.saturating_sub(8)).step_by(8) {
                    let va = u64::from_le_bytes(self.image[rva..rva + 8].try_into().unwrap());
                    if let Some(target) = self.va64_to_rva(va).filter(|&t| self.in_exec(t) && self.state[t as usize] == 0) {
                        found.push(target);
                    }
                }
            }
            return self.run_candidates(found, RootKind::PointerScan);
        }
        for s in &self.pe.sections {
            let lo = s.virtual_address as usize;
            let hi = (lo + s.mapped_size() as usize).min(self.image.len());
            for rva in (lo..hi.saturating_sub(4)).step_by(4) {
                let va = u32::from_le_bytes(self.image[rva..rva + 4].try_into().unwrap());
                if let Some(target) = self.va_in_exec(va) {
                    if self.state[target as usize] == 0 {
                        found.push(target);
                    }
                }
            }
        }
        self.run_candidates(found, RootKind::PointerScan);
    }

    /// `E8 rel32` inside undecoded bytes whose target is undecoded code: the callers of a
    /// function we never reached are usually unreached themselves, so this breaks the chain.
    fn call_sweep(&mut self) {
        let mut found = Vec::new();
        for &(lo, hi) in &self.exec_ranges {
            for rva in lo..hi.saturating_sub(5) {
                if self.image[rva as usize] != 0xE8 || self.state[rva as usize] != 0 {
                    continue;
                }
                let rel = i32::from_le_bytes(self.image[rva as usize + 1..rva as usize + 5].try_into().unwrap());
                let target = (rva + 5).wrapping_add(rel as u32);
                if self.in_exec(target) && self.state[target as usize] == 0 && target != rva + 5 {
                    found.push(target);
                }
            }
        }
        self.run_candidates(found, RootKind::SweepCall);
    }

    /// `push ebp; mov ebp, esp` and `mov edi, edi; push ebp` inside gaps, after a terminator or padding.
    fn prologue_scan(&mut self) {
        let mut found = Vec::new();
        for &(lo, hi) in &self.exec_ranges.clone() {
            for rva in lo..hi.saturating_sub(3) {
                if self.state[rva as usize] != 0 {
                    continue;
                }
                let b = &self.image[rva as usize..(rva + 3) as usize];
                let prologue = if self.x64 { prologue64(&self.image[rva as usize..]) } else { b == [0x55, 0x8B, 0xEC] || b == [0x8B, 0xFF, 0x55] };
                if prologue && self.confidence(rva) >= 3 {
                    found.push(rva);
                }
            }
        }
        self.run_candidates(found, RootKind::Prologue);
    }

    fn padding_and_gaps(&self) -> (u64, Vec<Gap>) {
        let mut padding = 0u64;
        let mut gaps = Vec::new();
        for s in &self.pe.sections {
            if !s.is_executable() {
                continue;
            }
            let lo = s.virtual_address as usize;
            let hi = (lo + s.mapped_size() as usize).min(self.image.len());
            let mut i = lo;
            while i < hi {
                if self.state[i] != 0 {
                    i += 1;
                    continue;
                }
                let start = i;
                while i < hi && self.state[i] == 0 {
                    i += 1;
                }
                let bytes = &self.image[start..i];
                if bytes.iter().all(|&b| matches!(b, 0x00 | 0xCC | 0x90)) {
                    padding += bytes.len() as u64;
                } else {
                    gaps.push(Gap { rva: start as u32, bytes: bytes.len() as u32, section: s.name.clone() });
                }
            }
        }
        (padding, gaps)
    }
}

/// Instructions before a `jmp reg` that `switch64` looks through.
const SWITCH_WINDOW: usize = 12;

/// A 64-bit switch dispatch (`switch64`).
#[derive(Debug, Clone, Copy)]
pub struct Switch64 {
    /// Position of the table load in the instructions given.
    pub load: usize,
    /// The register the table's entries are offsets from (and the table is addressed by).
    pub base: Register,
    /// The case index register.
    pub index: Register,
    /// It still holds the index at the jump.
    pub index_kept: bool,
    /// The load's displacement (from `base`).
    pub disp: u64,
}

/// Whether the instruction writes (part of) `reg`.
pub fn writes(i: &Instruction, reg: Register) -> bool {
    use iced_x86::{InstructionInfoFactory, OpAccess};
    thread_local! {
        static INFO: std::cell::RefCell<InstructionInfoFactory> = std::cell::RefCell::new(InstructionInfoFactory::new());
    }
    let full = reg.full_register();
    INFO.with(|f| {
        f.borrow_mut().info(i).used_registers().iter().any(|u| {
            u.register().full_register() == full && matches!(u.access(), OpAccess::Write | OpAccess::CondWrite | OpAccess::ReadWrite | OpAccess::ReadCondWrite)
        })
    })
}

/// The 64-bit switch dispatch that `instrs` ends with: `mov r32, [b + x*4 + d]` (or
/// movsxd), `add` of `r` and `b` (either way round), `jmp` to the sum, other
/// instructions in between as long as they leave what is used later (compilers
/// schedule unrelated work there).
pub fn switch64(instrs: &[Instruction]) -> Option<Switch64> {
    let (jmp, body) = instrs.split_last()?;
    let target = jmp.op0_register();
    if jmp.mnemonic() != Mnemonic::Jmp || jmp.op0_kind() != OpKind::Register || target.size() != 8 {
        return None;
    }
    let last_writer = |end: usize, reg: Register| body[..end].iter().rposition(|i| writes(i, reg));
    let at_add = last_writer(body.len(), target)?;
    let add = &body[at_add];
    if add.mnemonic() != Mnemonic::Add || add.op0_kind() != OpKind::Register || add.op1_kind() != OpKind::Register || add.op0_register() != target || add.op1_register().size() != 8 {
        return None;
    }
    let other = add.op1_register();
    // the loaded value is one of the two added registers, the base the other
    for (loaded, base) in [(target, other), (other, target)] {
        let Some(at) = last_writer(at_add, loaded) else { continue };
        let load = &body[at];
        let ok = matches!(load.mnemonic(), Mnemonic::Mov | Mnemonic::Movsxd)
            && load.op0_kind() == OpKind::Register
            && load.op0_register().full_register() == loaded
            && load.op1_kind() == OpKind::Memory
            && load.memory_base() == base
            && load.memory_index() != Register::None
            && load.memory_index_scale() == 4
            && load.memory_size().size() == 4;
        if !ok || last_writer(at_add, base).is_some_and(|w| w > at) {
            continue;
        }
        let index = load.memory_index();
        let index_kept = !body[at..].iter().any(|i| writes(i, index));
        return Some(Switch64 { load: at, base, index, index_kept, disp: load.memory_displacement64() });
    }
    None
}

/// Common MSVC x64 prologues: saving registers to the home area (`mov [rsp+8], rbx`
/// and kin), `sub rsp, n`, `push rbx`..`push rdi` with REX, `mov rax, rsp`/`mov r11, rsp`.
fn prologue64(b: &[u8]) -> bool {
    match b {
        [0x48 | 0x4C, 0x89, m, 0x24, ..] if m & 0xC7 == 0x44 => true,
        [0x48, 0x83 | 0x81, 0xEC, ..] => true,
        [0x40, 0x53..=0x57, ..] => true,
        [0x48, 0x8B, 0xC4, ..] | [0x4C, 0x8B, 0xDC, ..] => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PE32 with one code section holding hand-assembled functions:
    ///   entry: call f2; jmp [eax*4+table]       (table: case0, case1)
    ///   f2:    xor eax,eax; ret
    ///   case0: ret   case1: ret
    ///   junk:  invalid bytes referenced by a pointer in .data (must be rejected)
    fn build() -> Vec<u8> {
        let base = 0x400000u32;
        let code_rva = 0x1000u32;
        let mut code = Vec::new();
        let at = |c: &Vec<u8>| code_rva + c.len() as u32;
        // entry @0x1000: E8 rel32 -> f2 (placed at 0x1010)
        let f2 = 0x1010u32;
        code.push(0xE8);
        code.extend_from_slice(&(f2 as i32 - (at(&code) as i32 + 4)).to_le_bytes());
        // jmp dword ptr [eax*4 + table]  (FF 24 85 disp32), table @0x1020
        code.extend_from_slice(&[0xFF, 0x24, 0x85]);
        code.extend_from_slice(&(base + 0x1020).to_le_bytes());
        while at(&code) < f2 {
            code.push(0xCC);
        }
        code.extend_from_slice(&[0x31, 0xC0, 0xC3]); // f2
        while at(&code) < 0x1018 {
            code.push(0xCC);
        }
        code.push(0xC3); // case0 @0x1018
        code.push(0xC3); // case1 @0x1019
        while at(&code) < 0x1020 {
            code.push(0xCC);
        }
        code.extend_from_slice(&(base + 0x1018).to_le_bytes());
        code.extend_from_slice(&(base + 0x1019).to_le_bytes());
        code.extend_from_slice(&0u32.to_le_bytes()); // table terminator (not a code pointer)
        while at(&code) < 0x1030 {
            code.push(0xCC);
        }
        code.extend_from_slice(&[0x0F, 0x0B, 0xF4, 0xF4]); // junk @0x1030: ud2, hlt
        code.resize(0x200, 0xCC);
        // .data holds a pointer to junk and a pointer to f2 (pointer scan must accept f2 only).
        let mut data = Vec::new();
        data.extend_from_slice(&(base + 0x1030).to_le_bytes());
        data.extend_from_slice(&(base + f2).to_le_bytes());
        data.resize(0x200, 0);

        let mut d = vec![0u8; 0x400];
        d[0..2].copy_from_slice(b"MZ");
        d[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        d[0x80..0x84].copy_from_slice(b"PE\0\0");
        let coff = 0x84;
        d[coff..coff + 2].copy_from_slice(&0x14cu16.to_le_bytes());
        d[coff + 2..coff + 4].copy_from_slice(&2u16.to_le_bytes());
        d[coff + 16..coff + 18].copy_from_slice(&224u16.to_le_bytes());
        d[coff + 18..coff + 20].copy_from_slice(&0x0103u16.to_le_bytes()); // relocs stripped
        let opt = coff + 20;
        d[opt..opt + 2].copy_from_slice(&0x10bu16.to_le_bytes());
        d[opt + 16..opt + 20].copy_from_slice(&code_rva.to_le_bytes());
        d[opt + 28..opt + 32].copy_from_slice(&base.to_le_bytes());
        d[opt + 32..opt + 36].copy_from_slice(&0x1000u32.to_le_bytes());
        d[opt + 36..opt + 40].copy_from_slice(&0x200u32.to_le_bytes());
        d[opt + 56..opt + 60].copy_from_slice(&0x3000u32.to_le_bytes());
        d[opt + 60..opt + 64].copy_from_slice(&0x400u32.to_le_bytes());
        d[opt + 92..opt + 96].copy_from_slice(&16u32.to_le_bytes());
        let sec = opt + 224;
        let mut section = |i: usize, name: &[u8], rva: u32, raw_off: u32, flags: u32| {
            let s = sec + i * 40;
            d[s..s + name.len()].copy_from_slice(name);
            d[s + 8..s + 12].copy_from_slice(&0x200u32.to_le_bytes());
            d[s + 12..s + 16].copy_from_slice(&rva.to_le_bytes());
            d[s + 16..s + 20].copy_from_slice(&0x200u32.to_le_bytes());
            d[s + 20..s + 24].copy_from_slice(&raw_off.to_le_bytes());
            d[s + 36..s + 40].copy_from_slice(&flags.to_le_bytes());
        };
        section(0, b".text", 0x1000, 0x400, pe::IMAGE_SCN_CNT_CODE | pe::IMAGE_SCN_MEM_EXECUTE);
        section(1, b".data", 0x2000, 0x600, pe::IMAGE_SCN_MEM_READ | pe::IMAGE_SCN_MEM_WRITE);
        d.extend_from_slice(&code);
        d.extend_from_slice(&data);
        d
    }

    #[test]
    fn finds_calls_tables_and_rejects_junk() {
        let bytes = build();
        let pe = Pe::parse(&bytes).unwrap();
        let d = discover(&pe);
        let kinds: Vec<(u32, RootKind)> = d.function_list.iter().map(|f| (f.rva, f.kind)).collect();
        assert_eq!(kinds, vec![(0x1000, RootKind::Entry), (0x1010, RootKind::Call)]);
        assert_eq!(d.jump_tables.len(), 1);
        assert_eq!(d.jump_tables[0].entries, 2);
        assert_eq!(d.invalid_decodes, 0);
        assert_eq!(d.rejected_roots, 1, "the pointer to ud2/hlt must be rejected: {:?}", d.diagnostics);
        // entry (5+7) + f2 (3) + two case rets (2) + table data (8)
        assert_eq!(d.total_bytes, 12 + 3 + 2 + 8);
        assert_eq!(d.unresolved_indirect_jumps, 0);
    }
}

pub fn discover(pe: &Pe) -> Discovery {
    let mut w = Walker::new(pe);
    w.add_function(pe.entry_rva, RootKind::Entry);
    for e in pe.exports().unwrap_or_default() {
        if e.forwarder.is_none() {
            w.add_function(e.rva, RootKind::Export);
        }
    }
    if let Ok(Some(tls)) = pe.tls() {
        for cb in tls.callbacks {
            if let Some(rva) = pe.va_to_rva(cb) {
                w.add_function(rva, RootKind::TlsCallback);
            }
        }
    }
    if pe.pe64 {
        // RUNTIME_FUNCTION entries (begin, end, unwind info), 12 bytes each; a chained
        // one (UNW_FLAG_CHAININFO) is a part of the function before it, not an entry
        let dir = pe.dir(3);
        for k in 0..dir.size / 12 {
            let at = dir.rva + 12 * k;
            let (Ok(begin), Ok(unwind)) = (pe.u32_at_rva(at), pe.u32_at_rva(at + 8)) else { break };
            let chained = pe.bytes_at(unwind & !1, 1).is_ok_and(|b| (b[0] >> 3) & 4 != 0);
            if !chained && unwind & 1 == 0 {
                w.add_function(begin, RootKind::Pdata);
            }
        }
    }
    w.drain();
    let strong_bytes = w.covered_bytes();

    let mut reloc_targets = Vec::new();
    for site in pe.relocations().unwrap_or_default() {
        let target = if pe.pe64 { w.read_u64(site).and_then(|va| w.va64_to_rva(va)).filter(|&r| w.in_exec(r)) } else { w.read_u32(site).and_then(|va| w.va_in_exec(va)) };
        if let Some(rva) = target {
            reloc_targets.push(rva);
        }
    }
    w.run_candidates(reloc_targets, RootKind::Reloc);
    if pe.relocs_stripped() {
        w.pointer_scan();
    }
    w.call_sweep();
    w.prologue_scan();
    let total_bytes = w.covered_bytes();

    let (padding_bytes, mut gaps) = w.padding_and_gaps();
    gaps.sort_by_key(|g| std::cmp::Reverse(g.bytes));
    let gap_count = gaps.len() as u32;
    let gap_bytes = gaps.iter().map(|g| g.bytes as u64).sum();
    gaps.truncate(10);

    let mut functions_by_kind = BTreeMap::new();
    let mut function_list = Vec::with_capacity(w.functions.len());
    for (&rva, &kind) in &w.functions {
        *functions_by_kind.entry(kind).or_insert(0) += 1;
        function_list.push(Function { rva, kind });
    }
    Discovery {
        exec_bytes: w.exec_ranges.iter().map(|&(lo, hi)| (hi - lo) as u64).sum(),
        strong_bytes,
        total_bytes,
        functions: w.functions.len() as u32,
        functions_by_kind,
        basic_blocks: (w.block_starts.len() + w.functions.len()) as u32,
        instructions: w.instructions,
        direct_calls: w.direct_calls,
        indirect_calls: w.indirect_calls,
        indirect_jumps: w.indirect_jumps,
        jump_tables: w.jump_tables,
        unresolved_indirect_jumps: w.unresolved_indirect_jumps,
        invalid_decodes: w.invalid_decodes,
        rejected_roots: w.rejected_roots,
        padding_bytes,
        gap_count,
        gap_bytes,
        largest_gaps: gaps,
        function_list,
        diagnostics: w.diagnostics,
        instruction_starts: w.state.iter().enumerate().filter(|&(_, &s)| s == INSTR_START).map(|(i, _)| i as u32).collect(),
    }
}
