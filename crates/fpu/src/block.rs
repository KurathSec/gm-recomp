//! The x87 state as it lives in guest memory, where translated code works on it
//! directly (`recomp::x87`). Registers that hold a normal double, a zero or an
//! infinity are kept as f64 so inline code can compute with them; any other value
//! (extended precision, NaN, denormal, unsupported encodings) keeps its 80-bit
//! image and is only touched by [`Fpu::exec`]. One block per thread, just below
//! its TEB.

use super::*;

/// Bytes from the block to the TEB (`fs_base - OFFSET` is the block).
pub const OFFSET: u32 = 0x100;
pub const SIZE: usize = 0xB0;

/// f64 × 8, physical registers R0..R7.
pub const REGS: u32 = 0x00;
/// u8 × 8: [`VALID`] | [`EXT_IMAGE`].
pub const KIND: u32 = 0x40;
pub const TOP: u32 = 0x48;
/// u32: which inline paths are allowed (`FAST_*`).
pub const FAST: u32 = 0x4C;
pub const CW: u32 = 0x50;
/// Status word without TOP.
pub const SW: u32 = 0x52;
pub const FIP: u32 = 0x54;
pub const FDP: u32 = 0x58;
pub const FOP: u32 = 0x5C;
/// 10-byte images × 8 for registers with [`EXT_IMAGE`].
pub const EXT: u32 = 0x60;

/// The register is not empty.
pub const VALID: u8 = 1;
/// The register's image is in `EXT`, not `REGS`.
pub const EXT_IMAGE: u8 = 2;

/// No unmasked exception is pending.
pub const FAST_MOVE: u32 = 1;
/// … and precision exceptions are masked (inexact results allowed: fist, frndint,
/// fst m32; any rounding mode).
pub const FAST_INEXACT: u32 = 2;
/// … and precision control is 53 bits (add, sub, mul, div in f64; any rounding mode).
pub const FAST_ARITH: u32 = 4;
/// … and precision control is 64 bits: add, sub, mul, div whose f64 result is exact
/// (then it is the 64-bit result too, with no precision exception; any rounding mode).
pub const FAST_EXACT64: u32 = 8;

/// `FAST` for a control word with no exception pending.
pub fn fast_bits(cw: u16) -> u32 {
    let pm = (cw >> 5) & 1 != 0;
    let pc = (cw >> 8) & 3;
    FAST_MOVE | if pm { FAST_INEXACT } else { 0 } | if pm && pc == 2 { FAST_ARITH } else { 0 } | if pc == 3 { FAST_EXACT64 } else { 0 }
}

/// The f64 equal to an 80-bit image, when that is a zero, an infinity or a normal double.
pub fn ext_to_f64(bits: u128) -> Option<f64> {
    let sign = (bits >> 79) & 1;
    let exp = ((bits >> 64) & 0x7fff) as i32;
    let mant = bits as u64;
    let s = (sign as u64) << 63;
    if exp == 0 && mant == 0 {
        return Some(f64::from_bits(s));
    }
    if exp == 0x7fff && mant == 1 << 63 {
        return Some(f64::from_bits(s | 0x7ff0_0000_0000_0000));
    }
    // A normal double: integer bit set, low 11 bits zero, exponent in range.
    let e = exp - 16383 + 1023;
    if mant >> 63 == 0 || mant & 0x7ff != 0 || !(1..=2046).contains(&e) {
        return None;
    }
    Some(f64::from_bits(s | (e as u64) << 52 | (mant >> 11) & ((1 << 52) - 1)))
}

/// The 80-bit image of a zero, infinity or normal double.
pub fn f64_to_ext(v: f64) -> u128 {
    let b = v.to_bits();
    let sign = ((b >> 63) as u128) << 79;
    let exp = ((b >> 52) & 0x7ff) as u128;
    let frac = b & ((1 << 52) - 1);
    if exp == 0 {
        debug_assert!(frac == 0, "denormal doubles are kept as images");
        return sign;
    }
    if exp == 0x7ff {
        return sign | 0x7fff << 64 | 1 << 63;
    }
    sign | (exp + 16383 - 1023) << 64 | 1 << 63 | (frac as u128) << 11
}

