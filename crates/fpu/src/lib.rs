//! x87 FPU model: register stack, control/status/tag words and every instruction
//! the corpus uses, built on `rustc_apfloat` so arithmetic is bit-exact under
//! all three precision-control modes and four rounding modes.
//!
//! Transcendental instructions (fsin, fcos, fptan, fpatan, fyl2x, fyl2xp1, f2xm1)
//! are evaluated through f64 and are therefore accurate to about 53 bits, not the
//! ~64 the hardware delivers; everything else is exact.

use iced_x86::{MemorySize, Mnemonic};
use rustc_apfloat::ieee::{Double, IeeeFloat, Semantics, Single, X87DoubleExtended};
use rustc_apfloat::{Float, FloatConvert, Round, Status};
use std::cmp::Ordering;

pub type Ext = X87DoubleExtended;

/// 53-bit significand with the 15-bit exponent range: x87 precision control = double.
pub struct X87DoubleS;
impl Semantics for X87DoubleS {
    const BITS: usize = 80;
    const EXP_BITS: usize = 15;
    const PRECISION: usize = 53;
    const QNAN_SIGNIFICAND: u128 = 0b11 << (Self::PRECISION - 2);
}
pub type Pc53 = IeeeFloat<X87DoubleS>;

/// 24-bit significand with the 15-bit exponent range: x87 precision control = single.
pub struct X87SingleS;
impl Semantics for X87SingleS {
    const BITS: usize = 80;
    const EXP_BITS: usize = 15;
    const PRECISION: usize = 24;
    const QNAN_SIGNIFICAND: u128 = 0b11 << (Self::PRECISION - 2);
}
pub type Pc24 = IeeeFloat<X87SingleS>;

/// Wide intermediate (112-bit significand, 127 bits total so it fits `u128` for bit
/// access). Operations are truncated into it and a sticky bit is set when inexact, so a
/// final rounding to 24/53/64 bits in any mode equals a single rounding of the exact value.
pub struct WideS;
impl Semantics for WideS {
    const BITS: usize = 127;
    const EXP_BITS: usize = 15;
    const PRECISION: usize = 112;
    const QNAN_SIGNIFICAND: u128 = 0b11 << (Self::PRECISION - 2);
}
pub type Wide = IeeeFloat<WideS>;

/// Truncating wide operation with a sticky bit.
fn wide_sticky(op: Arith, a: Wide, b: Wide) -> (Wide, Status) {
    let r = match op {
        Arith::Add => a.add_r(b, Round::TowardZero),
        Arith::Sub => a.sub_r(b, Round::TowardZero),
        Arith::Mul => a.mul_r(b, Round::TowardZero),
        Arith::Div => a.div_r(b, Round::TowardZero),
    };
    let v = if r.status.contains(Status::INEXACT) && r.value.is_finite() && !r.value.is_zero() {
        Wide::from_bits(r.value.to_bits() | 1)
    } else {
        r.value
    };
    (v, r.status)
}

// Status word bits.
pub const SW_IE: u16 = 1 << 0;
pub const SW_DE: u16 = 1 << 1;
pub const SW_ZE: u16 = 1 << 2;
pub const SW_OE: u16 = 1 << 3;
pub const SW_UE: u16 = 1 << 4;
pub const SW_PE: u16 = 1 << 5;
pub const SW_SF: u16 = 1 << 6;
pub const SW_ES: u16 = 1 << 7;
pub const SW_C0: u16 = 1 << 8;
pub const SW_C1: u16 = 1 << 9;
pub const SW_C2: u16 = 1 << 10;
pub const SW_C3: u16 = 1 << 14;
pub const SW_B: u16 = 1 << 15;

pub trait Mem {
    fn read(&mut self, addr: u32, buf: &mut [u8]);
    fn write(&mut self, addr: u32, data: &[u8]);
}

/// Argument packing shared with the lifter (`recomp::lift::x87`).
pub mod args {
    /// `b` layout: bits 0-2 st index of op1, bits 8-15 iced MemorySize of a memory operand
    /// (0 = none), bit 16 op0 is an ST register, bit 17 op1 is an ST register.
    pub const OP0_IS_ST: u32 = 1 << 16;
    pub const OP1_IS_ST: u32 = 1 << 17;
    pub fn memsize(b: u32) -> u8 {
        (b >> 8) as u8
    }
}

#[derive(Clone)]
pub struct Fpu {
    /// Physical registers R0..R7 as raw 80-bit images; ST(i) = regs[(top + i) & 7].
    /// Raw bits are kept so unsupported encodings survive FXSAVE/FNSAVE round trips.
    regs: [u128; 8],
    /// Physical tags: 0 valid, 1 zero, 2 special, 3 empty.
    tags: [u8; 8],
    top: u8,
    cw: u16,
    /// Status word without the TOP field.
    sw: u16,
    /// Last non-control instruction: pointer, opcode and data pointer.
    fip: u32,
    fop: u16,
    fdp: u32,
    cur_fop: u16,
    cur_fdp: u32,
    /// A memory operand just loaded was a denormal single/double (the caller decides
    /// whether that raises DE: not when a NaN operand takes precedence).
    mem_denormal: bool,
    /// The current instruction's source operand comes from memory.
    mem_source: bool,
    /// A register operand had a zero exponent with nonzero significand (denormal or
    /// pseudo-denormal), which raises DE even when apfloat sees a normal value.
    reg_denormal: bool,
    /// An unmasked exception was raised by the current instruction: it is aborted
    /// (register stack and memory are left as they were; the status word keeps the flags).
    aborted: bool,
    /// Set by `operand_or_indef` when an operand was empty or unsupported: the
    /// instruction's result is then the indefinite QNaN regardless of other operands.
    fault_operand: bool,
    pub unmasked_pending: bool,
}

impl Default for Fpu {
    fn default() -> Self {
        Self::new()
    }
}

fn ext_from_bits(b: u128) -> Ext {
    Ext::from_bits(b & ((1u128 << 80) - 1))
}

fn round_mode(cw: u16) -> Round {
    match (cw >> 10) & 3 {
        0 => Round::NearestTiesToEven,
        1 => Round::TowardNegative,
        2 => Round::TowardPositive,
        _ => Round::TowardZero,
    }
}

fn status_bits(s: Status) -> u16 {
    let mut f = 0;
    if s.contains(Status::INVALID_OP) { f |= SW_IE; }
    if s.contains(Status::DIV_BY_ZERO) { f |= SW_ZE; }
    if s.contains(Status::OVERFLOW) { f |= SW_OE; }
    if s.contains(Status::UNDERFLOW) { f |= SW_UE; }
    if s.contains(Status::INEXACT) { f |= SW_PE; }
    f
}

/// Converts between formats, returning the value as Ext and the exception bits.
fn widen<S: Semantics>(v: IeeeFloat<S>) -> Ext {
    let mut lost = false;
    v.convert_r(Round::NearestTiesToEven, &mut lost).value
}

