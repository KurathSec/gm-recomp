//! Direct3D 10/11 shader bytecode (DXBC: shader model 4.0/4.1/5.0, vertex and pixel
//! shaders) to GLSL ES 3.00, for hle::d3d11.
//!
//! Conventions shared with web/src/gpu.mjs (the same as hle::d3d9_shader's, see there):
//! - vertex inputs are `in vec4 a_<SEMANTIC><index>; // v<register>`, SEMANTIC being the
//!   Direct3D 9 usage name, and bit `register` of `u_bgra` says the input is a
//!   B8G8R8A8 color to swizzle;
//! - stage outputs/inputs are `v_<SEMANTIC><index>` vec4 varyings, so a vertex and a
//!   pixel shader translated separately link by semantics (`// @varyings`, `// @init`
//!   mark where the host adds a pixel shader input the vertex shader does not write).
//!   A varying holds its signature element's components from `.x` on, whatever
//!   register components the element is packed into in either stage;
//! - constant buffers are packed into `uniform vec4 vc[256]` (vertex) / `pc[224]`
//!   (pixel): `Shader::cbuffers` says where each slot starts; textures are
//!   `uniform highp sampler2D s<t#>` (`sampler2DShadow` when sampled with comparison);
//! - rows are stored in D3D order, so the vertex shader negates y and maps depth 0..1 to
//!   -1..1 (no half-pixel offset: Direct3D 10+ has none).
//!
//! Registers are typeless 32-bit values: temporaries, inputs and outputs are `vec4`s
//! holding the bits and integer instructions reinterpret them (floatBitsToInt and so
//! on, as wined3d does), so float and integer code can share a register.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

/// An input or output signature element (ISGN/OSGN).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SigElem {
    /// Semantic name as written (e.g. "POSITION", "SV_Position", "TEXCOORD").
    pub name: String,
    pub index: u32,
    /// D3D_NAME system value (0 = none, 1 = position, ...; 64 = target).
    pub sysval: u32,
    pub register: u32,
    pub mask: u8,
}

/// A translated shader.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Shader {
    pub vertex: bool,
    pub glsl: String,
    pub inputs: Vec<SigElem>,
    pub outputs: Vec<SigElem>,
    /// Constant buffers the shader reads: (slot cb#, first vec4 in `vc`/`pc`, size in vec4).
    pub cbuffers: Vec<(u32, u32, u32)>,
    /// Textures it samples: (texture slot t#, sampler slot s#).
    pub samples: Vec<(u32, u32)>,
}

// Opcodes (D3D10_SB_OPCODE_TYPE, D3D11_SB_OPCODE_TYPE).
const ADD: u32 = 0;
const AND: u32 = 1;
const BREAK: u32 = 2;
const BREAKC: u32 = 3;
const CALL: u32 = 4;
const CALLC: u32 = 5;
const CASE: u32 = 6;
const CONTINUE: u32 = 7;
const CONTINUEC: u32 = 8;
const DEFAULT: u32 = 10;
const DERIV_RTX: u32 = 11;
const DERIV_RTY: u32 = 12;
const DISCARD: u32 = 13;
const DIV: u32 = 14;
const DP2: u32 = 15;
const DP3: u32 = 16;
const DP4: u32 = 17;
const ELSE: u32 = 18;
const ENDIF: u32 = 21;
const ENDLOOP: u32 = 22;
const ENDSWITCH: u32 = 23;
const EQ: u32 = 24;
const EXP: u32 = 25;
const FRC: u32 = 26;
const FTOI: u32 = 27;
const FTOU: u32 = 28;
const GE: u32 = 29;
const IADD: u32 = 30;
const IF: u32 = 31;
const IEQ: u32 = 32;
const IGE: u32 = 33;
const ILT: u32 = 34;
const IMAD: u32 = 35;
const IMAX: u32 = 36;
const IMIN: u32 = 37;
const IMUL: u32 = 38;
const INE: u32 = 39;
const INEG: u32 = 40;
const ISHL: u32 = 41;
const ISHR: u32 = 42;
const ITOF: u32 = 43;
const LABEL: u32 = 44;
const LD: u32 = 45;
const LOG: u32 = 47;
const LOOP: u32 = 48;
const LT: u32 = 49;
const MAD: u32 = 50;
const MIN: u32 = 51;
const MAX: u32 = 52;
const CUSTOMDATA: u32 = 53;
const MOV: u32 = 54;
const MOVC: u32 = 55;
const MUL: u32 = 56;
const NE: u32 = 57;
const NOP: u32 = 58;
const NOT: u32 = 59;
const OR: u32 = 60;
const RESINFO: u32 = 61;
const RET: u32 = 62;
const RETC: u32 = 63;
const ROUND_NE: u32 = 64;
const ROUND_NI: u32 = 65;
const ROUND_PI: u32 = 66;
const ROUND_Z: u32 = 67;
const RSQ: u32 = 68;
const SAMPLE: u32 = 69;
const SAMPLE_C: u32 = 70;
const SAMPLE_C_LZ: u32 = 71;
const SAMPLE_L: u32 = 72;
const SAMPLE_D: u32 = 73;
const SAMPLE_B: u32 = 74;
const SQRT: u32 = 75;
const SWITCH: u32 = 76;
const SINCOS: u32 = 77;
const UDIV: u32 = 78;
const ULT: u32 = 79;
const UGE: u32 = 80;
const UMUL: u32 = 81;
const UMAD: u32 = 82;
const UMAX: u32 = 83;
const UMIN: u32 = 84;
const USHR: u32 = 85;
const UTOF: u32 = 86;
const XOR: u32 = 87;
const DCL_RESOURCE: u32 = 88;
const DCL_CONSTANT_BUFFER: u32 = 89;
const DCL_INDEX_RANGE: u32 = 91;
const DCL_INPUT: u32 = 95;
const DCL_INPUT_SGV: u32 = 96;
const DCL_INPUT_SIV: u32 = 97;
const DCL_INPUT_PS_SGV: u32 = 99;
const DCL_INPUT_PS_SIV: u32 = 100;
const DCL_OUTPUT: u32 = 101;
const DCL_OUTPUT_SGV: u32 = 102;
const DCL_OUTPUT_SIV: u32 = 103;
const DCL_TEMPS: u32 = 104;
const DCL_INDEXABLE_TEMP: u32 = 105;
const DCL_GLOBAL_FLAGS: u32 = 106;
const DERIV_RTX_COARSE: u32 = 122;
const DERIV_RTX_FINE: u32 = 123;
const DERIV_RTY_COARSE: u32 = 124;
const DERIV_RTY_FINE: u32 = 125;
const RCP: u32 = 129;
const F32TOF16: u32 = 130;
const F16TOF32: u32 = 131;
const SWAPC: u32 = 142;

/// Opcode names by number, for error messages.
const OPCODE_NAMES: &str = "add and break breakc call callc case continue continuec cut default deriv_rtx deriv_rty discard div dp2 dp3 dp4 else emit emitthencut endif endloop endswitch eq exp frc ftoi ftou ge iadd if ieq ige ilt imad imax imin imul ine ineg ishl ishr itof label ld ld_ms log loop lt mad min max customdata mov movc mul ne nop not or resinfo ret retc round_ne round_ni round_pi round_z rsq sample sample_c sample_c_lz sample_l sample_d sample_b sqrt switch sincos udiv ult uge umul umad umax umin ushr utof xor dcl_resource dcl_constantbuffer dcl_sampler dcl_index_range dcl_outputtopology dcl_inputprimitive dcl_maxout dcl_input dcl_input_sgv dcl_input_siv dcl_input_ps dcl_input_ps_sgv dcl_input_ps_siv dcl_output dcl_output_sgv dcl_output_siv dcl_temps dcl_indexableTemp dcl_globalFlags reserved0 lod gather4 sample_pos sample_info reserved1 hs_decls hs_control_point_phase hs_fork_phase hs_join_phase emit_stream cut_stream emitthencut_stream fcall bufinfo deriv_rtx_coarse deriv_rtx_fine deriv_rty_coarse deriv_rty_fine gather4_c gather4_po gather4_po_c rcp f32tof16 f16tof32 uaddc usubb countbits firstbit_hi firstbit_lo firstbit_shi ubfe ibfe bfi bfrev swapc dcl_stream dcl_function_body dcl_function_table dcl_interface dcl_input_control_point_count dcl_output_control_point_count dcl_tessellator_domain dcl_tessellator_partitioning dcl_tessellator_output_primitive dcl_hs_max_tessfactor dcl_hs_fork_phase_instance_count dcl_hs_join_phase_instance_count dcl_thread_group dcl_uav_typed dcl_uav_raw dcl_uav_structured dcl_tgsm_raw dcl_tgsm_structured dcl_resource_raw dcl_resource_structured ld_uav_typed store_uav_typed ld_raw store_raw ld_structured store_structured atomic_and atomic_or atomic_xor atomic_cmp_store atomic_iadd atomic_imax atomic_imin atomic_umax atomic_umin imm_atomic_alloc imm_atomic_consume imm_atomic_iadd imm_atomic_and imm_atomic_or imm_atomic_xor imm_atomic_exch imm_atomic_cmp_exch imm_atomic_imax imm_atomic_imin imm_atomic_umax imm_atomic_umin sync dadd dmax dmin dmul deq dge dlt dne dmov dmovc dtof ftod eval_snapped eval_sample_index eval_centroid dcl_gs_instance_count abort debug_break reserved2 ddiv dfma drcp msad dtoi dtou itod utod";

// Operand types (D3D10_SB_OPERAND_TYPE).
const TEMP: u32 = 0;
const INPUT: u32 = 1;
const OUTPUT: u32 = 2;
const INDEXABLE_TEMP: u32 = 3;
const IMMEDIATE32: u32 = 4;
const IMMEDIATE64: u32 = 5;
const CONSTANT_BUFFER: u32 = 8;
const IMMEDIATE_CONSTANT_BUFFER: u32 = 9;
const OUTPUT_DEPTH: u32 = 12;
const NULL: u32 = 13;
const OUTPUT_COVERAGE_MASK: u32 = 15;
const OUTPUT_DEPTH_GREATER_EQUAL: u32 = 38;
const OUTPUT_DEPTH_LESS_EQUAL: u32 = 39;

const OPERAND_NAMES: &str = "temp input output indexable_temp immediate32 immediate64 sampler resource constant_buffer immediate_constant_buffer label input_primitive_id output_depth null rasterizer output_coverage_mask stream function_body function_table interface function_input function_output output_control_point_id input_fork_instance_id input_join_instance_id input_control_point output_control_point input_patch_constant input_domain_point this_pointer unordered_access_view thread_group_shared_memory input_thread_id input_thread_group_id input_thread_id_in_group input_coverage_mask input_thread_id_in_group_flattened input_gs_instance_id output_depth_greater_equal output_depth_less_equal cycle_counter";

/// Direct3D 9 vertex input usages a vertex input semantic must be one of.
const USAGES: [&str; 10] = ["POSITION", "BLENDWEIGHT", "BLENDINDICES", "NORMAL", "PSIZE", "TEXCOORD", "TANGENT", "BINORMAL", "COLOR", "FOG"];

const XYZW: [char; 4] = ['x', 'y', 'z', 'w'];

fn op_name(op: u32) -> String {
    OPCODE_NAMES.split(' ').nth(op as usize).map_or_else(|| format!("opcode {op}"), str::to_string)
}

fn operand_name(ty: u32) -> String {
    OPERAND_NAMES.split(' ').nth(ty as usize).map_or_else(|| format!("operand type {ty}"), str::to_string)
}

/// One index of an operand: `imm`, plus a register's value when relative.
#[derive(Clone, Debug)]
struct Index {
    imm: u32,
    rel: Option<Box<Operand>>,
}

#[derive(Clone, Debug)]
struct Operand {
    ty: u32,
    /// 0, 1 or 4.
    comps: u32,
    /// Write mask (destinations).
    mask: u8,
    /// Component read for each position (sources; identity for masks).
    swz: [u8; 4],
    idx: Vec<Index>,
    imm: Vec<u32>,
    /// 1 neg, 2 abs, 3 -abs.
    modifier: u32,
}

#[derive(Clone, Debug)]
struct Instr {
    op: u32,
    /// The opcode token (instruction-specific controls in bits 11..23).
    tok: u32,
    ops: Vec<Operand>,
    /// Words after the operands (declaration fields, customdata contents).
    extra: Vec<u32>,
    /// Texel offsets of sample/ld (aoffimmi).
    offset: [i32; 3],
}

impl Instr {
    fn saturate(&self) -> bool {
        self.tok & (1 << 13) != 0
    }

    /// The test of if/breakc/discard/...: nonzero (else zero).
    fn nonzero(&self) -> bool {
        self.tok & (1 << 18) != 0
    }
}

fn word(t: &[u32], i: &mut usize) -> Result<u32, String> {
    let w = *t.get(*i).ok_or("truncated instruction")?;
    *i += 1;
    Ok(w)
}