impl Fpu {
    /// Reads the state from a block image.
    pub fn from_block(b: &[u8]) -> Fpu {
        let u16_at = |o: u32| u16::from_le_bytes([b[o as usize], b[o as usize + 1]]);
        let u32_at = |o: u32| u32::from_le_bytes(b[o as usize..o as usize + 4].try_into().unwrap());
        let mut f = Fpu::new();
        for p in 0..8 {
            let kind = b[(KIND + p) as usize];
            f.regs[p as usize] = if kind & EXT_IMAGE != 0 {
                let mut x = [0u8; 16];
                x[..10].copy_from_slice(&b[(EXT + 10 * p) as usize..(EXT + 10 * p + 10) as usize]);
                u128::from_le_bytes(x)
            } else {
                let o = (REGS + 8 * p) as usize;
                f64_to_ext(f64::from_le_bytes(b[o..o + 8].try_into().unwrap()))
            };
            f.tags[p as usize] = if kind & VALID == 0 { 3 } else { Self::tag_for_bits(f.regs[p as usize]) };
        }
        f.top = (u32_at(TOP) & 7) as u8;
        f.cw = u16_at(CW);
        f.sw = u16_at(SW) & !0x3800;
        f.fip = u32_at(FIP);
        f.fdp = u32_at(FDP);
        f.fop = u16_at(FOP);
        f.unmasked_pending = f.sw & 0x3f & !(f.cw & 0x3f) != 0;
        f
    }

    /// Writes the state as a block image.
    pub fn to_block(&self, b: &mut [u8]) {
        for p in 0..8u32 {
            let bits = self.regs[p as usize];
            let mut kind = if self.tags[p as usize] == 3 { 0 } else { VALID };
            let r = (REGS + 8 * p) as usize;
            match ext_to_f64(bits) {
                Some(v) => b[r..r + 8].copy_from_slice(&v.to_le_bytes()),
                None => {
                    kind |= EXT_IMAGE;
                    b[r..r + 8].fill(0);
                    let x = (EXT + 10 * p) as usize;
                    b[x..x + 10].copy_from_slice(&bits.to_le_bytes()[..10]);
                }
            }
            b[(KIND + p) as usize] = kind;
        }
        let put = |b: &mut [u8], o: u32, v: &[u8]| b[o as usize..o as usize + v.len()].copy_from_slice(v);
        put(b, TOP, &(self.top as u32).to_le_bytes());
        let fast = if self.unmasked_pending { 0 } else { fast_bits(self.cw) };
        put(b, FAST, &fast.to_le_bytes());
        put(b, CW, &self.cw.to_le_bytes());
        put(b, SW, &(self.sw & !0x3800).to_le_bytes());
        put(b, FIP, &self.fip.to_le_bytes());
        put(b, FDP, &self.fdp.to_le_bytes());
        put(b, FOP, &self.fop.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_round_trip() {
        for v in [0.0, -0.0, 1.0, -2.5, f64::MAX, f64::MIN_POSITIVE, f64::INFINITY, f64::NEG_INFINITY, 1e-300, 123456.789] {
            let bits = f64_to_ext(v);
            let e = Ext::from_bits(bits);
            let mut lost = false;
            let d: Double = e.convert_r(Round::NearestTiesToEven, &mut lost).value;
            assert_eq!(d.to_bits() as u64, v.to_bits(), "{v}");
            assert!(!lost);
            assert_eq!(ext_to_f64(bits).map(f64::to_bits), Some(v.to_bits()));
        }
        // not doubles: subnormal double, extended-only precision, NaN
        assert!(ext_to_f64(f64_to_ext(1.0) | 1).is_none());
        assert!(ext_to_f64(0x0001_8000_0000_0000_0000).is_none());
        assert!(ext_to_f64(Fpu::INDEFINITE_BITS).is_none());
        let sub = Ext::from_bits(0x3c00_8000_0000_0000_0000); // 2^-1023
        assert!(ext_to_f64(sub.to_bits()).is_none());
    }

    #[test]
    fn block_round_trip() {
        let mut f = Fpu::new();
        let mut m = NoMem;
        f.exec(Mnemonic::Fld1, 0, 0, 0x1000, 0, &mut m);
        f.exec(Mnemonic::Fldpi, 0, 0, 0x1004, 0, &mut m);
        let mut b = [0u8; SIZE];
        f.to_block(&mut b);
        let g = Fpu::from_block(&b);
        let mut fx1 = [0u8; 512];
        let mut fx2 = [0u8; 512];
        f.write_fxsave(&mut fx1);
        g.write_fxsave(&mut fx2);
        assert_eq!(fx1[..160], fx2[..160]);
        assert_eq!(f.tag_word(), g.tag_word());
        assert_eq!(b[KIND as usize + 7], VALID); // 1.0 at R7 as f64
        assert_eq!(b[KIND as usize + 6], VALID | EXT_IMAGE); // pi at R6 needs 64 bits
    }

    struct NoMem;
    impl Mem for NoMem {
        fn read(&mut self, _: u32, _: &mut [u8]) {}
        fn write(&mut self, _: u32, _: &[u8]) {}
    }
}