fn narrow<T: Semantics>(v: Ext, rm: Round) -> (IeeeFloat<T>, u16) {
    let mut lost = false;
    let r = v.convert_r(rm, &mut lost);
    (r.value, status_bits(r.status))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Arith {
    Add,
    Sub,
    Mul,
    Div,
}

impl Fpu {
    pub fn new() -> Self {
        Fpu { regs: [0; 8], tags: [3; 8], top: 0, cw: 0x037f, sw: 0, fip: 0, fop: 0, fdp: 0, cur_fop: 0, cur_fdp: 0, mem_denormal: false, mem_source: false, reg_denormal: false, aborted: false, fault_operand: false, unmasked_pending: false }
    }

    // ---- state access --------------------------------------------------------

    pub fn control_word(&self) -> u16 {
        self.cw
    }

    pub fn status_word(&self) -> u16 {
        (self.sw & !0x3800) | ((self.top as u16 & 7) << 11)
    }

    pub fn set_status_word(&mut self, sw: u16) {
        self.top = ((sw >> 11) & 7) as u8;
        self.sw = sw & !0x3800;
    }

    pub fn set_control_word(&mut self, cw: u16) {
        self.cw = (cw & 0x1f7f) | 0x40;
        self.refresh_es();
    }

    fn refresh_es(&mut self) {
        let unmasked = self.sw & 0x3f & !(self.cw & 0x3f);
        if unmasked != 0 {
            self.sw |= SW_ES | SW_B;
            self.unmasked_pending = true;
        } else {
            self.sw &= !(SW_ES | SW_B);
            self.unmasked_pending = false;
        }
    }

    /// Full 16-bit tag word (two bits per physical register).
    pub fn tag_word(&self) -> u16 {
        let mut t = 0u16;
        for i in 0..8 {
            t |= (self.tags[i] as u16) << (2 * i);
        }
        t
    }

    pub fn set_tag_word(&mut self, t: u16) {
        for i in 0..8 {
            self.tags[i] = ((t >> (2 * i)) & 3) as u8;
        }
    }

    fn phys(&self, i: u32) -> usize {
        ((self.top as u32 + i) & 7) as usize
    }

    pub fn st(&self, i: u32) -> Ext {
        ext_from_bits(self.regs[self.phys(i)])
    }

    pub fn st_bits(&self, i: u32) -> u128 {
        self.regs[self.phys(i)]
    }

    /// Unnormals and pseudo-NaN/infinities (explicit integer bit clear) are unsupported
    /// operands: they raise #IA and produce the indefinite QNaN.
    fn unsupported_bits(bits: u128) -> bool {
        let exp = (bits >> 64) & 0x7fff;
        exp != 0 && (bits >> 63) & 1 == 0
    }

    fn tag_for_bits(bits: u128) -> u8 {
        let exp = (bits >> 64) & 0x7fff;
        let mant = bits & ((1u128 << 64) - 1);
        if exp == 0 && mant == 0 {
            1
        } else if exp == 0x7fff || exp == 0 || Self::unsupported_bits(bits) {
            2
        } else {
            0
        }
    }

    pub fn is_empty(&self, i: u32) -> bool {
        self.tags[self.phys(i)] == 3
    }

    fn tag_for(v: Ext) -> u8 {
        if v.is_zero() {
            1
        } else if v.is_nan() || v.is_infinite() || v.is_denormal() {
            2
        } else {
            0
        }
    }

    fn set_st(&mut self, i: u32, v: Ext) {
        let p = self.phys(i);
        self.regs[p] = v.to_bits() & ((1u128 << 80) - 1);
        self.tags[p] = Self::tag_for(v);
    }

    /// Loads a value as the FXSAVE/FXRSTOR image would: raw bits plus a tag derived from them.
    pub fn set_st_raw(&mut self, i: u32, bits: u128, empty: bool) {
        let p = self.phys(i);
        self.regs[p] = bits & ((1u128 << 80) - 1);
        self.tags[p] = if empty { 3 } else { Self::tag_for_bits(self.regs[p]) };
    }

    /// Operand with the masked-underflow policy: an empty register or an unsupported
    /// encoding raises #IA and yields the indefinite QNaN, so the result is indefinite.
    fn operand_or_indef(&mut self, i: u32) -> Ext {
        if self.is_empty(i) {
            self.raise(SW_IE | SW_SF);
            self.clear_c1();
            self.fault_operand = true;
            return Self::indefinite();
        }
        let bits = self.st_bits(i);
        if Self::unsupported_bits(bits) {
            self.raise(SW_IE);
            self.clear_c1();
            self.fault_operand = true;
            return Self::indefinite();
        }
        if (bits >> 64) & 0x7fff == 0 && bits & ((1u128 << 64) - 1) != 0 {
            self.reg_denormal = true;
        }
        ext_from_bits(bits)
    }

    /// True when a waiting instruction would raise #MF because an unmasked exception is pending.
    pub fn fault_pending(&self, mn: Mnemonic) -> bool {
        use Mnemonic::*;
        self.unmasked_pending && !matches!(mn, Fninit | Fnclex | Fnstcw | Fnstsw | Fnstenv | Fnsave)
    }

    fn raise(&mut self, bits: u16) {
        self.sw |= bits;
        let unmasked = bits & !(self.cw & 0x3f) & 0x3f;
        if unmasked != 0 {
            self.fop = self.cur_fop;
            self.fdp = self.cur_fdp;
            // Pre-computation exceptions (invalid, denormal, zero divide) abort the
            // instruction; overflow/underflow/precision deliver a (biased) result.
            if unmasked & (SW_IE | SW_DE | SW_ZE) != 0 {
                self.aborted = true;
            }
        }
        self.refresh_es();
    }

    /// Unmasked overflow/underflow results are delivered with the exponent biased by
    /// 2^±24576 so a trap handler can recover them.
    fn bias_unmasked(&self, v: Ext, flags: u16) -> Ext {
        let unmasked = flags & !(self.cw & 0x3f);
        if unmasked & SW_OE != 0 && !v.is_nan() {
            if v.is_infinite() {
                // the unbiased value overflowed; scale the largest-magnitude value instead
                return Ext::largest().copy_sign(v).scalbn(-24576);
            }
            return v.scalbn(-24576);
        }
        if unmasked & SW_UE != 0 && !v.is_nan() {
            return v.scalbn(24576);
        }
        v
    }

    fn set_cc(&mut self, c0: bool, c1: bool, c2: bool, c3: bool) {
        self.sw &= !(SW_C0 | SW_C1 | SW_C2 | SW_C3);
        if c0 { self.sw |= SW_C0; }
        if c1 { self.sw |= SW_C1; }
        if c2 { self.sw |= SW_C2; }
        if c3 { self.sw |= SW_C3; }
    }

    fn clear_c1(&mut self) {
        self.sw &= !SW_C1;
    }

    fn push(&mut self, v: Ext) {
        self.top = self.top.wrapping_sub(1) & 7;
        if self.tags[self.top as usize] != 3 {
            // stack overflow: C1 = 1, IE, result is the indefinite QNaN when masked
            self.raise(SW_IE | SW_SF);
            self.sw |= SW_C1;
            if self.cw & 1 != 0 {
                let p = self.top as usize;
                self.regs[p] = Self::indefinite().to_bits();
                self.tags[p] = 2;
            }
            return;
        }
        let p = self.top as usize;
        self.regs[p] = v.to_bits() & ((1u128 << 80) - 1);
        self.tags[p] = Self::tag_for(v);
    }

    /// FNINIT semantics: control/status/tag words reset, register contents untouched.
    fn reinit(&mut self) {
        self.cw = 0x037f;
        self.sw = 0;
        self.top = 0;
        self.tags = [3; 8];
        self.fip = 0;
        self.fop = 0;
        self.fdp = 0;
        self.unmasked_pending = false;
    }

    fn push_would_overflow(&self) -> bool {
        self.tags[self.top.wrapping_sub(1) as usize & 7] != 3
    }

    fn push_raw(&mut self, bits: u128) {
        self.top = self.top.wrapping_sub(1) & 7;
        if self.tags[self.top as usize] != 3 {
            self.raise(SW_IE | SW_SF);
            self.sw |= SW_C1;
            if self.cw & 1 != 0 {
                let p = self.top as usize;
                self.regs[p] = Self::indefinite().to_bits();
                self.tags[p] = 2;
            }
            return;
        }
        let p = self.top as usize;
        self.regs[p] = bits & ((1u128 << 80) - 1);
        self.tags[p] = Self::tag_for_bits(self.regs[p]);
    }

    fn pop(&mut self) {
        let p = self.top as usize;
        self.tags[p] = 3;
        self.top = (self.top + 1) & 7;
    }

    pub const INDEFINITE_BITS: u128 = 0xFFFF_C000_0000_0000_0000;

    pub fn indefinite() -> Ext {
        ext_from_bits(Self::INDEFINITE_BITS)
    }

    fn set_st_nan(&mut self, i: u32, v: Ext) {
        // NaNs are stored through their raw image so sign and payload survive apfloat.
        let bits = if v.is_nan() { v.to_bits() } else { v.to_bits() };
        self.set_st_raw(i, bits, false);
    }

    /// Reads ST(i) for use as an operand; an empty register is a stack underflow.
    fn operand(&mut self, i: u32) -> Option<Ext> {
        if self.is_empty(i) {
            self.raise(SW_IE | SW_SF);
            self.clear_c1();
            return None;
        }
        Some(self.st(i))
    }

    // ---- rounding to the precision-control width ------------------------------

    /// Rounds a wide intermediate to the current precision and records exceptions.
    /// `pre` carries exception bits from the operation itself (e.g. inexact at 120 bits).
    fn round_result(&mut self, w: Wide, pre: u16) -> Ext {
        let rm = round_mode(self.cw);
        let (v, flags) = match (self.cw >> 8) & 3 {
            0 => {
                let mut lost = false;
                let r: rustc_apfloat::StatusAnd<Pc24> = w.convert_r(rm, &mut lost);
                (widen(r.value), status_bits(r.status))
            }
            2 => {
                let mut lost = false;
                let r: rustc_apfloat::StatusAnd<Pc53> = w.convert_r(rm, &mut lost);
                (widen(r.value), status_bits(r.status))
            }
            _ => {
                let mut lost = false;
                let r: rustc_apfloat::StatusAnd<Ext> = w.convert_r(rm, &mut lost);
                (r.value, status_bits(r.status))
            }
        };
        let flags = flags | (pre & (SW_PE | SW_OE | SW_UE));
        self.raise(flags);
        self.set_c1_rounded(v, w, flags);
        self.bias_unmasked(v, flags)
    }

    /// C1 reports whether an inexact result was rounded up (toward +infinity); 0 when exact.
    fn set_c1_rounded(&mut self, v: Ext, exact: Wide, flags: u16) {
        self.clear_c1();
        if flags & SW_PE != 0 && !v.is_nan() {
            let mut lost = false;
            let back: Wide = v.convert_r(Round::NearestTiesToEven, &mut lost).value;
            if back.abs() > exact.abs() || (v.is_infinite() && !exact.is_infinite()) {
                self.sw |= SW_C1;
            }
        }
    }

    fn arith(&mut self, op: Arith, a: Ext, b: Ext) -> Ext {
        let rm = round_mode(self.cw);
        let pc = (self.cw >> 8) & 3;
        let mut flags = 0;
        if self.fault_operand {
            self.clear_c1();
            return Self::indefinite();
        }
        if a.is_nan() || b.is_nan() {
            // NaN operands propagate; only signaling NaNs raise anything (no denormal check)
            if a.is_signaling() || b.is_signaling() {
                self.raise(SW_IE);
            }
            self.clear_c1();
            return self.nan_result(a, b);
        }
        let zero_divide = op == Arith::Div && b.is_zero() && !a.is_zero();
        if (a.is_denormal() || b.is_denormal() || self.mem_denormal || self.reg_denormal) && !zero_divide {
            flags |= SW_DE;
        }
        // Operand-class exceptions and NaN results are decided on the 64-bit format.
        let probe = match op {
            Arith::Add => a.add_r(b, rm),
            Arith::Sub => a.sub_r(b, rm),
            Arith::Mul => a.mul_r(b, rm),
            Arith::Div => a.div_r(b, rm),
        };
        flags |= status_bits(probe.status) & (SW_IE | SW_ZE);
        self.raise(flags);
        if probe.value.is_nan() {
            self.clear_c1();
            return self.nan_result(a, b);
        }
        let _ = pc;
        let fix_zero = |r: Ext| -> Ext {
            if !r.is_zero() || !matches!(op, Arith::Add | Arith::Sub) {
                return r;
            }
            let sb = if op == Arith::Sub { !b.is_negative() } else { b.is_negative() };
            let neg = if a.is_zero() && b.is_zero() {
                if a.is_negative() == sb { a.is_negative() } else { rm == Round::TowardNegative }
            } else {
                rm == Round::TowardNegative
            };
            if neg { -r.abs() } else { r.abs() }
        };
        let (wa, wb) = (to_wide(a), to_wide(b));
        let (w, st) = wide_sticky(op, wa, wb);
        let pre = status_bits(st) & SW_PE;
        let v = match pc {
            0 => {
                let (r, f) = narrow_wide::<X87SingleS>(w, rm);
                self.raise((f | pre) & !(SW_IE | SW_ZE));
                let v = widen(r);
                self.set_c1_rounded(v, w, f | pre);
                self.bias_unmasked(v, f | pre)
            }
            2 => {
                let (r, f) = narrow_wide::<X87DoubleS>(w, rm);
                self.raise((f | pre) & !(SW_IE | SW_ZE));
                let v = widen(r);
                self.set_c1_rounded(v, w, f | pre);
                self.bias_unmasked(v, f | pre)
            }
            _ => {
                let (r, f) = narrow_wide::<rustc_apfloat::ieee::X87DoubleExtendedS>(w, rm);
                self.raise((f | pre) & !(SW_IE | SW_ZE));
                self.set_c1_rounded(r, w, f | pre);
                self.bias_unmasked(r, f | pre)
            }
        };
        fix_zero(v)
    }

    /// x87 NaN propagation: the NaN with the larger significand wins; SNaNs are quieted.
    /// Equal significands (measured on hardware): the positive NaN wins.
    fn nan_result(&self, a: Ext, b: Ext) -> Ext {
        let quiet = |v: Ext| ext_from_bits(v.to_bits() | (1u128 << 62));
        match (a.is_nan(), b.is_nan()) {
            (true, true) => {
                let (ma, mb) = (a.to_bits() & ((1u128 << 63) - 1), b.to_bits() & ((1u128 << 63) - 1));
                if ma > mb {
                    quiet(a)
                } else if mb > ma {
                    quiet(b)
                } else if a.is_negative() && !b.is_negative() {
                    quiet(b)
                } else {
                    quiet(a)
                }
            }
            (true, false) => quiet(a),
            (false, true) => quiet(b),
            _ => Self::indefinite(),
        }
    }

    // ---- memory formats -------------------------------------------------------

    fn load_mem(&mut self, m: &mut dyn Mem, addr: u32, size: MemorySize) -> Option<Ext> {
        let mut buf = [0u8; 16];
        match size {
            MemorySize::Float32 => {
                m.read(addr, &mut buf[..4]);
                let s = Single::from_bits(u32::from_le_bytes(buf[..4].try_into().unwrap()) as u128);
                if s.is_denormal() { self.mem_denormal = true; }
                if s.is_signaling() { self.raise(SW_IE); return Some(widen(quiet_single(s))); }
                Some(widen(s))
            }
            MemorySize::Float64 => {
                m.read(addr, &mut buf[..8]);
                let d = Double::from_bits(u64::from_le_bytes(buf[..8].try_into().unwrap()) as u128);
                if d.is_denormal() { self.mem_denormal = true; }
                if d.is_signaling() { self.raise(SW_IE); return Some(widen(quiet_double(d))); }
                Some(widen(d))
            }
            MemorySize::Float80 => {
                m.read(addr, &mut buf[..10]);
                let bits = u128::from_le_bytes(buf) & ((1u128 << 80) - 1);
                Some(ext_from_bits(bits))
            }
            MemorySize::Int16 => {
                m.read(addr, &mut buf[..2]);
                Some(Ext::from_i128(i16::from_le_bytes(buf[..2].try_into().unwrap()) as i128).value)
            }
            MemorySize::Int32 => {
                m.read(addr, &mut buf[..4]);
                Some(Ext::from_i128(i32::from_le_bytes(buf[..4].try_into().unwrap()) as i128).value)
            }
            MemorySize::Int64 => {
                m.read(addr, &mut buf[..8]);
                Some(Ext::from_i128(i64::from_le_bytes(buf[..8].try_into().unwrap()) as i128).value)
            }
            MemorySize::Bcd => {
                m.read(addr, &mut buf[..10]);
                let mut v: i128 = 0;
                for i in (0..9).rev() {
                    v = v * 100 + ((buf[i] >> 4) as i128) * 10 + (buf[i] & 15) as i128;
                }
                if buf[9] & 0x80 != 0 { v = -v; }
                Some(Ext::from_i128(v).value)
            }
            _ => None,
        }
    }

    fn store_float(&mut self, m: &mut dyn Mem, addr: u32, size: MemorySize, v: Ext) -> bool {
        let rm = round_mode(self.cw);
        match size {
            MemorySize::Float32 => {
                let (s, f) = narrow::<rustc_apfloat::ieee::SingleS>(v, rm);
                let s = if v.is_signaling() { self.raise(SW_IE); quiet_single(s) } else { s };
                self.raise(f);
                self.c1_from_store(widen(s), v, f);
                if !self.aborted { m.write(addr, &(s.to_bits() as u32).to_le_bytes()); }
            }
            MemorySize::Float64 => {
                let (d, f) = narrow::<rustc_apfloat::ieee::DoubleS>(v, rm);
                let d = if v.is_signaling() { self.raise(SW_IE); quiet_double(d) } else { d };
                self.raise(f);
                self.c1_from_store(widen(d), v, f);
                if !self.aborted { m.write(addr, &(d.to_bits() as u64).to_le_bytes()); }
            }
            MemorySize::Float80 => {
                self.clear_c1();
                if !self.aborted { m.write(addr, &v.to_bits().to_le_bytes()[..10]); }
            }
            _ => return false,
        }
        true
    }

    fn c1_from_store(&mut self, stored: Ext, v: Ext, flags: u16) {
        // C1 after fst: 1 if the stored magnitude was rounded up (overflow to infinity included).
        self.clear_c1();
        if flags & SW_PE != 0 && (stored.abs() > v.abs() || stored.is_infinite()) {
            self.sw |= SW_C1;
        }
    }

    /// Integer stores: rounding per RC (or truncation for fisttp); out-of-range gives the
    /// integer indefinite value and IE.
    fn store_int(&mut self, m: &mut dyn Mem, addr: u32, size: MemorySize, v: Ext, truncate: bool) -> bool {
        let bits = match size { MemorySize::Int16 => 16, MemorySize::Int32 => 32, MemorySize::Int64 => 64, _ => return false };
        let rm = if truncate { Round::TowardZero } else { round_mode(self.cw) };
        let mut exact = false;
        let r = v.to_i128_r(bits, rm, &mut exact);
        let (val, flags) = if v.is_nan() || r.status.contains(Status::INVALID_OP) {
            (1i128 << (bits - 1), SW_IE) // indefinite: 0x8000...
        } else {
            (r.value, if exact { 0 } else { SW_PE })
        };
        self.raise(flags);
        self.clear_c1();
        if flags & SW_PE != 0 {
            // C1 = 1 if the magnitude was rounded up
            let back = Ext::from_i128(val).value;
            if back.abs() > v.abs() { self.sw |= SW_C1; }
        }
        if !self.aborted {
            match bits {
                16 => m.write(addr, &(val as i16).to_le_bytes()),
                32 => m.write(addr, &(val as i32).to_le_bytes()),
                _ => m.write(addr, &(val as i64).to_le_bytes()),
            }
        }
        true
    }

    fn store_bcd(&mut self, m: &mut dyn Mem, addr: u32, v: Ext) {
        self.clear_c1();
        let rm = round_mode(self.cw);
        let mut exact = false;
        let r = v.to_i128_r(80, rm, &mut exact);
        let mut out = [0u8; 10];
        if v.is_nan() || r.status.contains(Status::INVALID_OP) || r.value.abs() > 999_999_999_999_999_999 {
            self.raise(SW_IE);
            out = [0, 0, 0, 0, 0, 0, 0, 0xC0, 0xFF, 0xFF];
        } else {
            self.clear_c1();
            if !exact {
                self.raise(SW_PE);
                if Ext::from_i128(r.value).value.abs() > v.abs() { self.sw |= SW_C1; }
            }
            let mut n = r.value.abs();
            for b in out.iter_mut().take(9) {
                *b = ((n % 10) as u8) | ((((n / 10) % 10) as u8) << 4);
                n /= 100;
            }
            if v.is_negative() { out[9] = 0x80; }
        }
        if !self.aborted { m.write(addr, &out); }
    }

    // ---- compare ----------------------------------------------------------------

    /// Sets C0/C2/C3 from a comparison; `signaling` compares raise IE on any NaN,
    /// unordered ones only on SNaN.
    fn compare(&mut self, a: Ext, b: Ext, signaling: bool) -> Option<Ordering> {
        if a.is_nan() || b.is_nan() {
            if signaling || a.is_signaling() || b.is_signaling() {
                self.raise(SW_IE);
            }
            self.set_cc(true, false, true, true);
            return None;
        }
        if a.is_denormal() || b.is_denormal() || self.mem_denormal || self.reg_denormal {
            self.raise(SW_DE);
        }
        let o = a.partial_cmp(&b).unwrap_or(Ordering::Equal);
        match o {
            Ordering::Greater => self.set_cc(false, false, false, false),
            Ordering::Less => self.set_cc(true, false, false, false),
            Ordering::Equal => self.set_cc(false, false, false, true),
        }
        Some(o)
    }

    /// EFLAGS bits (CF=1, PF=4, ZF=0x40) for fcomi-style compares.
    fn compare_eflags(&mut self, a: Ext, b: Ext, signaling: bool) -> i32 {
        // fcomi/fucomi leave C0/C2/C3 alone but clear C1.
        let saved = self.sw & (SW_C0 | SW_C1 | SW_C2 | SW_C3);
        let o = self.compare(a, b, signaling);
        self.sw = (self.sw & !(SW_C0 | SW_C1 | SW_C2 | SW_C3)) | saved;
        match o {
            None => 0x45,
            Some(Ordering::Less) => 1,
            Some(Ordering::Equal) => 0x40,
            Some(Ordering::Greater) => 0,
        }
    }

    // ---- environment images --------------------------------------------------

    /// 28-byte protected-mode environment (fnstenv / fnsave prefix). The upper halves of
    /// the control/status/tag and data-selector dwords read as ones on real hardware.
    pub fn env_image(&self) -> [u8; 28] {
        let mut e = [0u8; 28];
        e[0..2].copy_from_slice(&self.cw.to_le_bytes());
        e[2..4].fill(0xff);
        e[4..6].copy_from_slice(&self.status_word().to_le_bytes());
        e[6..8].fill(0xff);
        e[8..10].copy_from_slice(&self.tag_word().to_le_bytes());
        e[10..12].fill(0xff);
        e[12..16].copy_from_slice(&self.fip.to_le_bytes());
        e[16..18].copy_from_slice(&0u16.to_le_bytes()); // FCS: deprecated, reads as 0
        e[18..20].copy_from_slice(&self.fop.to_le_bytes());
        e[20..24].copy_from_slice(&self.fdp.to_le_bytes());
        e[24..26].copy_from_slice(&0u16.to_le_bytes()); // FDS: deprecated, reads as 0
        e[26..28].fill(0xff);
        e
    }

    pub fn load_env_image(&mut self, e: &[u8]) {
        self.cw = (u16::from_le_bytes([e[0], e[1]]) & 0x1f7f) | 0x40;
        self.set_status_word(u16::from_le_bytes([e[4], e[5]]));
        self.set_tag_word(u16::from_le_bytes([e[8], e[9]]));
        self.refresh_es();
    }

    /// FXSAVE layout (first 160 bytes: x87 part), tags abridged, ST(i) in stack order.
    pub fn write_fxsave(&self, fx: &mut [u8]) {
        fx[0..2].copy_from_slice(&self.cw.to_le_bytes());
        fx[2..4].copy_from_slice(&self.status_word().to_le_bytes());
        let mut abridged = 0u8;
        for i in 0..8 {
            if self.tags[i] != 3 {
                abridged |= 1 << i;
            }
        }
        fx[4] = abridged;
        fx[5] = 0;
        fx[6..8].copy_from_slice(&self.fop.to_le_bytes());
        fx[8..12].copy_from_slice(&self.fip.to_le_bytes());
        fx[12..16].copy_from_slice(&0u32.to_le_bytes());
        fx[16..20].copy_from_slice(&self.fdp.to_le_bytes());
        fx[20..24].copy_from_slice(&0u32.to_le_bytes());
        for i in 0..8u32 {
            let bits = self.st_bits(i).to_le_bytes();
            fx[32 + 16 * i as usize..42 + 16 * i as usize].copy_from_slice(&bits[..10]);
            fx[42 + 16 * i as usize..48 + 16 * i as usize].fill(0);
        }
    }

    pub fn read_fxsave(&mut self, fx: &[u8]) {
        self.cw = (u16::from_le_bytes([fx[0], fx[1]]) & 0x1f7f) | 0x40;
        self.set_status_word(u16::from_le_bytes([fx[2], fx[3]]));
        let abridged = fx[4];
        for i in 0..8u32 {
            let mut b = [0u8; 16];
            b[..10].copy_from_slice(&fx[32 + 16 * i as usize..42 + 16 * i as usize]);
            let phys = self.phys(i);
            let empty = abridged & (1 << phys) == 0;
            self.set_st_raw(i, u128::from_le_bytes(b), empty);
        }
        self.refresh_es();
    }

    // ---- the instruction dispatcher --------------------------------------------

    /// Executes one x87 instruction. `a` is the memory address or ST index of operand 0;
    /// `b` is packed per `args`. Returns the value some instructions hand back to the
    /// integer side (fnstsw ax: the status word; fcomi family: EFLAGS bits).
    pub fn exec(&mut self, mn: Mnemonic, a: u32, b: u32, ip: u32, fop: u16, m: &mut dyn Mem) -> i32 {
        let snapshot = (self.regs, self.tags, self.top);
        self.aborted = false;
        let r = self.exec_inner(mn, a, b, ip, fop, m);
        if self.aborted {
            (self.regs, self.tags, self.top) = snapshot;
        }
        r
    }

    fn exec_inner(&mut self, mn: Mnemonic, a: u32, b: u32, ip: u32, fop: u16, m: &mut dyn Mem) -> i32 {
        use Mnemonic::*;
        let memsize: MemorySize = unsafe { std::mem::transmute(args::memsize(b) as u8) };
        let has_mem = args::memsize(b) != 0;
        self.fault_operand = false;
        self.mem_denormal = false;
        self.reg_denormal = false;
        self.mem_source = has_mem;
        // Control instructions (fnstcw, fldcw, fnstsw, fnstenv, fldenv, fnsave, frstor, fninit,
        // fnclex, fnop, fincstp, fdecstp, ffree, fwait) leave the pointers alone.
        // Current CPUs update FOP and FDP only when an unmasked exception is raised
        // (FCS/FDS are gone entirely); FIP tracks every non-control instruction.
        if !matches!(mn, Wait | Fninit | Fnclex | Fldcw | Fnstcw | Fnstsw | Fnstenv | Fldenv | Fnsave | Frstor | Fincstp | Fdecstp | Ffree | Ffreep) {
            self.fip = ip;
            self.cur_fop = fop & 0x7ff;
            self.cur_fdp = if has_mem { a } else { 0 };
        }
        let op0_st = b & args::OP0_IS_ST != 0;
        let op1_st = b & args::OP1_IS_ST != 0;
        let st1 = b & 7;
        // "the other register" of a two-operand st0,st(i) form, or st(1) for the implicit forms
        let other = if op1_st { st1 } else if op0_st { a } else { 1 };
        // Pending unmasked exception: the next waiting instruction would raise #MF.
        match mn {
            Fnop | Wait => return 0,
            Fninit => { self.reinit(); return 0; }
            Fnclex => { self.sw &= !(0x80ff); self.unmasked_pending = false; return 0; }
            Fldcw => {
                let mut buf = [0u8; 2];
                m.read(a, &mut buf);
                self.set_control_word(u16::from_le_bytes(buf));
                return 0;
            }
            Fnstcw => { m.write(a, &self.cw.to_le_bytes()); return 0; }
            Fnstsw => {
                let sw = self.status_word();
                if has_mem { m.write(a, &sw.to_le_bytes()); }
                return sw as i32;
            }
            Fnstenv => {
                let e = self.env_image();
                if memsize == MemorySize::FpuEnv14 { m.write(a, &env14(&e)); } else { m.write(a, &e); }
                // fnstenv masks all exceptions afterwards
                self.cw |= 0x3f;
                self.refresh_es();
                return 0;
            }
            Fldenv => {
                let mut e = [0u8; 28];
                if memsize == MemorySize::FpuEnv14 {
                    let mut s = [0u8; 14];
                    m.read(a, &mut s);
                    e = env_from14(&s);
                } else {
                    m.read(a, &mut e);
                }
                self.load_env_image(&e);
                return 0;
            }
            Fnsave => {
                let e = self.env_image();
                let mut img = Vec::new();
                if memsize == MemorySize::FpuState94 { img.extend_from_slice(&env14(&e)); } else { img.extend_from_slice(&e); }
                for i in 0..8u32 {
                    img.extend_from_slice(&self.st_bits(i).to_le_bytes()[..10]);
                }
                m.write(a, &img);
                self.reinit();
                return 0;
            }
            Frstor => {
                let env_len = if memsize == MemorySize::FpuState94 { 14 } else { 28 };
                let mut img = vec![0u8; env_len + 80];
                m.read(a, &mut img);
                let e = if env_len == 14 { env_from14(&img[..14]) } else { img[..28].try_into().unwrap() };
                self.load_env_image(&e);
                for i in 0..8u32 {
                    let mut bb = [0u8; 16];
                    bb[..10].copy_from_slice(&img[env_len + 10 * i as usize..env_len + 10 * i as usize + 10]);
                    let p = self.phys(i);
                    self.regs[p] = u128::from_le_bytes(bb) & ((1u128 << 80) - 1);
                }
                return 0;
            }
            Fincstp => { self.top = (self.top + 1) & 7; self.clear_c1(); return 0; }
            Fdecstp => { self.top = self.top.wrapping_sub(1) & 7; self.clear_c1(); return 0; }
            Ffree => { let p = self.phys(a); self.tags[p] = 3; self.clear_c1(); return 0; }
            Ffreep => { let p = self.phys(a); self.tags[p] = 3; self.pop(); self.clear_c1(); return 0; }
            _ => {}
        }

        match mn {
            Fld if !has_mem && self.is_empty(a) => {
                // underflow on the source takes precedence over a full stack: C1 stays 0
                self.raise(SW_IE | SW_SF);
                self.clear_c1();
                self.top = self.top.wrapping_sub(1) & 7;
                if self.cw & 1 != 0 {
                    let p = self.top as usize;
                    self.regs[p] = Self::INDEFINITE_BITS;
                    self.tags[p] = 2;
                }
            }
            Fld | Fild | Fbld | Fld1 | Fldl2t | Fldl2e | Fldpi | Fldlg2 | Fldln2 | Fldz if self.push_would_overflow() => {
                self.raise(SW_IE | SW_SF);
                self.top = self.top.wrapping_sub(1) & 7;
                self.sw |= SW_C1;
                if self.cw & 1 != 0 {
                    let p = self.top as usize;
                    self.regs[p] = Self::INDEFINITE_BITS;
                    self.tags[p] = 2;
                }
            }
            Fld => {
                let v = if has_mem {
                    if memsize == MemorySize::Float80 {
                        let mut buf = [0u8; 16];
                        m.read(a, &mut buf[..10]);
                        self.clear_c1();
                        self.push_raw(u128::from_le_bytes(buf));
                        return 0;
                    }
                    let Some(v) = self.load_mem(m, a, memsize) else { return 0 };
                    if self.mem_denormal { self.raise(SW_DE); }
                    v
                } else {
                    // an empty source keeps its raw bits readable: fld st(i) copies them
                    if self.is_empty(a) {
                        self.raise(SW_IE | SW_SF);
                        self.clear_c1();
                        self.push(Self::indefinite());
                        return 0;
                    }
                    let bits = self.st_bits(a);
                    self.clear_c1();
                    self.push_raw(bits);
                    return 0;
                };
                self.clear_c1();
                self.push(v);
            }
            Fild => {
                if let Some(v) = self.load_mem(m, a, memsize) {
                    self.clear_c1();
                    self.push(v);
                }
            }
            Fbld => {
                if let Some(v) = self.load_mem(m, a, MemorySize::Bcd) {
                    self.clear_c1();
                    self.push(v);
                }
            }
            Fld1 | Fldl2t | Fldl2e | Fldpi | Fldlg2 | Fldln2 | Fldz => {
                // Constants are kept to 66 bits internally and rounded by RC only (PC is ignored).
                let text = match mn {
                    Fld1 => "1",
                    Fldl2t => "3.32192809488736234787031942948939017586483139302458061205",
                    Fldl2e => "1.44269504088896340735992468100189213742664595415298593413",
                    Fldpi => "3.14159265358979323846264338327950288419716939937510582097",
                    Fldlg2 => "0.30102999566398119521373889472449302676818988146210854131",
                    Fldln2 => "0.69314718055994530941723212145817656807550013436025525412",
                    _ => "0",
                };
                let w: Wide = text.parse().unwrap();
                let v = widen_wide_to_ext(w, round_mode(self.cw));
                self.clear_c1();
                self.push(v);
            }
            Fst | Fstp => {
                if has_mem {
                    let v = if memsize == MemorySize::Float80 && !self.is_empty(0) {
                        // 80-bit stores copy the raw image, unsupported encodings included
                        let bits = self.st_bits(0);
                        self.clear_c1();
                        if !self.aborted { m.write(a, &bits.to_le_bytes()[..10]); }
                        if mn == Fstp { self.pop(); }
                        return 0;
                    } else {
                        self.operand_or_indef(0)
                    };
                    self.store_float(m, a, memsize, v);
                } else {
                    let v = if self.is_empty(0) { self.operand_or_indef(0) } else { Ext::ZERO };
                    if self.is_empty(0) {
                        self.set_st(a, v);
                    } else {
                        let bits = self.st_bits(0);
                        self.clear_c1();
                        self.set_st_raw(a, bits, false);
                    }
                }
                if mn == Fstp { self.pop(); }
            }
            Fist | Fistp | Fisttp => {
                let v = self.operand_or_indef(0);
                self.store_int(m, a, memsize, v, mn == Fisttp);
                if mn != Fist { self.pop(); }
            }
            Fbstp => {
                let v = self.operand_or_indef(0);
                self.store_bcd(m, a, v);
                self.pop();
            }
            Fxch => {
                let i = other;
                let (e0, ei) = (self.is_empty(0), self.is_empty(i));
                let (x, y) = (self.st_bits(0), self.st_bits(i));
                if e0 || ei {
                    self.raise(SW_IE | SW_SF);
                }
                let (x, y) = (if e0 { Self::indefinite().to_bits() } else { x }, if ei { Self::indefinite().to_bits() } else { y });
                self.set_st_raw(0, y, false);
                self.set_st_raw(i, x, false);
                self.clear_c1();
            }
            Fadd | Fsub | Fsubr | Fmul | Fdiv | Fdivr | Faddp | Fsubp | Fsubrp | Fmulp | Fdivp | Fdivrp | Fiadd | Fisub
            | Fisubr | Fimul | Fidiv | Fidivr => {
                let op = match mn {
                    Fadd | Faddp | Fiadd => Arith::Add,
                    Fsub | Fsubp | Fisub | Fsubr | Fsubrp | Fisubr => Arith::Sub,
                    Fmul | Fmulp | Fimul => Arith::Mul,
                    _ => Arith::Div,
                };
                let reversed = matches!(mn, Fsubr | Fsubrp | Fisubr | Fdivr | Fdivrp | Fidivr);
                let pop = matches!(mn, Faddp | Fsubp | Fsubrp | Fmulp | Fdivp | Fdivrp);
                // Destination: ST(0) for memory forms and `st0, sti`; ST(i) for `sti, st0` and the p forms.
                let (dst, src_val) = if has_mem {
                    if self.is_empty(0) {
                        // stack underflow is detected before the memory operand is examined
                        let v = self.operand_or_indef(0);
                        self.set_st_raw(0, v.to_bits(), false);
                        return 0;
                    }
                    let Some(v) = self.load_mem(m, a, memsize) else { return 0 };
                    (0u32, Some(v))
                } else if a == 0 && !pop {
                    (0u32, Some(self.operand_or_indef(st1)))
                } else {
                    (a, Some(self.operand_or_indef(0)))
                };
                let Some(src) = src_val else { return 0 };
                let d = self.operand_or_indef(dst);
                let (x, y) = if reversed { (src, d) } else { (d, src) };
                let r = self.arith(op, x, y);
                self.set_st(dst, r);
                if pop { self.pop(); }
            }
            Fcom | Fcomp | Fcompp | Fucom | Fucomp | Fucompp | Ficom | Ficomp => {
                let signaling = matches!(mn, Fcom | Fcomp | Fcompp | Ficom | Ficomp);
                let pops = match mn { Fcompp | Fucompp => 2, Fcomp | Fucomp | Ficomp => 1, _ => 0 };
                let other = if has_mem {
                    if self.is_empty(0) { None } else { self.load_mem(m, a, memsize) }
                } else if matches!(mn, Fcompp | Fucompp) {
                    self.operand(1)
                } else {
                    self.operand(other)
                };
                let s0 = self.operand(0);
                match (s0, other) {
                    (Some(x), Some(y)) => { self.compare(x, y, signaling); }
                    _ => self.set_cc(true, false, true, true),
                }
                self.clear_c1();
                for _ in 0..pops { self.pop(); }
            }
            Fcomi | Fcomip | Fucomi | Fucomip => {
                let signaling = matches!(mn, Fcomi | Fcomip);
                let (x, y) = (self.operand(0), self.operand(other));
                let r = match (x, y) {
                    (Some(x), Some(y)) => self.compare_eflags(x, y, signaling),
                    _ => 0x45,
                };
                if matches!(mn, Fcomip | Fucomip) { self.pop(); }
                return r;
            }
            Ftst => {
                match self.operand(0) {
                    Some(x) => { self.compare(x, Ext::ZERO, true); }
                    None => self.set_cc(true, false, true, true),
                }
                self.clear_c1();
            }
            Fxam => {
                let empty = self.is_empty(0);
                let v = self.st(0);
                let sign = v.is_negative();
                let (c0, c2, c3) = if empty {
                    (true, false, true)
                } else if v.is_nan() {
                    (true, false, false)
                } else if v.is_infinite() {
                    (true, true, false)
                } else if v.is_zero() {
                    (false, false, true)
                } else if v.is_denormal() || is_unnormal(v) {
                    (false, true, true)
                } else {
                    (false, true, false)
                };
                self.set_cc(c0, sign, c2, c3);
            }
            Fchs | Fabs => {
                // sign changes act on the raw image, even for unsupported encodings;
                // an empty register underflows and becomes the indefinite QNaN
                if self.is_empty(0) {
                    self.raise(SW_IE | SW_SF);
                    self.set_st_raw(0, Self::INDEFINITE_BITS, false);
                } else {
                    let p = self.phys(0);
                    let bits = self.regs[p];
                    self.regs[p] = if mn == Fchs { bits ^ (1u128 << 79) } else { bits & !(1u128 << 79) };
                }
                self.clear_c1();
            }
            Fsqrt => {
                let v = self.operand_or_indef(0);
                let r = self.sqrt(v);
                self.set_st(0, r);
            }
            Frndint => {
                let v = self.operand_or_indef(0);
                if v.is_nan() {
                    let q = if self.fault_operand { Self::indefinite() } else { self.nan_result(v, v) };
                    self.clear_c1();
                    self.set_st(0, q);
                } else {
                    if v.is_denormal() { self.raise(SW_DE); }
                    let r = v.round_to_integral(round_mode(self.cw));
                    let flags = status_bits(r.status);
                    self.raise(flags);
                    self.clear_c1();
                    if flags & SW_PE != 0 && r.value.abs() > v.abs() { self.sw |= SW_C1; }
                    self.set_st(0, r.value);
                }
            }
            Fscale => {
                let (x, y) = (self.operand_or_indef(0), self.operand_or_indef(1));
                let r = self.scale(x, y);
                self.set_st(0, r);
            }
            Fxtract => {
                if self.tags[self.top.wrapping_sub(1) as usize & 7] != 3 {
                    // stack overflow: both results are indefinite
                    self.raise(SW_IE | SW_SF);
                    self.set_st_raw(0, Self::INDEFINITE_BITS, false);
                    self.push_raw(Self::INDEFINITE_BITS);
                    return 0;
                }
                let x = self.operand_or_indef(0);
                {
                    if x.is_denormal() { self.raise(SW_DE); }
                    if x.is_zero() {
                        self.raise(SW_ZE);
                        self.set_st(0, -Ext::INFINITY);
                        self.push(x);
                    } else if x.is_infinite() {
                        self.set_st(0, Ext::INFINITY);
                        self.push(x);
                    } else if x.is_nan() {
                        self.set_st(0, x);
                        self.push(x);
                    } else {
                        let e = x.ilogb();
                        let sig = x.scalbn(-e);
                        self.set_st(0, Ext::from_i128(e as i128).value);
                        self.push(sig);
                    }
                    self.clear_c1();
                }
            }
            Fprem | Fprem1 => {
                let (x, y) = (self.operand_or_indef(0), self.operand_or_indef(1));
                if x.is_denormal() || y.is_denormal() { self.raise(SW_DE); }
                let r = self.prem(x, y, mn == Fprem1);
                self.set_st(0, r);
            }
            F2xm1 | Fyl2x | Fyl2xp1 | Fsin | Fcos | Fsincos | Fptan | Fpatan => self.transcendental(mn),
            Fcmovb | Fcmove | Fcmovbe | Fcmovu | Fcmovnb | Fcmovne | Fcmovnbe | Fcmovnu => {
                // The condition was evaluated by the integer side: `b` bit 24 says "move".
                // An empty source or destination is a stack underflow whatever the condition.
                if self.is_empty(0) || self.is_empty(other) {
                    self.raise(SW_IE | SW_SF);
                    self.clear_c1();
                    if self.cw & 1 != 0 {
                        self.set_st(0, Self::indefinite());
                    }
                } else if b & (1 << 24) != 0 {
                    let bits = self.st_bits(other);
                    self.set_st_raw(0, bits, false);
                }
            }
            Femms | Emms => {
                for t in self.tags.iter_mut() { *t = 3; }
            }
            _ => return -1,
        }
        0
    }

    /// Constants are rounded to the current precision (fldpi etc. are 66-bit exact values).
    fn round_constant(&mut self, v: Ext) -> Ext {
        match (self.cw >> 8) & 3 {
            0 => widen(narrow::<X87SingleS>(v, round_mode(self.cw)).0),
            2 => widen(narrow::<X87DoubleS>(v, round_mode(self.cw)).0),
            _ => v,
        }
    }

    fn sqrt(&mut self, v: Ext) -> Ext {
        if self.fault_operand {
            self.clear_c1();
            return Self::indefinite();
        }
        if v.is_nan() {
            if v.is_signaling() { self.raise(SW_IE); }
            self.clear_c1();
            return self.nan_result(v, v);
        }
        if v.is_zero() {
            self.clear_c1();
            return v;
        }
        if v.is_negative() {
            self.raise(SW_IE);
            self.clear_c1();
            return Self::indefinite();
        }
        if v.is_infinite() {
            self.clear_c1();
            return v;
        }
        if v.is_denormal() { self.raise(SW_DE); }
        // v = m * 2^e with m a 64-bit integer significand; make the exponent even and
        // compute a 70-bit root with a sticky bit, which rounds correctly to <= 64 bits.
        let e = v.ilogb();
        let m64 = (v.to_bits() & ((1u128 << 64) - 1)) as u128; // explicit integer bit included
        let mut e2 = e - 63; // v = m64 * 2^(e-63)
        let mut m = m64;
        if e2 & 1 != 0 {
            m <<= 1;
            e2 -= 1;
        }
        // root of m << 76 gives 70 bits (m has 64-65 bits)
        let shift = 76u32;
        let (root, rem) = isqrt_u128_with_rem(m, shift);
        let root = if rem { root | 1 } else { root };
        let w = Wide::from_u128(root).value.scalbn(e2 / 2 - (shift as i32) / 2);
        self.round_result(w, 0)
    }

    fn scale(&mut self, x: Ext, y: Ext) -> Ext {
        if self.fault_operand {
            self.clear_c1();
            return Self::indefinite();
        }
        if x.is_nan() || y.is_nan() {
            if x.is_signaling() || y.is_signaling() { self.raise(SW_IE); }
            self.clear_c1();
            return self.nan_result(x, y);
        }
        if y.is_infinite() {
            if x.is_zero() && y.is_negative() { self.clear_c1(); return x; }
            if x.is_zero() || (x.is_infinite() && y.is_negative()) { self.raise(SW_IE); self.clear_c1(); return Self::indefinite(); }
            self.clear_c1();
            let mag = if y.is_negative() { Ext::ZERO } else { Ext::INFINITY };
            return if x.is_negative() { -mag } else { mag };
        }
        if x.is_zero() || x.is_infinite() {
            self.clear_c1();
            return x;
        }
        if x.is_denormal() || y.is_denormal() { self.raise(SW_DE); }
        let mut exact = false;
        let n = if y.abs() >= Ext::from_i128(1 << 20).value {
            if y.is_negative() { -70000 } else { 70000 }
        } else {
            y.to_i128_r(64, Round::TowardZero, &mut exact).value as i32
        };
        // Scaling by a power of two is exact unless the exponent leaves the range; the
        // precision control does not apply.
        let rm = round_mode(self.cw);
        let r = x.scalbn_r(n, rm);
        let mut flags = 0;
        self.clear_c1();
        let e_new = x.ilogb() as i64 + n as i64;
        if e_new > 16383 {
            flags |= SW_OE | SW_PE;
            if r.is_infinite() { self.sw |= SW_C1; }
        } else if r.scalbn(-n) != x {
            // lost bits while denormalizing
            flags |= SW_UE | SW_PE;
            let exact = to_wide(x).scalbn(n);
            if to_wide(r).abs() > exact.abs() { self.sw |= SW_C1; }
        }
        self.raise(flags);
        r
    }

    fn prem(&mut self, x: Ext, y: Ext, ieee: bool) -> Ext {
        if self.fault_operand {
            self.sw &= !(SW_C1 | SW_C2);
            return Self::indefinite();
        }
        if x.is_nan() || y.is_nan() || x.is_infinite() || y.is_zero() {
            self.sw &= !(SW_C1 | SW_C2);
            if !(x.is_nan() || y.is_nan()) { self.raise(SW_IE); return Self::indefinite(); }
            self.raise(if x.is_signaling() || y.is_signaling() { SW_IE } else { 0 });
            return self.nan_result(x, y);
        }
        if y.is_infinite() || x.is_zero() {
            self.set_cc(false, false, false, false);
            return x;
        }
        // Partial remainder: at most 64 quotient bits per pass; C2 = 1 when incomplete.
        let ex = x.ilogb();
        let ey = y.ilogb();
        let diff = ex - ey;
        if diff < 64 {
            let r = if ieee { x.ieee_rem(y) } else { x.c_fmod(y) };
            let r = r.value;
            // quotient low 3 bits -> C0 (bit 2), C3 (bit 1), C1 (bit 0); exact via (x - r) / y
            let q = ((to_wide(x) - to_wide(r)).value / to_wide(y)).value;
            let mut exact = false;
            let qi = q.round_to_integral(Round::NearestTiesToEven).value.to_i128_r(128, Round::TowardZero, &mut exact).value.unsigned_abs();
            self.set_cc(qi & 4 != 0, qi & 1 != 0, false, qi & 2 != 0);
            r
        } else {
            // Partial remainder: the hardware produces N = 32 + (D mod 32) quotient bits per
            // pass, i.e. it reduces by y * 2^(32 * (D/32 - 1)) and sets C2.
            let n = 32 * (diff / 32 - 1);
            let scaled_y = y.scalbn(n);
            let r = x.c_fmod(scaled_y).value;
            self.set_cc(false, false, true, false);
            r
        }
    }

    fn transcendental(&mut self, mn: Mnemonic) {
        use Mnemonic::*;
        let two_arg = matches!(mn, Fyl2x | Fyl2xp1 | Fpatan);
        if matches!(mn, Fsincos | Fptan) && self.push_would_overflow() {
            // stack overflow: both results are indefinite
            self.raise(SW_IE | SW_SF);
            self.set_st_raw(0, Self::INDEFINITE_BITS, false);
            self.push_raw(Self::INDEFINITE_BITS);
            return;
        }
        let x = self.operand_or_indef(0);
        let y = if two_arg { self.operand_or_indef(1) } else { Ext::ZERO };
        if self.fault_operand {
            // stack underflow: the destination becomes indefinite; stack motion still happens
            if two_arg {
                self.set_st_raw(1, Self::INDEFINITE_BITS, false);
                self.pop();
            } else {
                self.set_st_raw(0, Self::INDEFINITE_BITS, false);
                if matches!(mn, Fsincos | Fptan) {
                    self.push_raw(Self::INDEFINITE_BITS);
                }
            }
            return;
        }
        let xf = to_f64(x);
        let yf = to_f64(y);
        if x.is_signaling() || (two_arg && y.is_signaling()) { self.raise(SW_IE); }
        self.sw &= !(SW_C2 | SW_C1);
        match mn {
            Fsin | Fcos | Fptan | Fsincos => {
                if x.is_infinite() {
                    self.raise(SW_IE);
                    self.set_st(0, Self::indefinite());
                    if mn == Fsincos || mn == Fptan { self.push(Self::indefinite()); }
                    return;
                }
                if !x.is_nan() && xf.abs() >= 9223372036854775808.0 {
                    self.sw |= SW_C2; // out of range: operand unchanged
                    return;
                }
                if x.is_nan() {
                    let q = self.nan_result(x, x);
                    self.set_st(0, q);
                    if mn == Fsincos || mn == Fptan { self.push(q); }
                    return;
                }
                let r = match mn {
                    Fsin => from_f64(libm::sin(xf)),
                    Fcos => from_f64(libm::cos(xf)),
                    _ => Ext::ZERO,
                };
                if !x.is_zero() { self.raise(SW_PE); }
                match mn {
                    Fsincos => {
                        let sn = self.round_constant_rm(from_f64(libm::sin(xf)));
                        self.set_st(0, sn);
                        let c = self.round_constant_rm(from_f64(libm::cos(xf)));
                        self.push(c);
                    }
                    Fptan => {
                        let t = self.round_constant_rm(from_f64(libm::tan(xf)));
                        self.set_st(0, t);
                        self.push(ext_from_bits(0x3FFF_8000_0000_0000_0000));
                    }
                    _ => {
                        let r = self.round_constant_rm(r);
                        self.set_st(0, r);
                    }
                }
            }
            F2xm1 => {
                let r = if x.is_nan() { self.nan_result(x, x) } else { from_f64(libm::pow(2.0, xf) - 1.0) };
                if !x.is_zero() && !x.is_nan() { self.raise(SW_PE); }
                let r = self.round_constant_rm(r);
                self.set_st(0, r);
            }
            Fyl2x | Fyl2xp1 => {
                // ST(1) = ST(1) * log2(ST(0) [+1]); pop
                let r = if x.is_nan() || y.is_nan() {
                    self.nan_result(x, y)
                } else {
                    let l = if mn == Fyl2x { libm::log2(xf) } else { libm::log(1.0 + xf) / std::f64::consts::LN_2 };
                    if mn == Fyl2x && xf < 0.0 || mn == Fyl2xp1 && xf < -1.0 { self.raise(SW_IE); Self::indefinite() }
                    else if mn == Fyl2x && xf == 0.0 { self.raise(SW_ZE); if yf == 0.0 { self.raise(SW_IE); Self::indefinite() } else { from_f64(if yf > 0.0 { f64::NEG_INFINITY } else { f64::INFINITY }) } }
                    else { from_f64(yf * l) }
                };
                if !r.is_nan() && !r.is_infinite() { self.raise(SW_PE); }
                let r = self.round_constant_rm(r);
                self.set_st(1, r);
                self.pop();
            }
            Fpatan => {
                // ST(1) = atan(ST(1) / ST(0)); pop
                let r = if x.is_nan() || y.is_nan() { self.nan_result(x, y) } else { from_f64(libm::atan2(yf, xf)) };
                if !r.is_nan() { self.raise(SW_PE); }
                let r = self.round_constant_rm(r);
                self.set_st(1, r);
                self.pop();
            }
            _ => {}
        }
    }

    /// Transcendental results are produced at full 64-bit precision regardless of PC.
    fn round_constant_rm(&mut self, v: Ext) -> Ext {
        v
    }
}

fn narrow_wide<T: Semantics>(w: Wide, rm: Round) -> (IeeeFloat<T>, u16) {
    let mut lost = false;
    let r = w.convert_r(rm, &mut lost);
    (r.value, status_bits(r.status))
}

fn to_wide(v: Ext) -> Wide {
    let mut lost = false;
    v.convert_r(Round::NearestTiesToEven, &mut lost).value
}

fn widen_wide_to_ext(w: Wide, rm: Round) -> Ext {
    let mut lost = false;
    w.convert_r(rm, &mut lost).value
}

fn to_f64(v: Ext) -> f64 {
    let mut lost = false;
    let d: Double = v.convert_r(Round::NearestTiesToEven, &mut lost).value;
    f64::from_bits(d.to_bits() as u64)
}

fn from_f64(f: f64) -> Ext {
    widen(Double::from_bits(f.to_bits() as u128))
}

fn quiet_single(s: Single) -> Single {
    Single::from_bits(s.to_bits() | (1 << 22))
}

fn quiet_double(d: Double) -> Double {
    Double::from_bits(d.to_bits() | (1 << 51))
}

/// Unnormal: explicit integer bit clear with a nonzero biased exponent (never produced
/// by valid arithmetic; treated as "special" by fxam).
fn is_unnormal(v: Ext) -> bool {
    let bits = v.to_bits();
    let exp = (bits >> 64) & 0x7fff;
    exp != 0 && exp != 0x7fff && (bits >> 63) & 1 == 0
}

/// floor(sqrt(m << shift)) and whether the remainder is nonzero.
fn isqrt_u128_with_rem(m: u128, shift: u32) -> (u128, bool) {
    // m < 2^66, shift = 76 -> m << 76 < 2^142: do it in two u128 halves via long division style
    // using binary search on the root (root < 2^71).
    // Represent n = m << shift as (hi, lo) 256-bit.
    let hi = if shift >= 128 { m << (shift - 128) } else { m >> (128 - shift) };
    let lo = if shift >= 128 { 0 } else { m << shift };
    // binary search root in [0, 2^72)
    let (mut low, mut high) = (0u128, 1u128 << 72);
    while low + 1 < high {
        let mid = (low + high) / 2;
        // mid^2 as 256-bit
        let (sh, sl) = mul_u128(mid, mid);
        if (sh, sl) <= (hi, lo) { low = mid } else { high = mid }
    }
    let (sh, sl) = mul_u128(low, low);
    (low, (sh, sl) != (hi, lo))
}

fn mul_u128(a: u128, b: u128) -> (u128, u128) {
    let (a0, a1) = (a & u64::MAX as u128, a >> 64);
    let (b0, b1) = (b & u64::MAX as u128, b >> 64);
    let p00 = a0 * b0;
    let p01 = a0 * b1;
    let p10 = a1 * b0;
    let p11 = a1 * b1;
    let mid = (p00 >> 64) + (p01 & u64::MAX as u128) + (p10 & u64::MAX as u128);
    let lo = (p00 & u64::MAX as u128) | (mid << 64);
    let hi = p11 + (p01 >> 64) + (p10 >> 64) + (mid >> 64);
    (hi, lo)
}

fn env14(e: &[u8; 28]) -> [u8; 14] {
    let mut s = [0u8; 14];
    s[0..2].copy_from_slice(&e[0..2]);
    s[2..4].copy_from_slice(&e[4..6]);
    s[4..6].copy_from_slice(&e[8..10]);
    s
}

fn env_from14(s: &[u8]) -> [u8; 28] {
    let mut e = [0u8; 28];
    e[0..2].copy_from_slice(&s[0..2]);
    e[4..6].copy_from_slice(&s[2..4]);
    e[8..10].copy_from_slice(&s[4..6]);
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoMem;
    impl Mem for NoMem {
        fn read(&mut self, _: u32, _: &mut [u8]) {}
        fn write(&mut self, _: u32, _: &[u8]) {}
    }

    fn ld(f: &mut Fpu, v: f64) {
        f.clear_c1();
        f.push(from_f64(v));
    }

    #[test]
    fn indefinite_keeps_sign() {
        let v = Fpu::indefinite();
        assert_eq!(v.to_bits(), Fpu::INDEFINITE_BITS, "apfloat must round-trip the x87 indefinite QNaN");
        let mut f = Fpu::new();
        f.set_st(0, v);
        f.top = 7;
        f.push(v);
        assert_eq!(f.st_bits(0), Fpu::INDEFINITE_BITS);
    }

    #[test]
    fn add_and_round_to_double() {
        let mut f = Fpu::new();
        f.set_control_word(0x027f); // PC = 53
        ld(&mut f, 1.0);
        ld(&mut f, 3.0);
        f.exec(Mnemonic::Fdivp, 1, 0, 0, 0, &mut NoMem); // st1 = st1 / st0; pop  -> 1/3
        let r = to_f64(f.st(0));
        assert_eq!(r, 1.0 / 3.0);
        assert!(f.status_word() & SW_PE != 0);
    }

    #[test]
    fn sqrt_exact_and_inexact() {
        let mut f = Fpu::new();
        ld(&mut f, 16.0);
        f.exec(Mnemonic::Fsqrt, 0, 0, 0, 0, &mut NoMem);
        assert_eq!(to_f64(f.st(0)), 4.0);
        assert_eq!(f.status_word() & SW_PE, 0);
        ld(&mut f, 2.0);
        f.exec(Mnemonic::Fsqrt, 0, 0, 0, 0, &mut NoMem);
        assert_eq!(f.st(0).to_bits(), 0x3FFF_B504_F333_F9DE_6484);
    }

    #[test]
    fn integer_store_rounding() {
        struct M(Vec<u8>);
        impl Mem for M {
            fn read(&mut self, a: u32, b: &mut [u8]) { b.copy_from_slice(&self.0[a as usize..a as usize + b.len()]); }
            fn write(&mut self, a: u32, d: &[u8]) { self.0[a as usize..a as usize + d.len()].copy_from_slice(d); }
        }
        let mut m = M(vec![0; 64]);
        let mut f = Fpu::new();
        ld(&mut f, 2.5);
        f.exec(Mnemonic::Fistp, 0, (MemorySize::Int32 as u32) << 8, 0, 0, &mut m);
        assert_eq!(i32::from_le_bytes(m.0[0..4].try_into().unwrap()), 2); // ties to even
        ld(&mut f, -2.5);
        f.exec(Mnemonic::Fisttp, 8, (MemorySize::Int32 as u32) << 8, 0, 0, &mut m);
        assert_eq!(i32::from_le_bytes(m.0[8..12].try_into().unwrap()), -2);
    }
}