fn parse_operand(t: &[u32], i: &mut usize) -> Result<Operand, String> {
    let tok = word(t, i)?;
    let mut o = Operand { ty: (tok >> 12) & 0xFF, comps: 0, mask: 0xF, swz: [0, 1, 2, 3], idx: Vec::new(), imm: Vec::new(), modifier: 0 };
    match tok & 3 {
        0 => {}
        1 => {
            o.comps = 1;
            o.mask = 1;
            o.swz = [0; 4];
        }
        2 => {
            o.comps = 4;
            match (tok >> 2) & 3 {
                0 => o.mask = ((tok >> 4) & 0xF) as u8,
                1 => o.swz = [0, 2, 4, 6].map(|s| ((tok >> (4 + s)) & 3) as u8),
                2 => o.swz = [((tok >> 4) & 3) as u8; 4],
                m => return Err(format!("component selection mode {m}")),
            }
        }
        _ => return Err("N-component operands are not supported".into()),
    }
    let mut ext = tok >> 31 != 0;
    while ext {
        let e = word(t, i)?;
        if e & 0x3F == 1 {
            o.modifier = (e >> 6) & 0xFF;
        }
        ext = e >> 31 != 0;
    }
    match o.ty {
        IMMEDIATE32 => {
            for _ in 0..o.comps.max(1) {
                o.imm.push(word(t, i)?);
            }
        }
        IMMEDIATE64 => return Err("64-bit immediates are not supported".into()),
        _ => {}
    }
    for d in 0..(tok >> 20) & 3 {
        let rep = (tok >> (22 + 3 * d)) & 7;
        let ix = match rep {
            0 => Index { imm: word(t, i)?, rel: None },
            2 => Index { imm: 0, rel: Some(Box::new(parse_operand(t, i)?)) },
            3 => {
                let imm = word(t, i)?;
                Index { imm, rel: Some(Box::new(parse_operand(t, i)?)) }
            }
            _ => return Err(format!("operand index representation {rep} is not supported")),
        };
        o.idx.push(ix);
    }
    Ok(o)
}

/// Instructions whose operands this parser knows how to read; others are kept as
/// bare opcodes and rejected by name when translated.
fn has_operands(op: u32) -> bool {
    op <= DCL_GLOBAL_FLAGS || matches!(op, DERIV_RTX_COARSE..=DERIV_RTY_FINE | RCP | F32TOF16 | F16TOF32 | SWAPC)
}

/// Parses a SHDR/SHEX token stream: (vertex shader?, instructions).
fn parse(tokens: &[u32]) -> Result<(bool, Vec<Instr>), String> {
    let version = *tokens.first().ok_or("empty shader")?;
    let vertex = match version >> 16 {
        0 => false,
        1 => true,
        t => return Err(format!("program type {t} is not a vertex or pixel shader")),
    };
    let len = (*tokens.get(1).ok_or("truncated shader")? as usize).min(tokens.len());
    let mut out = Vec::new();
    let mut i = 2;
    while i < len {
        let tok = tokens[i];
        let op = tok & 0x7FF;
        let n = if op == CUSTOMDATA { *tokens.get(i + 1).ok_or("truncated customdata")? as usize } else { ((tok >> 24) & 0x7F) as usize };
        let body = tokens.get(i..i + n).filter(|_| n >= if op == CUSTOMDATA { 2 } else { 1 }).ok_or("truncated instruction")?;
        i += n;
        let mut ins = Instr { op, tok, ops: Vec::new(), extra: Vec::new(), offset: [0; 3] };
        if op == CUSTOMDATA {
            ins.extra = body[2..].to_vec();
            out.push(ins);
            continue;
        }
        let mut k = 1;
        let mut ext = tok >> 31 != 0;
        while ext {
            let e = word(body, &mut k)?;
            if e & 0x3F == 1 {
                // sample controls: 4-bit signed u, v, w texel offsets
                for (c, s) in [9, 13, 17].into_iter().enumerate() {
                    ins.offset[c] = ((((e >> s) & 0xF) as i32) << 28) >> 28;
                }
            }
            ext = e >> 31 != 0;
        }
        if has_operands(op) {
            match op {
                DCL_TEMPS | DCL_INDEXABLE_TEMP | DCL_GLOBAL_FLAGS => {}
                // one operand, then a system value name, return types or a count
                DCL_INPUT_SGV | DCL_INPUT_SIV | DCL_INPUT_PS_SGV | DCL_INPUT_PS_SIV | DCL_OUTPUT_SGV | DCL_OUTPUT_SIV | DCL_RESOURCE | DCL_INDEX_RANGE => {
                    ins.ops.push(parse_operand(body, &mut k)?);
                }
                _ => {
                    while k < body.len() {
                        ins.ops.push(parse_operand(body, &mut k)?);
                    }
                }
            }
            ins.extra = body[k..].to_vec();
        }
        out.push(ins);
    }
    Ok((vertex, out))
}

/// Reads a signature chunk whose elements are `stride` bytes (24: ISGN/OSGN; 28: OSG5
/// and 32: ISG1/OSG1, which start with a stream word). Returns the elements with their
/// component types (1 uint, 2 sint, 3 float).
fn parse_sig(c: &[u8], stride: usize) -> Result<Vec<(SigElem, u32)>, String> {
    let rd = |o: usize| c.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])).ok_or_else(|| "truncated signature".to_string());
    let count = rd(0)? as usize;
    let first = rd(4)? as usize;
    let skip = if stride == 24 { 0 } else { 4 };
    let mut out = Vec::new();
    for k in 0..count {
        let e = first + k * stride + skip;
        let at = rd(e)? as usize;
        let name = c.get(at..).and_then(|s| s.split(|&b| b == 0).next()).ok_or("bad signature element name")?;
        let mask = *c.get(e + 20).ok_or("truncated signature")?;
        let elem = SigElem { name: String::from_utf8_lossy(name).into_owned(), index: rd(e + 4)?, sysval: rd(e + 8)?, register: rd(e + 16)?, mask };
        out.push((elem, rd(e + 12)?));
    }
    Ok(out)
}

/// How an expression's bits are typed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ty {
    F,
    I,
    U,
}

/// The GLSL type of an n-component value.
fn vty(ty: Ty, n: usize) -> String {
    let (s, v) = match ty {
        Ty::F => ("float", "vec"),
        Ty::I => ("int", "ivec"),
        Ty::U => ("uint", "uvec"),
    };
    if n == 1 { s.into() } else { format!("{v}{n}") }
}

/// Reinterprets an n-component expression's bits.
fn conv(e: String, from: Ty, to: Ty, n: usize) -> String {
    match (from, to) {
        (Ty::F, Ty::I) => format!("floatBitsToInt({e})"),
        (Ty::F, Ty::U) => format!("floatBitsToUint({e})"),
        (Ty::I, Ty::F) => format!("intBitsToFloat({e})"),
        (Ty::U, Ty::F) => format!("uintBitsToFloat({e})"),
        (Ty::I, Ty::U) | (Ty::U, Ty::I) => format!("{}({e})", vty(to, n)),
        _ => e,
    }
}

fn int_lit(v: i32) -> String {
    match v {
        i32::MIN => "(-2147483647 - 1)".into(),
        v if v < 0 => format!("({v})"),
        v => v.to_string(),
    }
}

/// An immediate's bits as a literal of the type it is read as. Floats that are not
/// zero or normal (integers moved through a register, NaN masks) keep their bits.
fn lit(bits: u32, ty: Ty) -> String {
    match ty {
        Ty::I => int_lit(bits as i32),
        Ty::U => format!("{bits}u"),
        Ty::F => {
            let f = f32::from_bits(bits);
            if f != 0.0 && !f.is_normal() {
                return format!("intBitsToFloat({})", int_lit(bits as i32));
            }
            let s = format!("{f:?}");
            let s = if s.contains(['.', 'e', 'E']) { s } else { format!("{s}.0") };
            if s.starts_with('-') { format!("({s})") } else { s }
        }
    }
}

fn comps_str(pos: &[usize]) -> String {
    pos.iter().map(|&p| XYZW[p]).collect()
}

/// `reg = val` for a signature element in `reg`'s `mask` components, `val` holding the
/// element's components from `.x` on. An element at `.x` sets the whole register (so
/// unused components keep the source's defaults); packed ones follow it.
fn read_elem(reg: &str, mask: u8, val: &str) -> String {
    let pos: Vec<usize> = (0..4).filter(|k| mask & (1 << k) != 0).collect();
    if mask & 1 != 0 || pos.is_empty() { format!("{reg} = {val};") } else { format!("{reg}.{} = ({val}).{};", comps_str(&pos), &"xyzw"[..pos.len()]) }
}

/// The element in `reg`'s `mask` components as a vec4 from `.x` on.
fn write_elem(reg: &str, mask: u8) -> String {
    let pos: Vec<usize> = (0..4).filter(|k| mask & (1 << k) != 0).collect();
    if mask == 0xF || pos.is_empty() { reg.into() } else { format!("vec4({reg}.{}{})", comps_str(&pos), ", 0.0".repeat(4 - pos.len())) }
}

/// The varying name of a stage output/input element: `<SEMANTIC><index>`, upper case
/// (semantics are case-insensitive).
fn varying_name(e: &SigElem) -> Result<String, String> {
    if e.name.is_empty() || !e.name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err(format!("semantic {:?} is not a GLSL identifier", e.name));
    }
    Ok(format!("{}{}", e.name.to_ascii_uppercase(), e.index))
}

struct Gen {
    vertex: bool,
    /// cb# -> (first vec4, size).
    cbs: BTreeMap<u32, (u32, u32)>,
    /// t# -> the type its texels come back as.
    textures: BTreeMap<u32, Ty>,
    /// Textures sampled with comparison (sample_c).
    shadow: BTreeSet<u32>,
    samples: Vec<(u32, u32)>,
    temps: BTreeSet<u32>,
    /// x# -> number of registers.
    indexable: BTreeMap<u32, u32>,
    icb: Vec<u32>,
    inputs: BTreeSet<u32>,
    outputs: BTreeSet<u32>,
    uses_depth: bool,
    body: String,
    indent: usize,
}

impl Gen {
    fn line(&mut self, s: &str) {
        for _ in 0..self.indent {
            self.body.push_str("  ");
        }
        self.body.push_str(s);
        self.body.push('\n');
    }

    /// A register's vec4 (before component selection) and the type its bits are kept as.
    fn reg(&mut self, o: &Operand) -> Result<(String, Ty), String> {
        let direct = |k: usize| match o.idx.get(k) {
            Some(Index { imm, rel: None }) => Ok(*imm),
            _ => Err(format!("{} with a relative or missing index is not supported", operand_name(o.ty))),
        };
        Ok(match o.ty {
            TEMP => {
                let r = direct(0)?;
                self.temps.insert(r);
                (format!("r{r}"), Ty::F)
            }
            INPUT => {
                if o.idx.len() != 1 {
                    return Err("multi-dimensional inputs are not supported".into());
                }
                let r = direct(0)?;
                self.inputs.insert(r);
                (format!("v{r}"), Ty::F)
            }
            OUTPUT => {
                let r = direct(0)?;
                self.outputs.insert(r);
                (format!("o{r}"), Ty::F)
            }
            INDEXABLE_TEMP => {
                let r = direct(0)?;
                let len = *self.indexable.get(&r).ok_or_else(|| format!("x{r} is not declared"))?;
                let ix = o.idx.get(1).ok_or("indexable temp without an index")?;
                let i = self.index(ix, 0, len)?;
                (format!("x{r}[{i}]"), Ty::F)
            }
            CONSTANT_BUFFER => {
                let slot = direct(0)?;
                let (base, size) = *self.cbs.get(&slot).ok_or_else(|| format!("cb{slot} is not declared"))?;
                let ix = o.idx.get(1).ok_or("constant buffer operand without a register index")?;
                if ix.rel.is_none() && ix.imm >= size {
                    return Err(format!("cb{slot}[{}] is outside its declared size {size}", ix.imm));
                }
                let i = self.index(ix, base, size)?;
                (format!("{}[{i}]", if self.vertex { "vc" } else { "pc" }), Ty::F)
            }
            IMMEDIATE_CONSTANT_BUFFER => {
                let len = self.icb.len().div_ceil(4) as u32;
                if len == 0 {
                    return Err("the immediate constant buffer is not declared".into());
                }
                let i = self.index(o.idx.first().ok_or("icb operand without an index")?, 0, len)?;
                (format!("icb[{i}]"), Ty::I)
            }
            OUTPUT_DEPTH | OUTPUT_DEPTH_GREATER_EQUAL | OUTPUT_DEPTH_LESS_EQUAL => {
                self.uses_depth = true;
                ("oDepth".into(), Ty::F)
            }
            t => return Err(format!("{} operands are not supported", operand_name(t))),
        })
    }

