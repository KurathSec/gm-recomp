//! Test-case layout shared with tests/cpu/refrun.c (protocol "GMR2").

pub const DATA_BASE: u32 = 0x2000_0000;
pub const DATA_SIZE: usize = 0x800;
pub const CODE_BASE: u32 = 0x2000_1000;
pub const CODE_MAX: usize = 0x1000;
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
