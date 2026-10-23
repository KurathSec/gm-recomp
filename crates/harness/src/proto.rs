//! Test-case layout shared with tests/cpu/refrun.c (protocol "GMR2").

pub const DATA_BASE: u32 = 0x2000_0000;
pub const DATA_SIZE: usize = 0x800;
pub const CODE_BASE: u32 = 0x2000_1000;
pub const CODE_MAX: usize = 0x1000;
pub const STACK_BASE: u32 = 0x2000_2000;
pub const STACK_SIZE: usize = 0x400;

#[derive(Clone)]
pub struct CaseIn {
    pub code: Vec<u8>,
    /// esp holds an offset into the stack window.
    pub regs: [u32; 8],
    pub eflags: u32,
    pub fx: [u8; 512],
    pub data: Vec<u8>,
    pub stack: Vec<u8>,
}

#[derive(Clone)]
pub struct CaseOut {
    pub status: u32,
    pub fault_off: u32,
    pub stack_base: u32,
    pub regs: [u32; 8],
    pub eflags: u32,
    pub fx: [u8; 512],
    pub data: Vec<u8>,
    pub stack: Vec<u8>,
}

pub const OUT_SIZE: usize = 12 + 32 + 4 + 512 + DATA_SIZE + STACK_SIZE;

impl CaseIn {
    pub fn new(code: Vec<u8>) -> Self {
        let mut fx = [0u8; 512];
        fx[0..2].copy_from_slice(&0x037fu16.to_le_bytes()); // FCW: all masked, extended precision
        fx[24..28].copy_from_slice(&0x1f80u32.to_le_bytes()); // MXCSR default
        CaseIn { code, regs: [0; 8], eflags: 0, fx, data: vec![0; DATA_SIZE], stack: vec![0; STACK_SIZE] }
    }

    pub fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&(self.code.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.code);
        for r in self.regs {
            out.extend_from_slice(&r.to_le_bytes());
        }
        out.extend_from_slice(&self.eflags.to_le_bytes());
        out.extend_from_slice(&self.fx);
        out.extend_from_slice(&self.data);
        out.extend_from_slice(&self.stack);
    }
}

pub fn write_batch(cases: &[CaseIn]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"GMR2");
    out.extend_from_slice(&(cases.len() as u32).to_le_bytes());
    for c in cases {
        c.write(&mut out);
    }
    out
}

/// A 64-bit test case, laid out exactly as `tests/cpu/refrun64.c` reads it (`#[repr(C)]`,
/// passed to the native reference by pointer). The wasm side reads the same fields.
#[repr(C)]
#[derive(Clone)]
pub struct CaseIn64 {
    /// rax, rcx, rdx, rbx, rsp (an offset into the stack window), rbp, rsi, rdi, r8..r15.
    pub regs: [u64; 16],
    pub rflags: u64,
    pub fx: [u8; 512],
    pub data: [u8; DATA_SIZE],
    pub code: [u8; CODE_MAX],
    pub code_len: u32,
    pub stack: [u8; STACK_SIZE],
}

#[repr(C)]
#[derive(Clone)]
pub struct CaseOut64 {
    pub regs: [u64; 16],
    pub rflags: u64,
    pub fx: [u8; 512],
    pub data: [u8; DATA_SIZE],
    pub stack: [u8; STACK_SIZE],
    pub status: u32,
    pub fault_off: u32,
}

impl CaseIn64 {
    pub fn new(code: Vec<u8>) -> Box<Self> {
        let mut fx = [0u8; 512];
        fx[0..2].copy_from_slice(&0x037fu16.to_le_bytes()); // FCW
        fx[24..28].copy_from_slice(&0x1f80u32.to_le_bytes()); // MXCSR
        let mut c = Box::new(CaseIn64 { regs: [0; 16], rflags: 0, fx, data: [0; DATA_SIZE], code: [0xF4; CODE_MAX], code_len: 0, stack: [0; STACK_SIZE] });
        let n = code.len().min(CODE_MAX);
        c.code[..n].copy_from_slice(&code[..n]);
        c.code_len = n as u32;
        c
    }
}

impl CaseOut64 {
    pub fn zeroed() -> Box<Self> {
        Box::new(CaseOut64 { regs: [0; 16], rflags: 0, fx: [0; 512], data: [0; DATA_SIZE], stack: [0; STACK_SIZE], status: 0, fault_off: 0 })
    }
}

pub fn read_results(bytes: &[u8], count: usize) -> Result<Vec<CaseOut>, String> {
    if bytes.len() != count * OUT_SIZE {
        return Err(format!("result file has {} bytes, expected {}", bytes.len(), count * OUT_SIZE));
    }
    let u32_at = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
    let mut v = Vec::with_capacity(count);
    for i in 0..count {
        let b = i * OUT_SIZE;
        let mut regs = [0u32; 8];
        for (k, r) in regs.iter_mut().enumerate() {
            *r = u32_at(b + 12 + k * 4);
        }
        let mut fx = [0u8; 512];
        fx.copy_from_slice(&bytes[b + 48..b + 560]);
        v.push(CaseOut {
            status: u32_at(b),
            fault_off: u32_at(b + 4),
            stack_base: u32_at(b + 8),
            regs,
            eflags: u32_at(b + 44),
            fx,
            data: bytes[b + 560..b + 560 + DATA_SIZE].to_vec(),
            stack: bytes[b + 560 + DATA_SIZE..b + OUT_SIZE].to_vec(),
        });
    }
    Ok(v)
}