    /// An array index: immediate, or clamped to the array when relative.
    fn index(&mut self, ix: &Index, base: u32, len: u32) -> Result<String, String> {
        match &ix.rel {
            None => Ok((base as u64 + ix.imm as u64).to_string()),
            Some(r) => {
                let e = self.src(r, Ty::I, &[0])?;
                Ok(format!("clamp({} + {e}, {base}, {})", base.wrapping_add(ix.imm) as i32, base as u64 + len.max(1) as u64 - 1))
            }
        }
    }

    /// A source operand's components at swizzle positions `pos`, typed `ty`.
    fn src(&mut self, o: &Operand, ty: Ty, pos: &[usize]) -> Result<String, String> {
        let n = pos.len();
        let comps: Vec<usize> = pos.iter().map(|&p| o.swz[p] as usize).collect();
        let v = if o.ty == IMMEDIATE32 {
            let vals: Vec<String> = pos.iter().map(|&p| lit(o.imm[p.min(o.imm.len() - 1)], ty)).collect();
            if n == 1 { vals[0].clone() } else { format!("{}({})", vty(ty, n), vals.join(", ")) }
        } else {
            let (base, from) = self.reg(o)?;
            let sw = comps_str(&comps);
            let e = if sw == "xyzw" { base } else { format!("{base}.{sw}") };
            conv(e, from, ty, n)
        };
        Ok(match (o.modifier, ty) {
            (1, _) | (3, Ty::U) => format!("(-{v})"),
            (2, Ty::U) => v,
            (2, _) => format!("abs({v})"),
            (3, _) => format!("(-abs({v}))"),
            _ => v,
        })
    }

    /// A destination's name and written components (None: nothing is written).
    fn dst(&mut self, o: &Operand) -> Result<Option<(String, Vec<usize>)>, String> {
        match o.ty {
            NULL | OUTPUT_COVERAGE_MASK => return Ok(None),
            TEMP | OUTPUT | INDEXABLE_TEMP | OUTPUT_DEPTH | OUTPUT_DEPTH_GREATER_EQUAL | OUTPUT_DEPTH_LESS_EQUAL => {}
            t => return Err(format!("{} destinations are not supported", operand_name(t))),
        }
        let (name, _) = self.reg(o)?;
        let mask = if o.comps == 1 { 1 } else { o.mask };
        let pos: Vec<usize> = (0..4).filter(|&k| mask & (1 << k) != 0).collect();
        Ok(if pos.is_empty() { None } else { Some((name, pos)) })
    }

    /// `name.pos = v`, v being typed `ty` (saturated if the instruction says so).
    fn store(&mut self, ins: &Instr, name: &str, pos: &[usize], ty: Ty, v: String) {
        let n = pos.len();
        let v = if ty == Ty::F && ins.saturate() { format!("clamp({v}, 0.0, 1.0)") } else { v };
        let v = conv(v, ty, Ty::F, n);
        if n == 4 {
            self.line(&format!("{name} = {v};"));
        } else {
            self.line(&format!("{name}.{} = {v};", comps_str(pos)));
        }
    }

    /// Stores several results computed before any is written (a destination may be
    /// another's source).
    fn store_all(&mut self, ins: &Instr, vals: Vec<(String, Vec<usize>, Ty, String)>) {
        if vals.len() < 2 {
            for (name, pos, ty, v) in vals {
                self.store(ins, &name, &pos, ty, v);
            }
            return;
        }
        self.line("{");
        self.indent += 1;
        for (k, (_, pos, ty, v)) in vals.iter().enumerate() {
            self.line(&format!("{} tmp{k} = {v};", vty(*ty, pos.len())));
        }
        for (k, (name, pos, ty, _)) in vals.iter().enumerate() {
            self.store(ins, name, pos, *ty, format!("tmp{k}"));
        }
        self.indent -= 1;
        self.line("}");
    }

    /// A component-wise instruction: dst = f(sources read as `tin`), typed `tout`.
    fn cw(&mut self, ins: &Instr, tin: Ty, tout: Ty, f: impl Fn(&[String], usize) -> String) -> Result<(), String> {
        let Some((name, pos)) = self.dst(&ins.ops[0])? else { return Ok(()) };
        let mut a = Vec::new();
        for s in &ins.ops[1..] {
            a.push(self.src(s, tin, &pos)?);
        }
        let v = f(&a, pos.len());
        self.store(ins, &name, &pos, tout, v);
        Ok(())
    }

    /// A comparison: all ones where true, zero where false.
    fn cmp(&mut self, ins: &Instr, ty: Ty, op: &str, func: &str) -> Result<(), String> {
        self.cw(ins, ty, Ty::I, |a, n| if n == 1 { format!("-int({} {op} {})", a[0], a[1]) } else { format!("-ivec{n}({func}({}, {}))", a[0], a[1]) })
    }

    /// Per destination component: `cond != 0 ? a : b` (bits are moved, not converted).
    fn select(&mut self, cond: &Operand, a: &Operand, b: &Operand, pos: &[usize]) -> Result<String, String> {
        let mut c = Vec::new();
        let mut x = Vec::new();
        let mut y = Vec::new();
        for &p in pos {
            c.push(self.src(cond, Ty::I, &[p])?);
            x.push(self.src(a, Ty::F, &[p])?);
            y.push(self.src(b, Ty::F, &[p])?);
        }
        if pos.len() == 1 || c.iter().all(|e| *e == c[0]) {
            let (x, y) = (self.src(a, Ty::F, pos)?, self.src(b, Ty::F, pos)?);
            return Ok(format!("({} != 0 ? {x} : {y})", c[0]));
        }
        let parts: Vec<String> = (0..pos.len()).map(|k| format!("{} != 0 ? {} : {}", c[k], x[k], y[k])).collect();
        Ok(format!("vec{}({})", pos.len(), parts.join(", ")))
    }

    /// The test of if/breakc/retc/discard/...
    fn cond(&mut self, ins: &Instr, o: &Operand) -> Result<String, String> {
        let c = self.src(o, Ty::I, &[0])?;
        Ok(format!("{c} {} 0", if ins.nonzero() { "!=" } else { "==" }))
    }

    fn label(o: &Operand) -> u32 {
        o.idx.first().map_or(0, |i| i.imm)
    }

    fn instr(&mut self, ins: &Instr) -> Result<(), String> {
        use Ty::{F, I, U};
        let o = &ins.ops;
        let need = match ins.op {
            ADD | MUL | DIV | MIN | MAX | DP2 | DP3 | DP4 | SINCOS | LD | RESINFO | EQ | NE | LT | GE | IADD | ISHL | ISHR | USHR | AND | OR | XOR | IEQ | INE | ILT | IGE | ULT | UGE | IMIN | IMAX | UMIN | UMAX => 3,
            MAD | MOVC | IMAD | UMAD | IMUL | UMUL | UDIV | SAMPLE => 4,
            SAMPLE_B | SAMPLE_L | SAMPLE_C | SAMPLE_C_LZ | SWAPC => 5,
            SAMPLE_D => 6,
            MOV | FRC | ROUND_NE | ROUND_NI | ROUND_PI | ROUND_Z | SQRT | RSQ | EXP | LOG | RCP | NOT | INEG | ITOF | UTOF | FTOI | FTOU | F32TOF16 | F16TOF32 => 2,
            DERIV_RTX | DERIV_RTX_COARSE | DERIV_RTX_FINE | DERIV_RTY | DERIV_RTY_COARSE | DERIV_RTY_FINE => 2,
            IF | BREAKC | CONTINUEC | RETC | DISCARD | SWITCH | CASE | CALL => 1,
            CALLC => 2,
            _ => 0,
        };
        if o.len() < need {
            return Err(format!("{} with {} operands", op_name(ins.op), o.len()));
        }
        match ins.op {
            DCL_RESOURCE..=DCL_GLOBAL_FLAGS | CUSTOMDATA | NOP | LABEL => {}
            ADD => self.cw(ins, F, F, |a, _| format!("{} + {}", a[0], a[1]))?,
            MUL => self.cw(ins, F, F, |a, _| format!("{} * {}", a[0], a[1]))?,
            MAD => self.cw(ins, F, F, |a, _| format!("{} * {} + {}", a[0], a[1], a[2]))?,
            DIV => self.cw(ins, F, F, |a, _| format!("{} / {}", a[0], a[1]))?,
            MIN => self.cw(ins, F, F, |a, _| format!("min({}, {})", a[0], a[1]))?,
            MAX => self.cw(ins, F, F, |a, _| format!("max({}, {})", a[0], a[1]))?,
            RCP => self.cw(ins, F, F, |a, _| format!("1.0 / {}", a[0]))?,
            MOV => self.cw(ins, F, F, |a, _| a[0].clone())?,
            FRC | ROUND_NE | ROUND_NI | ROUND_PI | ROUND_Z | SQRT | RSQ | EXP | LOG | DERIV_RTX..=DERIV_RTY | DERIV_RTX_COARSE..=DERIV_RTY_FINE => {
                let f = match ins.op {
                    FRC => "fract",
                    ROUND_NE => "roundEven",
                    ROUND_NI => "floor",
                    ROUND_PI => "ceil",
                    ROUND_Z => "trunc",
                    SQRT => "sqrt",
                    RSQ => "inversesqrt",
                    EXP => "exp2",
                    LOG => "log2",
                    DERIV_RTX | DERIV_RTX_COARSE | DERIV_RTX_FINE => "dFdx",
                    _ => "dFdy",
                };
                self.cw(ins, F, F, |a, _| format!("{f}({})", a[0]))?
            }
            DP2 | DP3 | DP4 => {
                let Some((name, pos)) = self.dst(&o[0])? else { return Ok(()) };
                let r: Vec<usize> = (0..(ins.op - DP2 + 2) as usize).collect();
                let (a, b) = (self.src(&o[1], F, &r)?, self.src(&o[2], F, &r)?);
                let d = format!("dot({a}, {b})");
                let v = if pos.len() == 1 { d } else { format!("vec{}({d})", pos.len()) };
                self.store(ins, &name, &pos, F, v);
            }
            SINCOS => {
                let mut vals = Vec::new();
                for (k, f) in [(0, "sin"), (1, "cos")] {
                    if let Some((name, pos)) = self.dst(&o[k])? {
                        let a = self.src(&o[2], F, &pos)?;
                        vals.push((name, pos, F, format!("{f}({a})")));
                    }
                }
                self.store_all(ins, vals);
            }
            MOVC => {
                let Some((name, pos)) = self.dst(&o[0])? else { return Ok(()) };
                let v = self.select(&o[1], &o[2], &o[3], &pos)?;
                self.store(ins, &name, &pos, F, v);
            }
            SWAPC => {
                let mut vals = Vec::new();
                for (k, (a, b)) in [(0, (4, 3)), (1, (3, 4))] {
                    if let Some((name, pos)) = self.dst(&o[k])? {
                        let v = self.select(&o[2], &o[a], &o[b], &pos)?;
                        vals.push((name, pos, F, v));
                    }
                }
                self.store_all(ins, vals);
            }
            EQ => self.cmp(ins, F, "==", "equal")?,
            NE => self.cmp(ins, F, "!=", "notEqual")?,
            LT => self.cmp(ins, F, "<", "lessThan")?,
            GE => self.cmp(ins, F, ">=", "greaterThanEqual")?,
            IEQ => self.cmp(ins, I, "==", "equal")?,
            INE => self.cmp(ins, I, "!=", "notEqual")?,
            ILT => self.cmp(ins, I, "<", "lessThan")?,
            IGE => self.cmp(ins, I, ">=", "greaterThanEqual")?,
            ULT => self.cmp(ins, U, "<", "lessThan")?,
            UGE => self.cmp(ins, U, ">=", "greaterThanEqual")?,
            IADD => self.cw(ins, I, I, |a, _| format!("{} + {}", a[0], a[1]))?,
            IMAD => self.cw(ins, I, I, |a, _| format!("{} * {} + {}", a[0], a[1], a[2]))?,
            UMAD => self.cw(ins, U, U, |a, _| format!("{} * {} + {}", a[0], a[1], a[2]))?,
            ISHL => self.cw(ins, I, I, |a, _| format!("{} << ({} & 31)", a[0], a[1]))?,
            ISHR => self.cw(ins, I, I, |a, _| format!("{} >> ({} & 31)", a[0], a[1]))?,
            USHR => {
                let Some((name, pos)) = self.dst(&o[0])? else { return Ok(()) };
                let (a, b) = (self.src(&o[1], U, &pos)?, self.src(&o[2], I, &pos)?);
                self.store(ins, &name, &pos, U, format!("{a} >> ({b} & 31)"));
            }
            AND => self.cw(ins, I, I, |a, _| format!("{} & {}", a[0], a[1]))?,
            OR => self.cw(ins, I, I, |a, _| format!("{} | {}", a[0], a[1]))?,
            XOR => self.cw(ins, I, I, |a, _| format!("{} ^ {}", a[0], a[1]))?,
            NOT => self.cw(ins, I, I, |a, _| format!("~{}", a[0]))?,
            INEG => self.cw(ins, I, I, |a, _| format!("-{}", a[0]))?,
            IMIN => self.cw(ins, I, I, |a, _| format!("min({}, {})", a[0], a[1]))?,
            IMAX => self.cw(ins, I, I, |a, _| format!("max({}, {})", a[0], a[1]))?,
            UMIN => self.cw(ins, U, U, |a, _| format!("min({}, {})", a[0], a[1]))?,
            UMAX => self.cw(ins, U, U, |a, _| format!("max({}, {})", a[0], a[1]))?,
            ITOF => self.cw(ins, I, F, |a, n| format!("{}({})", vty(F, n), a[0]))?,
            UTOF => self.cw(ins, U, F, |a, n| format!("{}({})", vty(F, n), a[0]))?,
            FTOI => self.cw(ins, F, I, |a, n| format!("{}({})", vty(I, n), a[0]))?,
            // Direct3D clamps negative values to 0, GLSL leaves them undefined
            FTOU => self.cw(ins, F, U, |a, n| format!("{}(max({}, 0.0))", vty(U, n), a[0]))?,
            IMUL | UMUL => {
                let ty = if ins.op == IMUL { I } else { U };
                if self.dst(&o[0])?.is_some() {
                    return Err(format!("{} with a high result is not supported", op_name(ins.op)));
                }
                if let Some((name, pos)) = self.dst(&o[1])? {
                    let (a, b) = (self.src(&o[2], ty, &pos)?, self.src(&o[3], ty, &pos)?);
                    self.store(ins, &name, &pos, ty, format!("{a} * {b}"));
                }
            }
            UDIV => {
                let mut vals = Vec::new();
                for (k, op) in [(0, "/"), (1, "%")] {
                    if let Some((name, pos)) = self.dst(&o[k])? {
                        let (a, b) = (self.src(&o[2], U, &pos)?, self.src(&o[3], U, &pos)?);
                        vals.push((name, pos, U, format!("{a} {op} {b}")));
                    }
                }
                self.store_all(ins, vals);
            }
            F32TOF16 | F16TOF32 => {
                let Some((name, pos)) = self.dst(&o[0])? else { return Ok(()) };
                let (tin, tout) = if ins.op == F32TOF16 { (F, U) } else { (U, F) };
                let mut parts = Vec::new();
                for &p in &pos {
                    let a = self.src(&o[1], tin, &[p])?;
                    parts.push(if ins.op == F32TOF16 { format!("packHalf2x16(vec2({a}, 0.0))") } else { format!("unpackHalf2x16({a}).x") });
                }
                let v = if parts.len() == 1 { parts.remove(0) } else { format!("{}({})", vty(tout, parts.len()), parts.join(", ")) };
                self.store(ins, &name, &pos, tout, v);
            }
            IF => {
                let c = self.cond(ins, &o[0])?;
                self.line(&format!("if ({c}) {{"));
                self.indent += 1;
            }
            ELSE => {
                self.indent = self.indent.saturating_sub(1);
                self.line("} else {");
                self.indent += 1;
            }
            ENDIF | ENDLOOP => {
                self.indent = self.indent.saturating_sub(1);
                self.line("}");
            }
            LOOP => {
                self.line("while (true) {");
                self.indent += 1;
            }
            BREAK => self.line("break;"),
            CONTINUE => self.line("continue;"),
            RET => self.line("return;"),
            BREAKC | CONTINUEC | RETC | DISCARD => {
                let c = self.cond(ins, &o[0])?;
                let s = match ins.op {
                    BREAKC => "break",
                    CONTINUEC => "continue",
                    RETC => "return",
                    _ => "discard",
                };
                self.line(&format!("if ({c}) {s};"));
            }
            SWITCH => {
                let c = self.src(&o[0], I, &[0])?;
                self.line(&format!("switch ({c}) {{"));
                self.indent += 2;
            }
            CASE | DEFAULT => {
                let l = if ins.op == CASE { format!("case {}:", self.src(&o[0], I, &[0])?) } else { "default:".into() };
                self.indent = self.indent.saturating_sub(1);
                self.line(&l);
                self.indent += 1;
            }
            ENDSWITCH => {
                self.indent = self.indent.saturating_sub(2);
                self.line("}");
            }
            CALL => self.line(&format!("l{}();", Self::label(&o[0]))),
            CALLC => {
                let c = self.cond(ins, &o[0])?;
                self.line(&format!("if ({c}) l{}();", Self::label(&o[1])));
            }
            SAMPLE | SAMPLE_B | SAMPLE_L | SAMPLE_D | SAMPLE_C | SAMPLE_C_LZ | LD | RESINFO => self.texture(ins)?,
            op => return Err(format!("{} is not supported", op_name(op))),
        }
        Ok(())
    }

    /// sample*, ld and resinfo (on 2D textures).
    fn texture(&mut self, ins: &Instr) -> Result<(), String> {
        use Ty::{F, I, U};
        let o = &ins.ops;
        let Some((name, pos)) = self.dst(&o[0])? else { return Ok(()) };
        let res = &o[2];
        let t = res.idx.first().map_or(0, |i| i.imm);
        let rty = *self.textures.get(&t).ok_or_else(|| format!("t{t} is not declared"))?;
        let s = format!("s{t}");
        let off = (ins.offset != [0; 3]).then(|| format!("ivec2({}, {})", ins.offset[0], ins.offset[1]));
        let op = ins.op;
        if matches!(op, SAMPLE | SAMPLE_B | SAMPLE_L | SAMPLE_D | SAMPLE_C | SAMPLE_C_LZ) {
            let smp = o[3].idx.first().map_or(0, |i| i.imm);
            if !self.samples.contains(&(t, smp)) {
                self.samples.push((t, smp));
            }
        }
        let (v, ty, scalar) = match op {
            SAMPLE | SAMPLE_B | SAMPLE_L | SAMPLE_D => {
                let c = self.src(&o[1], F, &[0, 1])?;
                let e = match op {
                    SAMPLE => match &off {
                        None => format!("texture({s}, {c})"),
                        Some(f) => format!("textureOffset({s}, {c}, {f})"),
                    },
                    SAMPLE_B => {
                        let b = self.src(&o[4], F, &[0])?;
                        match &off {
                            None => format!("texture({s}, {c}, {b})"),
                            Some(f) => format!("textureOffset({s}, {c}, {f}, {b})"),
                        }
                    }
                    SAMPLE_L => {
                        let l = self.src(&o[4], F, &[0])?;
                        match &off {
                            None => format!("textureLod({s}, {c}, {l})"),
                            Some(f) => format!("textureLodOffset({s}, {c}, {l}, {f})"),
                        }
                    }
                    _ => {
                        let (dx, dy) = (self.src(&o[4], F, &[0, 1])?, self.src(&o[5], F, &[0, 1])?);
                        match &off {
                            None => format!("textureGrad({s}, {c}, {dx}, {dy})"),
                            Some(f) => format!("textureGradOffset({s}, {c}, {dx}, {dy}, {f})"),
                        }
                    }
                };
                (e, rty, false)
            }
            SAMPLE_C | SAMPLE_C_LZ => {
                let c = self.src(&o[1], F, &[0, 1])?;
                let r = self.src(&o[4], F, &[0])?;
                let p = format!("vec3({c}, {r})");
                let e = match (op == SAMPLE_C, &off) {
                    (true, None) => format!("texture({s}, {p})"),
                    (true, Some(f)) => format!("textureOffset({s}, {p}, {f})"),
                    (false, None) => format!("textureLod({s}, {p}, 0.0)"),
                    (false, Some(f)) => format!("textureLodOffset({s}, {p}, 0.0, {f})"),
                };
                (e, F, true)
            }
            LD => {
                if self.shadow.contains(&t) {
                    return Err(format!("ld from t{t}, which is also sampled with comparison"));
                }
                let (c, l) = (self.src(&o[1], I, &[0, 1])?, self.src(&o[1], I, &[3])?);
                let e = match &off {
                    None => format!("texelFetch({s}, {c}, {l})"),
                    Some(f) => format!("texelFetchOffset({s}, {c}, {l}, {f})"),
                };
                (e, rty, false)
            }
            _ => {
                // resinfo: width, height, 0 (no depth or array size), mip levels (one:
                // ES 3.00 cannot query the count)
                let l = self.src(&o[1], I, &[0])?;
                let sz = format!("textureSize({s}, {l})");
                match (ins.tok >> 11) & 3 {
                    0 => (format!("vec4(vec2({sz}), 0.0, 1.0)"), F, false),
                    1 => (format!("vec4(1.0 / vec2({sz}), 0.0, 1.0)"), F, false),
                    _ => (format!("uvec4(uvec2({sz}), 0u, 1u)"), U, false),
                }
            }
        };
        let n = pos.len();
        let v = if scalar {
            if n == 1 { v } else { format!("vec{n}({v})") }
        } else {
            let sw: Vec<usize> = pos.iter().map(|&p| res.swz[p] as usize).collect();
            let sw = comps_str(&sw);
            if sw == "xyzw" { v } else { format!("{v}.{sw}") }
        };
        self.store(ins, &name, &pos, ty, v);
        Ok(())
    }
}

/// Translates a DXBC container (vertex or pixel shader).
pub fn translate(dxbc: &[u8]) -> Result<Shader, String> {
    let rd = |o: usize| dxbc.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])).ok_or_else(|| "truncated DXBC container".to_string());
    if dxbc.get(..4) != Some(b"DXBC") {
        return Err("not a DXBC container".into());
    }
    let (mut inputs, mut outputs, mut code) = (Vec::new(), Vec::new(), None);
    for k in 0..rd(28)? as usize {
        let off = rd(32 + 4 * k)? as usize;
        let size = rd(off + 4)? as usize;
        let data = dxbc.get(off + 8..off + 8 + size).ok_or("truncated DXBC chunk")?;
        match &dxbc[off..off + 4] {
            b"ISGN" => inputs = parse_sig(data, 24)?,
            b"ISG1" => inputs = parse_sig(data, 32)?,
            b"OSGN" => outputs = parse_sig(data, 24)?,
            b"OSG5" => outputs = parse_sig(data, 28)?,
            b"OSG1" => outputs = parse_sig(data, 32)?,
            b"SHDR" | b"SHEX" => code = Some(data),
            _ => {}
        }
    }
    let code = code.ok_or("no SHDR/SHEX chunk")?;
    // Output signatures store no system value for these (reflection reports D3D_NAME's)
    for (e, _) in outputs.iter_mut().filter(|(e, _)| e.sysval == 0) {
        e.sysval = match e.name.to_ascii_uppercase().as_str() {
            "SV_TARGET" => 64,
            "SV_DEPTH" => 65,
            "SV_COVERAGE" => 66,
            "SV_DEPTHGREATEREQUAL" => 67,
            "SV_DEPTHLESSEQUAL" => 68,
            _ => 0,
        };
    }
    let tokens: Vec<u32> = code.chunks_exact(4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
    let (vertex, instrs) = parse(&tokens)?;
    let mut g = Gen {
        vertex,
        cbs: BTreeMap::new(),
        textures: BTreeMap::new(),
        shadow: BTreeSet::new(),
        samples: Vec::new(),
        temps: BTreeSet::new(),
        indexable: BTreeMap::new(),
        icb: Vec::new(),
        inputs: BTreeSet::new(),
        outputs: BTreeSet::new(),
        uses_depth: false,
        body: String::new(),
        indent: 1,
    };

    // Declarations first: the code refers to them.
    let mut cb_sizes = BTreeMap::new();
    // input/output register -> components declared
    let mut in_decl: BTreeMap<u32, u8> = BTreeMap::new();
    let mut out_decl: BTreeMap<u32, u8> = BTreeMap::new();
    let mut plain = BTreeSet::new();
    let reg_of = |o: &Operand| o.idx.first().map_or(0, |i| i.imm);
    for ins in &instrs {
        let o0 = ins.ops.first();
        match ins.op {
            DCL_TEMPS => g.temps.extend(0..ins.extra.first().copied().unwrap_or(0)),
            DCL_INDEXABLE_TEMP => {
                let e = ins.extra.get(..2).ok_or("short dcl_indexableTemp")?;
                g.indexable.insert(e[0], e[1]);
            }
            DCL_CONSTANT_BUFFER => {
                let o = o0.filter(|o| o.idx.len() >= 2).ok_or("bad dcl_constantbuffer")?;
                cb_sizes.insert(o.idx[0].imm, o.idx[1].imm);
            }
            DCL_RESOURCE => {
                let t = reg_of(o0.ok_or("bad dcl_resource")?);
                let dim = (ins.tok >> 11) & 0x1F;
                if dim != 3 {
                    let names = ["unknown", "buffer", "texture1d", "texture2d", "texture2dms", "texture3d", "texturecube", "texture1darray", "texture2darray", "texture2dmsarray", "texturecubearray"];
                    return Err(format!("t{t}: {} resources are not supported", names.get(dim as usize).copied().unwrap_or("unknown")));
                }
                let ty = match ins.extra.first().map_or(5, |r| r & 0xF) {
                    3 => Ty::I,
                    4 => Ty::U,
                    _ => Ty::F,
                };
                g.textures.insert(t, ty);
            }
            DCL_INPUT..=DCL_INPUT_PS_SIV => {
                let o = o0.ok_or("bad input declaration")?;
                if o.ty != INPUT {
                    return Err(format!("{} inputs are not supported", operand_name(o.ty)));
                }
                *in_decl.entry(reg_of(o)).or_default() |= if o.comps == 1 { 1 } else { o.mask };
            }
            DCL_OUTPUT..=DCL_OUTPUT_SIV => {
                let o = o0.ok_or("bad output declaration")?;
                match o.ty {
                    OUTPUT => *out_decl.entry(reg_of(o)).or_default() |= if o.comps == 1 { 1 } else { o.mask },
                    OUTPUT_DEPTH | OUTPUT_DEPTH_GREATER_EQUAL | OUTPUT_DEPTH_LESS_EQUAL => g.uses_depth = true,
                    OUTPUT_COVERAGE_MASK => {}
                    t => return Err(format!("{} outputs are not supported", operand_name(t))),
                }
            }
            // dcl_immediateConstantBuffer
            CUSTOMDATA if ins.tok >> 11 == 3 => g.icb = ins.extra.clone(),
            SAMPLE_C | SAMPLE_C_LZ if ins.ops.len() > 2 => {
                g.shadow.insert(reg_of(&ins.ops[2]));
            }
            SAMPLE | SAMPLE_B | SAMPLE_L | SAMPLE_D | LD if ins.ops.len() > 2 => {
                plain.insert(reg_of(&ins.ops[2]));
            }
            _ => {}
        }
    }
    if let Some(t) = g.shadow.intersection(&plain).next() {
        return Err(format!("t{t} is sampled both with and without comparison"));
    }
    if let Some(t) = g.shadow.iter().find(|t| g.textures.get(t).is_some_and(|&ty| ty != Ty::F)) {
        return Err(format!("t{t}: comparison sampling of an integer texture"));
    }
    let (cname, limit) = if vertex { ("vc", 256) } else { ("pc", 224) };
    let mut cbuffers = Vec::new();
    let mut base = 0;
    for (&slot, &size) in &cb_sizes {
        g.cbs.insert(slot, (base, size));
        cbuffers.push((slot, base, size));
        base = base.checked_add(size).filter(|&b| b <= limit).ok_or_else(|| format!("the constant buffers need more than {limit} vec4s"))?;
    }

    // Code: the main program, then subroutines (label .. ret).
    let labels: Vec<usize> = instrs.iter().enumerate().filter(|(_, i)| i.op == LABEL).map(|(k, _)| k).collect();
    let mut funcs = vec![("shader_main".to_string(), 0, labels.first().copied().unwrap_or(instrs.len()))];
    for (n, &k) in labels.iter().enumerate() {
        let l = instrs[k].ops.first().map_or(0, Gen::label);
        funcs.push((format!("l{l}"), k + 1, labels.get(n + 1).copied().unwrap_or(instrs.len())));
    }
    let mut code_out = String::new();
    for (name, from, to) in &funcs {
        let mut body = &instrs[*from..*to];
        if body.last().is_some_and(|i| i.op == RET) {
            body = &body[..body.len() - 1];
        }
        g.indent = 1;
        for ins in body {
            g.instr(ins)?;
        }
        let b = std::mem::take(&mut g.body);
        let _ = writeln!(code_out, "void {name}() {{\n{b}}}");
    }

    // Stage inputs and outputs from the signatures, for the registers declared.
    let mut decl = String::new();
    let mut init = Vec::new();
    let mut epilogue = Vec::new();
    let elems_in = |sig: &[(SigElem, u32)], reg: u32, mask: u8| -> Vec<(SigElem, u32)> {
        let mut v: Vec<(SigElem, u32)> = sig.iter().filter(|(e, _)| e.register == reg && e.mask & mask != 0).cloned().collect();
        v.sort_by_key(|(e, _)| e.mask.trailing_zeros());
        v
    };
    if vertex {
        decl.push_str("uniform int u_bgra;\n");
        for (&reg, &mask) in &in_decl {
            g.inputs.insert(reg);
            let elems = elems_in(&inputs, reg, mask);
            if elems.is_empty() {
                return Err(format!("v{reg} is not in the input signature"));
            }
            for (e, ct) in elems {
                let val = match e.sysval {
                    0 => {
                        let usage = USAGES.iter().find(|u| u.eq_ignore_ascii_case(&e.name)).ok_or_else(|| format!("vertex input semantic {} has no Direct3D 9 usage", e.name))?;
                        let a = format!("a_{usage}{}", e.index);
                        let _ = writeln!(decl, "in vec4 {a}; // v{reg}");
                        // integer elements reach the shader as values, Direct3D passes bits
                        match ct {
                            1 => format!("uintBitsToFloat(uvec4({a}))"),
                            2 => format!("intBitsToFloat(ivec4({a}))"),
                            _ => format!("(((u_bgra >> {reg}) & 1) != 0 ? {a}.zyxw : {a})"),
                        }
                    }
                    6 => "vec4(intBitsToFloat(gl_VertexID), 0.0, 0.0, 0.0)".into(),
                    8 => "vec4(intBitsToFloat(gl_InstanceID), 0.0, 0.0, 0.0)".into(),
                    s => return Err(format!("vertex input {} (system value {s}) is not supported", e.name)),
                };
                init.push(read_elem(&format!("v{reg}"), e.mask, &val));
            }
        }
        let mut position = None;
        for (e, ct) in &outputs {
            if out_decl.get(&e.register).is_none_or(|&m| m & e.mask == 0) {
                continue;
            }
            g.outputs.insert(e.register);
            match e.sysval {
                1 => position = Some(e.register),
                0 => {
                    let v = varying_name(e)?;
                    let _ = writeln!(decl, "out vec4 v_{v};");
                    let val = write_elem(&format!("o{}", e.register), e.mask);
                    // integer varyings travel as values (interpolating bits would break them)
                    let val = match ct {
                        1 => format!("vec4(floatBitsToUint({val}))"),
                        2 => format!("vec4(floatBitsToInt({val}))"),
                        _ => val,
                    };
                    epilogue.push(format!("v_{v} = {val};"));
                }
                // clip and cull distances, render target and viewport indices: WebGL has none
                _ => {}
            }
        }
        decl.push_str("// @varyings\n");
        if let Some(r) = position {
            epilogue.push(format!("gl_Position = vec4(o{r}.x, -o{r}.y, 2.0 * o{r}.z - o{r}.w, o{r}.w);"));
        }
    } else {
        decl.push_str("uniform int u_atest;\nuniform float u_aref;\n");
        for (&reg, &mask) in &in_decl {
            g.inputs.insert(reg);
            let elems = elems_in(&inputs, reg, mask);
            if elems.is_empty() {
                return Err(format!("v{reg} is not in the input signature"));
            }
            for (e, ct) in elems {
                let val = match e.sysval {
                    0 => {
                        let v = varying_name(&e)?;
                        let _ = writeln!(decl, "in vec4 v_{v}; // v{reg}");
                        match ct {
                            1 => format!("uintBitsToFloat(uvec4(round(v_{v})))"),
                            2 => format!("intBitsToFloat(ivec4(round(v_{v})))"),
                            _ => format!("v_{v}"),
                        }
                    }
                    // Direct3D's w is the clip-space w, gl_FragCoord.w its reciprocal
                    1 => "vec4(gl_FragCoord.xyz, 1.0 / gl_FragCoord.w)".into(),
                    9 => "vec4(intBitsToFloat(gl_FrontFacing ? -1 : 0))".into(),
                    s => return Err(format!("pixel shader input {} (system value {s}) is not supported", e.name)),
                };
                init.push(read_elem(&format!("v{reg}"), e.mask, &val));
            }
        }
        decl.push_str("out vec4 o_frag;\n");
        g.outputs.extend(out_decl.keys());
        let target = outputs.iter().find(|(e, _)| e.sysval == 64 && e.index == 0).map_or(0, |(e, _)| e.register);
        let color = if g.outputs.contains(&target) { format!("o{target}") } else { "vec4(0.0)".into() };
        epilogue.push(format!("if (!atest9({color}.a)) discard;"));
        if g.uses_depth {
            epilogue.push("gl_FragDepth = clamp(oDepth.x, 0.0, 1.0);".into());
        }
        epilogue.push(format!("o_frag = {color};"));
    }

    let mut out = String::new();
    out.push_str("#version 300 es\nprecision highp float;\nprecision highp int;\n");
    if !g.cbs.is_empty() {
        let _ = writeln!(out, "uniform vec4 {cname}[{limit}];");
    }
    for (&t, &ty) in &g.textures {
        let kind = match ty {
            _ if g.shadow.contains(&t) => "sampler2DShadow",
            Ty::I => "isampler2D",
            Ty::U => "usampler2D",
            Ty::F => "sampler2D",
        };
        let _ = writeln!(out, "uniform highp {kind} s{t};");
    }
    out.push_str(&decl);
    for &r in &g.temps {
        let _ = writeln!(out, "vec4 r{r} = vec4(0.0);");
    }
    for (&x, &n) in &g.indexable {
        let _ = writeln!(out, "vec4 x{x}[{n}];");
    }
    if !g.icb.is_empty() {
        let rows: Vec<String> = g.icb.chunks(4).map(|c| format!("ivec4({})", (0..4).map(|k| int_lit(c.get(k).copied().unwrap_or(0) as i32)).collect::<Vec<_>>().join(", "))).collect();
        let _ = writeln!(out, "const ivec4 icb[{n}] = ivec4[{n}](\n  {});", rows.join(",\n  "), n = rows.len());
    }
    for &r in &g.inputs {
        let _ = writeln!(out, "vec4 v{r} = vec4(0.0);");
    }
    for &r in &g.outputs {
        let _ = writeln!(out, "vec4 o{r} = vec4(0.0);");
    }
    if g.uses_depth {
        out.push_str("vec4 oDepth = vec4(0.0);\n");
    }
    if !vertex {
        // the host's alpha test (as for Direct3D 9; Direct3D 11 hosts leave it off)
        out.push_str("bool atest9(float a) {\n  float v = floor(clamp(a, 0.0, 1.0) * 255.0 + 0.5);\n  if (u_atest == 1) return false; if (u_atest == 2) return v < u_aref; if (u_atest == 3) return v == u_aref;\n  if (u_atest == 4) return v <= u_aref; if (u_atest == 5) return v > u_aref; if (u_atest == 6) return v != u_aref;\n  if (u_atest == 7) return v >= u_aref; return true;\n}\n");
    }
    for (name, ..) in &funcs[1..] {
        let _ = writeln!(out, "void {name}();");
    }
    out.push_str(&code_out);
    out.push_str("void main() {\n");
    if vertex {
        out.push_str("  // @init\n");
    }
    for l in &init {
        let _ = writeln!(out, "  {l}");
    }
    out.push_str("  shader_main();\n");
    for l in &epilogue {
        let _ = writeln!(out, "  {l}");
    }
    out.push_str("}\n");
    Ok(Shader {
        vertex,
        glsl: out,
        inputs: inputs.into_iter().map(|(e, _)| e).collect(),
        outputs: outputs.into_iter().map(|(e, _)| e).collect(),
        cbuffers,
        samples: g.samples,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A DXBC container of (fourcc, data) chunks (the checksum is not checked).
    fn container(chunks: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        let mut out = b"DXBC".to_vec();
        out.extend([0; 16]);
        out.extend(1u32.to_le_bytes());
        let total = 32 + 4 * chunks.len() + chunks.iter().map(|(_, d)| 8 + d.len()).sum::<usize>();
        out.extend((total as u32).to_le_bytes());
        out.extend((chunks.len() as u32).to_le_bytes());
        let mut at = 32 + 4 * chunks.len();
        for (_, d) in chunks {
            out.extend((at as u32).to_le_bytes());
            at += 8 + d.len();
        }
        for (cc, d) in chunks {
            out.extend(*cc);
            out.extend((d.len() as u32).to_le_bytes());
            out.extend(d);
        }
        out
    }

    /// An ISGN/OSGN chunk of (name, index, system value, register, mask) float elements.
    fn sig(elems: &[(&str, u32, u32, u32, u8)]) -> Vec<u8> {
        sig_typed(&elems.iter().map(|&(n, i, sv, r, m)| (n, i, sv, r, m, 3)).collect::<Vec<_>>())
    }

    /// The same with component types (1 uint, 2 sint, 3 float).
    fn sig_typed(elems: &[(&str, u32, u32, u32, u8, u32)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut names = Vec::new();
        out.extend((elems.len() as u32).to_le_bytes());
        out.extend(8u32.to_le_bytes());
        for &(name, index, sv, reg, mask, ct) in elems {
            let at = 8 + 24 * elems.len() + names.len();
            names.extend(name.as_bytes());
            names.push(0);
            for w in [at as u32, index, sv, ct, reg, mask as u32 | (mask as u32) << 8] {
                out.extend(w.to_le_bytes());
            }
        }
        out.extend(names);
        out
    }

    /// A SHDR chunk: program type (0 pixel, 1 vertex), shader model 4.0.
    fn shdr(ty: u32, body: &[u32]) -> Vec<u8> {
        let mut t = vec![ty << 16 | 0x40, body.len() as u32 + 2];
        t.extend(body);
        t.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    #[test]
    fn vertex_shader() {
        let code = [
            0x0400_0059, 0x0020_8E46, 0, 1, // dcl_constantbuffer cb0[1], immediateIndexed
            0x0300_005F, 0x0010_10F2, 0, // dcl_input v0.xyzw
            0x0300_005F, 0x0010_10F2, 1, // dcl_input v1.xyzw
            0x0400_0067, 0x0010_20F2, 0, 1, // dcl_output_siv o0.xyzw, position
            0x0300_0065, 0x0010_20F2, 1, // dcl_output o1.xyzw
            0x0500_0036, 0x0010_20F2, 0, 0x0010_1E46, 0, // mov o0.xyzw, v0.xyzw
            0x0800_0038, 0x0010_20F2, 1, 0x0010_1E46, 1, 0x0020_8E46, 0, 0, // mul o1.xyzw, v1.xyzw, cb0[0].xyzw
            0x0100_003E, // ret
        ];
        let dxbc = container(&[
            (b"ISGN", sig(&[("POSITION", 0, 0, 0, 0xF), ("color", 0, 0, 1, 0xF)])),
            (b"OSGN", sig(&[("SV_Position", 0, 1, 0, 0xF), ("COLOR", 0, 0, 1, 0xF)])),
            (b"SHDR", shdr(1, &code)),
        ]);
        let s = translate(&dxbc).unwrap();
        let g = &s.glsl;
        assert!(s.vertex);
        assert!(g.starts_with("#version 300 es\n"), "{g}");
        for want in [
            "uniform vec4 vc[256];",
            "uniform int u_bgra;",
            "in vec4 a_POSITION0; // v0",
            "in vec4 a_COLOR0; // v1",
            "out vec4 v_COLOR0;",
            "// @varyings",
            "// @init",
            "v1 = (((u_bgra >> 1) & 1) != 0 ? a_COLOR0.zyxw : a_COLOR0);",
            "o0 = v0;",
            "o1 = v1 * vc[0];",
            "v_COLOR0 = o1;",
            "gl_Position = vec4(o0.x, -o0.y, 2.0 * o0.z - o0.w, o0.w);",
        ] {
            assert!(g.contains(want), "{want}:\n{g}");
        }
        assert_eq!(s.cbuffers, vec![(0, 0, 1)]);
        assert_eq!(s.inputs[1].name, "color");
    }

    #[test]
    fn pixel_shader() {
        let code = [
            0x0400_0059, 0x0020_8E46, 0, 1, // dcl_constantbuffer cb0[1], immediateIndexed
            0x0300_005A, 0x0010_6000, 0, // dcl_sampler s0, mode_default
            0x0400_1858, 0x0010_7000, 0, 0x5555, // dcl_resource_texture2d (float,float,float,float) t0
            0x0300_1062, 0x0010_1032, 1, // dcl_input_ps linear v1.xy
            0x0300_0065, 0x0010_20F2, 0, // dcl_output o0.xyzw
            0x0200_0068, 1, // dcl_temps 1
            0x0900_0045, 0x0010_00F2, 0, 0x0010_1046, 1, 0x0010_7E46, 0, 0x0010_6000, 0, // sample r0.xyzw, v1.xyxx, t0.xyzw, s0
            0x0800_0038, 0x0010_20F2, 0, 0x0010_0E46, 0, 0x0020_8E46, 0, 0, // mul o0.xyzw, r0.xyzw, cb0[0].xyzw
            0x0100_003E, // ret
        ];
        let dxbc = container(&[
            (b"ISGN", sig(&[("SV_Position", 0, 1, 0, 0xF), ("TEXCOORD", 0, 0, 1, 0x3)])),
            (b"OSGN", sig(&[("SV_Target", 0, 0, 0, 0xF)])),
            (b"SHDR", shdr(0, &code)),
        ]);
        let s = translate(&dxbc).unwrap();
        let g = &s.glsl;
        assert!(!s.vertex);
        for want in [
            "uniform vec4 pc[224];",
            "uniform highp sampler2D s0;",
            "uniform int u_atest;",
            "uniform float u_aref;",
            "in vec4 v_TEXCOORD0; // v1",
            "out vec4 o_frag;",
            "v1 = v_TEXCOORD0;",
            "r0 = texture(s0, v1.xy);",
            "o0 = r0 * pc[0];",
            "if (!atest9(o0.a)) discard;",
            "o_frag = o0;",
        ] {
            assert!(g.contains(want), "{want}:\n{g}");
        }
        assert!(!g.contains("gl_FragCoord"), "{g}");
        assert_eq!(s.samples, vec![(0, 0)]);
        assert_eq!(s.cbuffers, vec![(0, 0, 1)]);
        assert_eq!(s.outputs[0].sysval, 64);
    }

    // A small assembler for the instruction stream.
    const R: u32 = TEMP;
    const V: u32 = INPUT;
    const O: u32 = OUTPUT;
    const NZ: u32 = 1 << 7;
    const SAT: u32 = 1 << 2;

    fn comp(c: char) -> u32 {
        "xyzw".find(c).unwrap() as u32
    }

    /// An instruction: opcode, controls (bits 11..), operands.
    fn ins(op: u32, ctl: u32, ops: &[Vec<u32>]) -> Vec<u32> {
        let body = ops.concat();
        let mut v = vec![op | ctl << 11 | (body.len() as u32 + 1) << 24];
        v.extend(body);
        v
    }

    /// The same with texel offsets (aoffimmi).
    fn ins_off(op: u32, (u, v): (i32, i32), ops: &[Vec<u32>]) -> Vec<u32> {
        let mut t = ins(op, 0, ops);
        t[0] = (t[0] + (1 << 24)) | 1 << 31;
        t.insert(1, 1 | ((u as u32) & 0xF) << 9 | ((v as u32) & 0xF) << 13);
        t
    }

    /// A destination register with its write mask ("xz").
    fn d(ty: u32, n: u32, mask: &str) -> Vec<u32> {
        let m: u32 = mask.chars().map(|c| 1 << comp(c)).sum();
        vec![2 | m << 4 | ty << 12 | 1 << 20, n]
    }

    fn swz_bits(sw: &str) -> u32 {
        if sw.len() == 1 { 2 << 2 | comp(sw.chars().next().unwrap()) << 4 } else { 1 << 2 | sw.chars().enumerate().map(|(k, c)| comp(c) << (2 * k)).sum::<u32>() << 4 }
    }

    /// A source register with a swizzle ("xyzw"; one letter: select1).
    fn s(ty: u32, n: u32, sw: &str) -> Vec<u32> {
        vec![2 | swz_bits(sw) | ty << 12 | 1 << 20, n]
    }

    fn modified(mut o: Vec<u32>, m: u32) -> Vec<u32> {
        o[0] |= 1 << 31;
        o.insert(1, 1 | m << 6);
        o
    }

    fn imm(v: &[u32]) -> Vec<u32> {
        let mut t = vec![if v.len() == 1 { 1 } else { 2 } | IMMEDIATE32 << 12];
        t.extend(v);
        t
    }

    fn fl(v: &[f32]) -> Vec<u32> {
        imm(&v.iter().map(|f| f.to_bits()).collect::<Vec<_>>())
    }

    fn cb(slot: u32, i: u32, sw: &str) -> Vec<u32> {
        vec![2 | swz_bits(sw) | CONSTANT_BUFFER << 12 | 2 << 20, slot, i]
    }

    /// cb#[rel + i], x#[rel + i] (2D) or icb[rel + i] (1D).
    fn rel(ty: u32, n: u32, i: u32, r: Vec<u32>, sw: &str, dst: bool) -> Vec<u32> {
        let sel = if dst { 2 | sw.chars().map(|c| 1 << comp(c)).sum::<u32>() << 4 } else { 2 | swz_bits(sw) };
        let mut t = if ty == IMMEDIATE_CONSTANT_BUFFER { vec![sel | ty << 12 | 1 << 20 | 3 << 22, i] } else { vec![sel | ty << 12 | 2 << 20 | 3 << 25, n, i] };
        t.extend(r);
        t
    }

    /// A 0-component operand (sampler 6, label 10, null 13).
    fn bare(ty: u32, n: u32) -> Vec<u32> {
        if ty == NULL { vec![ty << 12] } else { vec![ty << 12 | 1 << 20, n] }
    }

    /// A vertex shader using most of the integer and flow control instructions.
    fn kitchen_vertex() -> Vec<u8> {
        let null = || bare(NULL, 0);
        let rs = |sw: &str| s(R, 3, sw);
        let code = [
            ins(DCL_CONSTANT_BUFFER, 1, &[cb(0, 8, "xyzw")]),
            ins(DCL_CONSTANT_BUFFER, 0, &[cb(2, 4, "xyzw")]),
            ins(90, 0, &[bare(6, 1)]),
            ins(DCL_RESOURCE, 3, &[bare(7, 3), vec![0x5555]]),
            ins(DCL_INPUT, 0, &[d(V, 0, "xyzw")]),
            ins(DCL_INPUT, 0, &[d(V, 1, "xyzw")]),
            ins(DCL_INPUT_SGV, 0, &[d(V, 2, "x"), vec![6]]),
            ins(DCL_OUTPUT_SIV, 0, &[d(O, 0, "xyzw"), vec![1]]),
            ins(DCL_OUTPUT, 0, &[d(O, 1, "xyzw")]),
            ins(DCL_OUTPUT, 0, &[d(O, 2, "xy")]),
            ins(DCL_OUTPUT, 0, &[d(O, 2, "zw")]),
            ins(DCL_OUTPUT, 0, &[d(O, 3, "x")]),
            ins(DCL_TEMPS, 0, &[vec![4]]),
            ins(DCL_INDEXABLE_TEMP, 0, &[vec![0, 4, 4]]),
            {
                let data = [1.0f32.to_bits(), 2.0f32.to_bits(), 3.0f32.to_bits(), 4.0f32.to_bits(), 0, 1, 2, 0xFFFF_FFFF];
                let mut t = vec![CUSTOMDATA | 3 << 11, 2 + data.len() as u32];
                t.extend(data);
                t
            },
            ins(MOV, 0, &[d(R, 0, "xyzw"), s(V, 0, "xyzw")]),
            ins(IADD, 0, &[d(R, 1, "x"), s(V, 2, "x"), imm(&[1])]),
            ins(ISHL, 0, &[d(R, 1, "y"), s(R, 1, "x"), imm(&[2])]),
            ins(USHR, 0, &[d(R, 1, "z"), s(R, 1, "y"), imm(&[1])]),
            ins(AND, 0, &[d(R, 1, "w"), s(R, 1, "z"), imm(&[0xFFFF])]),
            ins(ISHR, 0, &[d(R, 1, "zw"), s(R, 1, "zwzz"), imm(&[1, 2, 3, 4])]),
            ins(FTOI, 0, &[d(R, 2, "x"), s(V, 0, "x")]),
            ins(ITOF, 0, &[d(R, 2, "y"), s(R, 2, "x")]),
            ins(UTOF, 0, &[d(R, 2, "z"), s(V, 1, "x")]),
            ins(FTOU, 0, &[d(R, 2, "w"), s(V, 0, "y")]),
            ins(IMUL, 0, &[null(), d(R, 3, "x"), s(R, 1, "x"), s(R, 1, "y")]),
            ins(UMUL, 0, &[null(), d(R, 3, "y"), s(R, 1, "x"), imm(&[7])]),
            ins(UDIV, 0, &[d(R, 3, "y"), d(R, 3, "z"), s(R, 1, "y"), imm(&[3])]),
            ins(IMAD, 0, &[d(R, 3, "w"), s(R, 1, "x"), imm(&[3]), modified(s(R, 1, "y"), 1)]),
            ins(UMAD, 0, &[d(R, 3, "w"), rs("w"), imm(&[3]), s(R, 1, "y")]),
            ins(IEQ, 0, &[d(R, 2, "xy"), s(R, 1, "xyxx"), imm(&[2, 3, 0, 0])]),
            ins(INE, 0, &[d(R, 2, "x"), s(R, 1, "x"), imm(&[0x8000_0000])]),
            ins(ILT, 0, &[d(R, 2, "x"), s(R, 1, "x"), imm(&[0xFFFF_FFFB])]),
            ins(IGE, 0, &[d(R, 2, "x"), s(R, 1, "x"), s(R, 1, "y")]),
            ins(ULT, 0, &[d(R, 2, "x"), s(R, 1, "x"), s(R, 1, "y")]),
            ins(UGE, 0, &[d(R, 2, "x"), s(R, 1, "x"), s(R, 1, "y")]),
            ins(IMIN, 0, &[d(R, 2, "xy"), s(R, 1, "xyxx"), s(R, 1, "zwzz")]),
            ins(IMAX, 0, &[d(R, 2, "x"), s(R, 1, "x"), s(R, 1, "y")]),
            ins(UMIN, 0, &[d(R, 2, "x"), s(R, 1, "x"), s(R, 1, "y")]),
            ins(UMAX, 0, &[d(R, 2, "x"), s(R, 1, "x"), s(R, 1, "y")]),
            ins(NOT, 0, &[d(R, 2, "x"), s(R, 1, "x")]),
            ins(INEG, 0, &[d(R, 2, "x"), s(R, 1, "x")]),
            ins(XOR, 0, &[d(R, 2, "x"), s(R, 1, "x"), s(R, 1, "y")]),
            ins(OR, 0, &[d(R, 2, "x"), s(R, 1, "x"), imm(&[0x3F80_0000])]),
            ins(MOVC, 0, &[d(R, 0, "xy"), s(R, 2, "xyxx"), s(R, 0, "xyxx"), fl(&[0.0, 1.0, 0.0, 0.0])]),
            ins(MOVC, SAT, &[d(R, 0, "zw"), s(R, 2, "xxxx"), s(R, 0, "zwzz"), modified(s(R, 1, "xyxx"), 2)]),
            ins(SWAPC, 0, &[d(R, 2, "xy"), d(R, 3, "xy"), s(R, 2, "zwzz"), s(R, 3, "xyxx"), s(R, 2, "xyxx")]),
            ins(SINCOS, 0, &[d(R, 2, "x"), d(R, 2, "y"), s(R, 2, "x")]),
            ins(SINCOS, 0, &[null(), d(R, 2, "zw"), s(R, 0, "xyxx")]),
            ins(DP2, 0, &[d(R, 2, "z"), s(R, 0, "xyxx"), s(R, 0, "zwzz")]),
            ins(DP3, SAT, &[d(R, 2, "zw"), s(R, 0, "xyzx"), s(R, 0, "zwzz")]),
            ins(DP4, 0, &[d(R, 2, "w"), s(R, 0, "xyzw"), cb(2, 1, "xyzw")]),
            ins(ROUND_NE, 0, &[d(R, 0, "x"), s(R, 0, "x")]),
            ins(ROUND_NI, 0, &[d(R, 0, "x"), s(R, 0, "x")]),
            ins(ROUND_PI, 0, &[d(R, 0, "x"), s(R, 0, "x")]),
            ins(ROUND_Z, 0, &[d(R, 0, "x"), s(R, 0, "x")]),
            ins(RSQ, 0, &[d(R, 0, "y"), modified(s(R, 0, "y"), 2)]),
            ins(SQRT, 0, &[d(R, 0, "y"), s(R, 0, "y")]),
            ins(EXP, 0, &[d(R, 0, "y"), s(R, 0, "y")]),
            ins(LOG, 0, &[d(R, 0, "y"), s(R, 0, "y")]),
            ins(FRC, 0, &[d(R, 0, "y"), s(R, 0, "y")]),
            ins(RCP, 0, &[d(R, 0, "yz"), s(R, 0, "yzyy")]),
            ins(DIV, 0, &[d(R, 0, "y"), s(R, 0, "y"), fl(&[3.0])]),
            ins(MIN, 0, &[d(R, 0, "y"), s(R, 0, "y"), fl(&[-3.0])]),
            ins(MAX, 0, &[d(R, 0, "y"), s(R, 0, "y"), modified(s(R, 0, "z"), 3)]),
            ins(MAD, SAT, &[d(R, 0, "y"), s(R, 0, "y"), fl(&[2.5]), cb(0, 3, "x")]),
            ins(EQ, 0, &[d(R, 3, "x"), s(R, 0, "y"), fl(&[1.0])]),
            ins(NE, 0, &[d(R, 3, "xy"), s(R, 0, "yzyy"), fl(&[1.0, 2.0, 0.0, 0.0])]),
            ins(LOOP, 0, &[]),
            ins(BREAKC, NZ, &[s(R, 3, "x")]),
            ins(IADD, 0, &[d(R, 3, "x"), s(R, 3, "x"), imm(&[0xFFFF_FFFF])]),
            ins(IF, 0, &[s(R, 3, "y")]),
            ins(CONTINUEC, NZ, &[s(R, 3, "z")]),
            ins(ELSE, 0, &[]),
            ins(MOV, 0, &[rel(INDEXABLE_TEMP, 0, 0, s(R, 3, "x"), "xyzw", true), s(R, 0, "xyzw")]),
            ins(MOV, 0, &[d(R, 0, "w"), rel(INDEXABLE_TEMP, 0, 1, s(R, 3, "y"), "z", false)]),
            ins(ENDIF, 0, &[]),
            ins(SWITCH, 0, &[s(R, 3, "y")]),
            ins(CASE, 0, &[imm(&[0])]),
            ins(CASE, 0, &[imm(&[0xFFFF_FFFF])]),
            ins(MOV, 0, &[d(R, 0, "x"), rel(IMMEDIATE_CONSTANT_BUFFER, 0, 1, s(R, 3, "x"), "x", false)]),
            ins(IADD, 0, &[d(R, 0, "y"), rel(IMMEDIATE_CONSTANT_BUFFER, 0, 0, s(R, 3, "x"), "y", false), imm(&[1])]),
            ins(BREAK, 0, &[]),
            ins(DEFAULT, 0, &[]),
            ins(MOV, 0, &[d(R, 0, "y"), rel(CONSTANT_BUFFER, 0, 2, s(R, 3, "x"), "y", false)]),
            ins(BREAK, 0, &[]),
            ins(ENDSWITCH, 0, &[]),
            ins(CONTINUE, 0, &[]),
            ins(ENDLOOP, 0, &[]),
            ins(SAMPLE_L, 0, &[d(R, 2, "xyzw"), s(R, 0, "xyxx"), s(7, 3, "xyzw"), bare(6, 1), fl(&[0.0])]),
            ins(LD, 0, &[d(R, 2, "xyzw"), s(R, 1, "xyzw"), s(7, 3, "wzyx")]),
            ins(RESINFO, 2, &[d(R, 3, "xy"), imm(&[0]), s(7, 3, "xyzw")]),
            ins(F32TOF16, 0, &[d(R, 3, "zw"), s(R, 0, "xyxx")]),
            ins(F16TOF32, 0, &[d(R, 3, "w"), s(R, 3, "z")]),
            ins(CALLC, NZ, &[s(R, 3, "x"), bare(10, 0)]),
            ins(CALL, 0, &[bare(10, 1)]),
            ins(MOV, 0, &[d(O, 0, "xyzw"), s(R, 0, "xyzw")]),
            ins(MOV, 0, &[d(O, 1, "xyzw"), s(R, 2, "xyzw")]),
            ins(MOV, 0, &[d(O, 2, "xy"), s(R, 3, "xyxx")]),
            ins(MOV, 0, &[d(O, 2, "zw"), s(R, 3, "zzzw")]),
            ins(MOV, 0, &[d(O, 3, "x"), s(R, 1, "x")]),
            ins(RET, 0, &[]),
            ins(LABEL, 0, &[bare(10, 0)]),
            ins(MOV, 0, &[d(R, 1, "xyzw"), cb(2, 3, "xyzw")]),
            ins(RETC, 0, &[s(R, 1, "x")]),
            ins(CALL, 0, &[bare(10, 1)]),
            ins(RET, 0, &[]),
            ins(LABEL, 0, &[bare(10, 1)]),
            ins(MOV, 0, &[d(R, 1, "x"), fl(&[1.0])]),
            ins(RET, 0, &[]),
        ]
        .concat();
        container(&[
            (b"ISGN", sig_typed(&[("POSITION", 0, 0, 0, 0xF, 3), ("BLENDINDICES", 0, 0, 1, 0xF, 1), ("SV_VertexID", 0, 6, 2, 1, 1)])),
            (b"OSGN", sig_typed(&[("SV_Position", 0, 1, 0, 0xF, 3), ("TEXCOORD", 0, 0, 1, 0xF, 3), ("TEXCOORD", 1, 0, 2, 0x3, 3), ("TEXCOORD", 2, 0, 2, 0xC, 3), ("BLENDINDICES", 0, 0, 3, 1, 1)])),
            (b"SHDR", shdr(1, &code)),
        ])
    }

    /// A pixel shader using every sampling instruction and the system value inputs.
    fn kitchen_pixel() -> Vec<u8> {
        let t = |n: u32, sw: &str| s(7, n, sw);
        let code = [
            ins(DCL_CONSTANT_BUFFER, 0, &[cb(1, 2, "xyzw")]),
            ins(90, 0, &[bare(6, 0)]),
            ins(90, 1, &[bare(6, 1)]),
            ins(DCL_RESOURCE, 3, &[bare(7, 0), vec![0x5555]]),
            ins(DCL_RESOURCE, 3, &[bare(7, 1), vec![0x5555]]),
            ins(DCL_RESOURCE, 3, &[bare(7, 2), vec![0x4444]]),
            ins(DCL_INPUT_PS_SIV, 4, &[d(V, 0, "xyzw"), vec![1]]),
            ins(98, 2, &[d(V, 1, "xyzw")]),
            ins(98, 2, &[d(V, 2, "xy")]),
            ins(98, 2, &[d(V, 2, "zw")]),
            ins(98, 1, &[d(V, 3, "x")]),
            ins(DCL_INPUT_PS_SGV, 1, &[d(V, 4, "x"), vec![9]]),
            ins(DCL_OUTPUT, 0, &[d(O, 0, "xyzw")]),
            ins(DCL_OUTPUT, 0, &[d(O, 1, "xyzw")]),
            ins(DCL_OUTPUT, 0, &[vec![1 | OUTPUT_DEPTH << 12]]),
            ins(DCL_TEMPS, 0, &[vec![3]]),
            ins(SAMPLE, 0, &[d(R, 0, "xyzw"), s(V, 1, "xyxx"), t(0, "xyzw"), bare(6, 0)]),
            ins_off(SAMPLE, (1, -1), &[d(R, 0, "xy"), s(V, 1, "xyxx"), t(0, "zyxw"), bare(6, 0)]),
            ins(SAMPLE_B, 0, &[d(R, 1, "xyzw"), s(V, 1, "xyzw"), t(0, "xyzw"), bare(6, 0), fl(&[0.5])]),
            ins_off(SAMPLE_B, (-8, 7), &[d(R, 1, "xyzw"), s(V, 1, "xyzw"), t(0, "xyzw"), bare(6, 0), cb(1, 1, "x")]),
            ins(SAMPLE_D, 0, &[d(R, 1, "xyzw"), s(V, 1, "xyzw"), t(0, "xyzw"), bare(6, 0), s(V, 2, "xyxx"), s(V, 2, "zwzz")]),
            ins_off(SAMPLE_D, (1, 1), &[d(R, 1, "xyzw"), s(V, 1, "xyzw"), t(0, "xyzw"), bare(6, 0), s(V, 2, "xyxx"), s(V, 2, "zwzz")]),
            ins(SAMPLE_L, 0, &[d(R, 1, "xyzw"), s(V, 1, "xyzw"), t(0, "xyzw"), bare(6, 0), fl(&[1.0])]),
            ins_off(SAMPLE_L, (2, 0), &[d(R, 1, "w"), s(V, 1, "xyzw"), t(0, "xyzw"), bare(6, 0), fl(&[1.0])]),
            ins(SAMPLE_C, 0, &[d(R, 2, "x"), s(V, 1, "xyxx"), t(1, "xxxx"), bare(6, 1), s(V, 1, "z")]),
            ins_off(SAMPLE_C, (1, 0), &[d(R, 2, "xy"), s(V, 1, "xyxx"), t(1, "xxxx"), bare(6, 1), s(V, 1, "z")]),
            ins(SAMPLE_C_LZ, 0, &[d(R, 2, "y"), s(V, 1, "xyxx"), t(1, "xxxx"), bare(6, 1), s(V, 1, "w")]),
            ins_off(SAMPLE_C_LZ, (0, 1), &[d(R, 2, "y"), s(V, 1, "xyxx"), t(1, "xxxx"), bare(6, 1), s(V, 1, "w")]),
            ins(LD, 0, &[d(R, 1, "xyzw"), imm(&[0, 0, 0, 0]), t(2, "xyzw")]),
            ins_off(LD, (1, 2), &[d(R, 1, "xy"), s(V, 3, "xxxx"), t(2, "xyzw")]),
            ins(RESINFO, 0, &[d(R, 2, "zw"), imm(&[0]), t(0, "xxxy")]),
            ins(RESINFO, 1, &[d(R, 2, "zw"), imm(&[0]), t(1, "xxxy")]),
            ins(RESINFO, 2, &[d(R, 2, "zw"), imm(&[0]), t(2, "xxxy")]),
            ins(DERIV_RTX, 0, &[d(R, 1, "x"), s(V, 1, "x")]),
            ins(DERIV_RTY, 0, &[d(R, 1, "y"), s(V, 1, "y")]),
            ins(DERIV_RTX_FINE, 0, &[d(R, 1, "zw"), s(V, 0, "xyxx")]),
            ins(DERIV_RTY_COARSE, 0, &[d(R, 1, "zw"), s(V, 0, "xyxx")]),
            ins(LT, 0, &[d(R, 1, "z"), s(V, 1, "x"), fl(&[0.0])]),
            ins(GE, 0, &[d(R, 1, "w"), s(V, 1, "x"), modified(s(V, 0, "w"), 1)]),
            ins(DISCARD, NZ, &[s(R, 1, "z")]),
            ins(DISCARD, 0, &[s(V, 4, "x")]),
            ins(MOV, SAT, &[d(O, 0, "xyzw"), s(R, 0, "xyzw")]),
            ins(ADD, 0, &[d(O, 1, "xyzw"), s(R, 1, "xyzw"), cb(1, 0, "xyzw")]),
            ins(MUL, 0, &[d(O, 1, "x"), s(R, 2, "x"), s(V, 3, "x")]),
            ins(RETC, NZ, &[s(V, 4, "x")]),
            ins(MOV, 0, &[vec![1 | OUTPUT_DEPTH << 12], s(V, 0, "z")]),
            ins(RET, 0, &[]),
        ]
        .concat();
        container(&[
            (b"ISGN", sig_typed(&[("SV_Position", 0, 1, 0, 0xF, 3), ("TEXCOORD", 0, 0, 1, 0xF, 3), ("TEXCOORD", 1, 0, 2, 0x3, 3), ("TEXCOORD", 2, 0, 2, 0xC, 3), ("BLENDINDICES", 0, 0, 3, 1, 1), ("SV_IsFrontFace", 0, 9, 4, 1, 1)])),
            (b"OSGN", sig_typed(&[("SV_Target", 0, 0, 0, 0xF, 3), ("SV_Target", 1, 0, 1, 0xF, 3), ("SV_Depth", 0, 65, 0xFFFF_FFFF, 1, 3)])),
            (b"SHDR", shdr(0, &code)),
        ])
    }

    /// A pixel shader mixing float and integer work on the same registers, writing
    /// (1.0, 0.25, 0.5, 0.75) for cb0 = (1.0, 2.0, int 5, int -3), (0.25, 0.5, int 1, 0):
    /// rendered in a browser to check bits survive float registers and uniforms.
    fn bits_pixel() -> Vec<u8> {
        let code = [
            ins(DCL_CONSTANT_BUFFER, 0, &[cb(0, 2, "xyzw")]),
            ins(DCL_OUTPUT, 0, &[d(O, 0, "xyzw")]),
            ins(DCL_TEMPS, 0, &[vec![2]]),
            ins(LT, 0, &[d(R, 0, "x"), cb(0, 0, "x"), cb(0, 0, "y")]),
            ins(AND, 0, &[d(R, 1, "x"), s(R, 0, "x"), imm(&[0x3F80_0000])]),
            ins(INE, 0, &[d(R, 0, "y"), cb(0, 1, "z"), imm(&[0])]),
            ins(MOVC, 0, &[d(R, 1, "y"), s(R, 0, "y"), cb(0, 1, "x"), fl(&[0.0])]),
            ins(IADD, 0, &[d(R, 0, "z"), cb(0, 0, "z"), cb(0, 0, "w")]),
            ins(ITOF, 0, &[d(R, 0, "z"), s(R, 0, "z")]),
            ins(MUL, 0, &[d(R, 1, "z"), s(R, 0, "z"), fl(&[0.25])]),
            ins(ISHL, 0, &[d(R, 0, "w"), cb(0, 0, "z"), imm(&[1])]),
            ins(IEQ, 0, &[d(R, 0, "w"), s(R, 0, "w"), imm(&[10])]),
            ins(MOVC, 0, &[d(R, 1, "w"), s(R, 0, "w"), fl(&[0.75]), fl(&[0.0])]),
            ins(GE, 0, &[d(R, 0, "x"), cb(0, 0, "x"), cb(0, 0, "y")]),
            ins(OR, 0, &[d(R, 0, "x"), s(R, 0, "x"), s(R, 0, "w")]),
            ins(NOT, 0, &[d(R, 0, "x"), s(R, 0, "x")]),
            ins(IF, NZ, &[s(R, 0, "x")]),
            ins(MOV, 0, &[d(R, 1, "xyzw"), fl(&[0.0, 0.0, 0.0, 0.0])]),
            ins(ENDIF, 0, &[]),
            ins(MOV, 0, &[d(O, 0, "xyzw"), s(R, 1, "xyzw")]),
            ins(RET, 0, &[]),
        ]
        .concat();
        container(&[(b"ISGN", sig(&[])), (b"OSGN", sig(&[("SV_Target", 0, 0, 0, 0xF)])), (b"SHDR", shdr(0, &code))])
    }

    #[test]
    fn kitchen_sink() {
        let v = translate(&kitchen_vertex()).unwrap();
        let g = &v.glsl;
        for want in [
            "in vec4 a_POSITION0; // v0",
            "in vec4 a_BLENDINDICES0; // v1",
            "v1 = uintBitsToFloat(uvec4(a_BLENDINDICES0));",
            "v2 = vec4(intBitsToFloat(gl_VertexID), 0.0, 0.0, 0.0);",
            "out vec4 v_TEXCOORD2;",
            "v_TEXCOORD2 = vec4(o2.zw, 0.0, 0.0);",
            "v_BLENDINDICES0 = vec4(floatBitsToUint(vec4(o3.x, 0.0, 0.0, 0.0)));",
            "r1.x = intBitsToFloat(floatBitsToInt(v2.x) + 1);",
            "r1.z = uintBitsToFloat(floatBitsToUint(r1.y) >> (1 & 31));",
            "r2.x = intBitsToFloat(-int(floatBitsToInt(r1.x) != (-2147483647 - 1)));",
            "x0[clamp(0 + floatBitsToInt(r3.x), 0, 3)] = r0;",
            "r0.x = intBitsToFloat(icb[clamp(1 + floatBitsToInt(r3.x), 0, 1)].x);",
            "r0.y = vc[clamp(2 + floatBitsToInt(r3.x), 0, 7)].y;",
            "const ivec4 icb[2]",
            "switch (floatBitsToInt(r3.y)) {",
            "case (-1):",
            "if (floatBitsToInt(r3.x) != 0) l0();",
            "void l1();",
            "texelFetch(s3, floatBitsToInt(r1.xy), floatBitsToInt(r1.w)).wzyx",
            "textureSize(s3, 0)",
        ] {
            assert!(g.contains(want), "{want}:\n{g}");
        }
        assert_eq!(v.cbuffers, vec![(0, 0, 8), (2, 8, 4)]);
        assert_eq!(v.samples, vec![(3, 1)]);
        let p = translate(&kitchen_pixel()).unwrap();
        let g = &p.glsl;
        for want in [
            "uniform highp sampler2DShadow s1;",
            "uniform highp usampler2D s2;",
            "v0 = vec4(gl_FragCoord.xyz, 1.0 / gl_FragCoord.w);",
            "v3 = uintBitsToFloat(uvec4(round(v_BLENDINDICES0)));",
            "v4 = vec4(intBitsToFloat(gl_FrontFacing ? -1 : 0));",
            "v2.zw = (v_TEXCOORD2).xy;",
            "textureOffset(s0, v1.xy, ivec2(1, -1)).zy",
            "r2.xy = vec2(textureOffset(s1, vec3(v1.xy, v1.z), ivec2(1, 0)));",
            "gl_FragDepth = clamp(oDepth.x, 0.0, 1.0);",
            "if (floatBitsToInt(v4.x) == 0) discard;",
        ] {
            assert!(g.contains(want), "{want}:\n{g}");
        }
        assert_eq!(p.samples, vec![(0, 0), (1, 1)]);
        assert_eq!(p.cbuffers, vec![(1, 0, 2)]);
    }

    #[test]
    fn rejects() {
        let pixel = |code: &[u32]| container(&[(b"ISGN", sig(&[])), (b"OSGN", sig(&[("SV_Target", 0, 0, 0, 0xF)])), (b"SHDR", shdr(0, code))]);
        // gather4 is not in GLSL ES 3.00
        let e = translate(&pixel(&ins(109, 0, &[d(R, 0, "xyzw"), s(R, 0, "xyxx"), s(7, 0, "xyzw"), bare(6, 0)]))).unwrap_err();
        assert!(e.contains("gather4"), "{e}");
        // too many constants for a pixel shader
        let e = translate(&pixel(&ins(DCL_CONSTANT_BUFFER, 0, &[cb(0, 225, "xyzw")]))).unwrap_err();
        assert!(e.contains("224"), "{e}");
        // a vertex input without a Direct3D 9 usage
        let vs = container(&[(b"ISGN", sig(&[("BONES", 0, 0, 0, 0xF)])), (b"OSGN", sig(&[])), (b"SHDR", shdr(1, &ins(DCL_INPUT, 0, &[d(V, 0, "xyzw")])))]);
        assert!(translate(&vs).unwrap_err().contains("BONES"));
        // geometry shaders
        let gs = container(&[(b"SHDR", shdr(2, &[]))]);
        assert!(translate(&gs).is_err());
    }

    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else {
                out.push(p);
            }
        }
    }

    fn json_str(s: &str) -> String {
        let mut o = String::from("\"");
        for c in s.chars() {
            match c {
                '"' => o.push_str("\\\""),
                '\\' => o.push_str("\\\\"),
                '\n' => o.push_str("\\n"),
                c if (c as u32) < 0x20 => {
                    let _ = write!(o, "\\u{:04x}", c as u32);
                }
                c => o.push(c),
            }
        }
        o.push('"');
        o
    }

    /// With GMR_DXBC_DIR set, every DXBC blob found in the files under it must
    /// translate; GMR_DXBC_DUMP=<file> also writes the translations as JSON (to compile
    /// them in a browser).
    #[test]
    fn real_shaders() {
        let Ok(dir) = std::env::var("GMR_DXBC_DIR") else { return };
        let mut files = Vec::new();
        walk(std::path::Path::new(&dir), &mut files);
        files.sort();
        let mut seen = std::collections::HashSet::new();
        let (mut ok, mut failed, mut json) = (0, Vec::new(), Vec::new());
        for f in &files {
            let Ok(d) = std::fs::read(f) else { continue };
            let mut i = 0;
            while let Some(k) = d[i..].windows(4).position(|w| w == b"DXBC") {
                let at = i + k;
                i = at + 4;
                let rd = |o: usize| d.get(at + o..at + o + 4).map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
                let size = rd(24);
                if rd(20) != 1 || size < 32 || at + size > d.len() || rd(28) > 32 {
                    continue;
                }
                let blob = &d[at..at + size];
                i = at + size;
                if !seen.insert(blob.to_vec()) {
                    continue;
                }
                let name = format!("{}@{at:#x}", f.strip_prefix(&dir).unwrap_or(f).display());
                match translate(blob) {
                    Ok(s) => {
                        ok += 1;
                        json.push(format!("{{\"name\":{},\"vertex\":{},\"glsl\":{}}}", json_str(&name), s.vertex, json_str(&s.glsl)));
                    }
                    Err(e) => failed.push(format!("{name}: {e}")),
                }
            }
        }
        // the hand-assembled shaders too, to compile what the real ones do not use
        for (name, dxbc) in [("kitchen@vertex", kitchen_vertex()), ("kitchen@pixel", kitchen_pixel()), ("bits@pixel", bits_pixel())] {
            let s = translate(&dxbc).unwrap();
            json.push(format!("{{\"name\":\"{name}\",\"vertex\":{},\"glsl\":{}}}", s.vertex, json_str(&s.glsl)));
        }
        if let Ok(p) = std::env::var("GMR_DXBC_DUMP") {
            std::fs::write(p, format!("[\n{}\n]\n", json.join(",\n"))).unwrap();
        }
        eprintln!("{ok} shaders translated, {} failed", failed.len());
        for f in &failed {
            eprintln!("  {f}");
        }
        assert!(failed.is_empty());
    }
}
