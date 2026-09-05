//! Direct3D 9 shader bytecode (vs/ps 2.0, 2.x and 3.0) to GLSL ES 3.00.
//!
//! Conventions shared with web/src/gpu.mjs:
//! - vertex inputs are `in vec4 a_<USAGE><index>` (from the dcl of each v#); bit k of
//!   `u_bgra` says input v_k is a D3DCOLOR (bytes B, G, R, A) to swizzle;
//! - stage outputs/inputs are `v_<USAGE><index>` vec4 varyings, so a vertex and a
//!   pixel shader translated separately link by D3D semantics; a pixel shader input
//!   the vertex shader does not write is added to it by the host (`// @varyings`,
//!   `// @init` mark where);
//! - constants: `uniform vec4 vc[256]` / `pc[224]`, integers `vi`/`pi`, booleans
//!   `vb`/`pb`; samplers `s0`..; the position fix-up `u_fixup` = (1/w, 1/h) of the
//!   viewport and the pixel shader's alpha test `u_atest`, `u_aref`;
//! - rows are stored in D3D order (as everywhere in the renderer), so the vertex
//!   shader negates y, moves the half-pixel D3D9 offset and maps depth 0..1 to -1..1.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

const TEMP: u32 = 0;
const INPUT: u32 = 1;
const CONST: u32 = 2;
const ADDR: u32 = 3; // vs: a0; ps: t# (texture coordinates)
const RASTOUT: u32 = 4;
const ATTROUT: u32 = 5;
const OUTPUT: u32 = 6; // vs_3_0 o#; earlier: oT#
const CONSTINT: u32 = 7;
const COLOROUT: u32 = 8;
const DEPTHOUT: u32 = 9;
const SAMPLER: u32 = 10;
const CONSTBOOL: u32 = 14;
const LOOP: u32 = 15;
const MISC: u32 = 17;
const LABEL: u32 = 18;
const PREDICATE: u32 = 19;

const USAGES: [&str; 14] = ["POSITION", "BLENDWEIGHT", "BLENDINDICES", "NORMAL", "PSIZE", "TEXCOORD", "TANGENT", "BINORMAL", "TESSFACTOR", "POSITIONT", "COLOR", "FOG", "DEPTH", "SAMPLE"];

#[derive(Clone, Copy, Debug)]
struct Dst {
    ty: u32,
    num: u32,
    mask: u32,
    sat: bool,
}

#[derive(Clone, Debug)]
struct Src {
    ty: u32,
    num: u32,
    swz: [u8; 4],
    modifier: u32,
    /// Relative addressing: (register type, number, component).
    rel: Option<(u32, u32, u8)>,
}

#[derive(Clone, Debug)]
struct Instr {
    op: u32,
    ctl: u32,
    dst: Option<Dst>,
    pred: Option<Src>,
    src: Vec<Src>,
    /// dcl: usage, usage index, sampler type; def/defi/defb: literal words.
    extra: Vec<u32>,
}

fn reg_type(t: u32) -> u32 {
    ((t >> 28) & 7) | ((t >> 8) & 0x18)
}

fn parse_dst(t: u32) -> Dst {
    Dst { ty: reg_type(t), num: t & 0x7FF, mask: (t >> 16) & 0xF, sat: (t >> 20) & 1 != 0 }
}

/// Reads a source token (and the relative-address token that may follow).
fn parse_src(tokens: &[u32], i: &mut usize) -> Result<Src, String> {
    let t = *tokens.get(*i).ok_or("truncated source")?;
    *i += 1;
    let s = (t >> 16) & 0xFF;
    let swz = [s & 3, (s >> 2) & 3, (s >> 4) & 3, (s >> 6) & 3].map(|v| v as u8);
    let mut rel = None;
    if t & (1 << 13) != 0 {
        let r = *tokens.get(*i).ok_or("truncated relative address")?;
        *i += 1;
        rel = Some((reg_type(r), r & 0x7FF, ((r >> 16) & 3) as u8));
    }
    Ok(Src { ty: reg_type(t), num: t & 0x7FF, swz, modifier: (t >> 24) & 0xF, rel })
}

fn has_dst(op: u32) -> bool {
    !matches!(op, 0 | 25..=30 | 38..=45 | 96 | 65)
}

fn parse(tokens: &[u32]) -> Result<(bool, u32, Vec<Instr>), String> {
    let version = *tokens.first().ok_or("empty shader")?;
    let vertex = match version >> 16 {
        0xFFFE => true,
        0xFFFF => false,
        _ => return Err(format!("not a shader version token: {version:#x}")),
    };
    let major = (version >> 8) & 0xFF;
    if major < 2 {
        return Err(format!("shader model {major}.{} is not supported", version & 0xFF));
    }
    let mut out = Vec::new();
    let mut i = 1;
    while i < tokens.len() {
        let t = tokens[i];
        let op = t & 0xFFFF;
        if op == 0xFFFF {
            break;
        }
        if op == 0xFFFE {
            // comment (the constant table lives here)
            i += 1 + ((t >> 16) & 0x7FFF) as usize;
            continue;
        }
        let len = ((t >> 24) & 0xF) as usize;
        let body = tokens.get(i + 1..i + 1 + len).ok_or("truncated instruction")?;
        i += 1 + len;
        let ctl = (t >> 16) & 0xFF;
        let mut ins = Instr { op, ctl, dst: None, pred: None, src: Vec::new(), extra: Vec::new() };
        match op {
            31 => {
                // dcl: usage token, then the register
                let u = *body.first().ok_or("short dcl")?;
                ins.extra = vec![u & 0x1F, (u >> 16) & 0xF, (u >> 27) & 0xF];
                ins.dst = Some(parse_dst(*body.get(1).ok_or("short dcl")?));
            }
            81 | 48 | 47 => {
                ins.dst = Some(parse_dst(*body.first().ok_or("short def")?));
                ins.extra = body[1..].to_vec();
            }
            _ => {
                let mut k = 0;
                if has_dst(op) || op == 65 {
                    ins.dst = Some(parse_dst(*body.first().ok_or("missing destination")?));
                    k = 1;
                }
                if t & (1 << 28) != 0 {
                    ins.pred = Some(parse_src(body, &mut k)?);
                }
                while k < body.len() {
                    ins.src.push(parse_src(body, &mut k)?);
                }
            }
        }
        out.push(ins);
    }
    Ok((vertex, major, out))
}

fn usage_name(usage: u32, index: u32) -> String {
    format!("{}{}", USAGES.get(usage as usize).copied().unwrap_or("UNKNOWN"), index)
}

fn mask_str(mask: u32) -> String {
    let m = if mask == 0 { 0xF } else { mask };
    "xyzw".chars().enumerate().filter(|(k, _)| m & (1 << k) != 0).map(|(_, c)| c).collect()
}

fn swz_str(swz: &[u8; 4]) -> String {
    swz.iter().map(|&c| ['x', 'y', 'z', 'w'][c as usize]).collect()
}

fn float_lit(bits: u32) -> String {
    let f = f32::from_bits(bits);
    if f.is_nan() {
        return "(0.0/0.0)".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "(1.0/0.0)".into() } else { "(-1.0/0.0)".into() };
    }
    let s = format!("{f:?}");
    if s.contains(['.', 'e', 'E']) { s } else { format!("{s}.0") }
}

struct Gen {
    vertex: bool,
    major: u32,
    /// def/defi/defb constants by register number.
    defs: BTreeMap<u32, String>,
    defi: BTreeMap<u32, String>,
    defb: BTreeMap<u32, bool>,
    temps: BTreeSet<u32>,
    /// v# -> (usage, index) from dcl; for pixel shaders also t#.
    inputs: BTreeMap<(u32, u32), (u32, u32)>,
    /// vs_3_0 o# -> (usage, index).
    outputs: BTreeMap<u32, (u32, u32)>,
    samplers: BTreeMap<u32, u32>,
    /// Varyings written (vs) or read (ps).
    varyings: BTreeSet<String>,
    uses_a0: bool,
    uses_al: bool,
    uses_pred: bool,
    loop_depth: u32,
    body: String,
    indent: usize,
}

impl Gen {
    fn cname(&self) -> &'static str {
        if self.vertex { "vc" } else { "pc" }
    }

    /// The register as a vec4 (or bool/ivec4 for those types), before swizzle.
    fn reg(&mut self, ty: u32, num: u32, rel: Option<(u32, u32, u8)>) -> String {
        match ty {
            TEMP => {
                self.temps.insert(num);
                format!("r{num}")
            }
            INPUT => format!("in_v{num}"),
            CONST => match rel {
                Some((rty, rnum, comp)) => {
                    let idx = self.rel_index(rty, rnum, comp);
                    format!("{}[clamp({num} + {idx}, 0, {})]", self.cname(), if self.vertex { 255 } else { 223 })
                }
                None => match self.defs.get(&num) {
                    Some(lit) => lit.clone(),
                    None => format!("{}[{num}]", self.cname()),
                },
            },
            ADDR => {
                if self.vertex {
                    self.uses_a0 = true;
                    "vec4(a0)".into()
                } else {
                    format!("in_t{num}")
                }
            }
            CONSTINT => match self.defi.get(&num) {
                Some(lit) => format!("vec4({lit})"),
                None => format!("vec4({}[{num}])", if self.vertex { "vi" } else { "pi" }),
            },
            CONSTBOOL => match self.defb.get(&num) {
                Some(b) => format!("{b}"),
                None => format!("{}[{num}]", if self.vertex { "vb" } else { "pb" }),
            },
            LOOP => {
                self.uses_al = true;
                "vec4(float(aL))".into()
            }
            MISC => {
                if num == 0 {
                    "vec4(gl_FragCoord.xy - 0.5, 0.0, 0.0)".into()
                } else {
                    "vec4(gl_FrontFacing ? 1.0 : -1.0)".into()
                }
            }
            PREDICATE => {
                self.uses_pred = true;
                "vec4(p0)".into()
            }
            SAMPLER => format!("s{num}"),
            _ => format!("vec4(0.0) /* register type {ty} */"),
        }
    }

    fn rel_index(&mut self, ty: u32, _num: u32, comp: u8) -> String {
        let c = ['x', 'y', 'z', 'w'][comp as usize];
        if ty == LOOP {
            self.uses_al = true;
            "aL".into()
        } else {
            self.uses_a0 = true;
            format!("a0.{c}")
        }
    }

    /// A source operand as a vec4 expression.
    fn src(&mut self, s: &Src) -> String {
        let base = self.reg(s.ty, s.num, s.rel);
        if s.ty == CONSTBOOL {
            return base;
        }
        let sw = swz_str(&s.swz);
        let v = if sw == "xyzw" { base } else { format!("{base}.{sw}") };
        match s.modifier {
            0 => v,
            1 => format!("(-{v})"),
            2 => format!("({v} - 0.5)"),
            3 => format!("(0.5 - {v})"),
            4 => format!("(2.0 * {v} - 1.0)"),
            5 => format!("(1.0 - 2.0 * {v})"),
            6 => format!("(1.0 - {v})"),
            7 => format!("(2.0 * {v})"),
            8 => format!("(-2.0 * {v})"),
            11 => format!("abs({v})"),
            12 => format!("(-abs({v}))"),
            _ => v,
        }
    }

    fn scalar(&mut self, s: &Src) -> String {
        format!("{}.x", self.src(s))
    }

    /// The destination's name.
    fn dst_name(&mut self, d: &Dst) -> String {
        match d.ty {
            TEMP => {
                self.temps.insert(d.num);
                format!("r{}", d.num)
            }
            ADDR if self.vertex => {
                self.uses_a0 = true;
                "a0f".into()
            }
            RASTOUT => match d.num {
                0 => "o_pos".into(),
                1 => {
                    self.varyings.insert("FOG0".into());
                    "v_FOG0".into()
                }
                _ => "o_psize".into(),
            },
            ATTROUT => {
                let n = usage_name(10, d.num);
                self.varyings.insert(n.clone());
                format!("v_{n}")
            }
            OUTPUT => {
                if self.major >= 3 {
                    format!("o{}", d.num)
                } else {
                    let n = usage_name(5, d.num);
                    self.varyings.insert(n.clone());
                    format!("v_{n}")
                }
            }
            COLOROUT => format!("o_color{}", d.num),
            DEPTHOUT => "o_depth".into(),
            PREDICATE => {
                self.uses_pred = true;
                "p0f".into()
            }
            _ => format!("t_unknown{}", d.ty),
        }
    }

    fn line(&mut self, s: &str) {
        for _ in 0..self.indent {
            self.body.push_str("  ");
        }
        self.body.push_str(s);
        self.body.push('\n');
    }

    /// dst.mask = value (vec4), with saturate and predication.
    fn assign(&mut self, d: &Dst, value: String, pred: &Option<Src>) {
        let name = self.dst_name(d);
        let m = mask_str(d.mask);
        let v = if d.sat { format!("clamp({value}, 0.0, 1.0)") } else { value };
        match pred {
            Some(p) => {
                self.uses_pred = true;
                let mut pv = format!("p0.{}", swz_str(&p.swz));
                if p.modifier == 13 {
                    pv = format!("not({pv})");
                }
                self.line(&format!("{name}.{m} = mix({name}, {v}, {pv}).{m};"));
            }
            None => self.line(&format!("{name}.{m} = ({v}).{m};")),
        }
    }

    fn compare(ctl: u32, a: &str, b: &str) -> String {
        let op = match ctl {
            1 => ">",
            2 => "==",
            3 => ">=",
            4 => "<",
            5 => "!=",
            6 => "<=",
            _ => "==",
        };
        format!("({a} {op} {b})")
    }

    fn instr(&mut self, ins: &Instr) -> Result<(), String> {
        let s = &ins.src;
        let d = ins.dst;
        let pred = &ins.pred;
        let set = |g: &mut Gen, v: String| {
            if let Some(d) = d {
                g.assign(&d, v, pred);
            }
        };
        match ins.op {
            0 | 31 | 81 | 48 | 47 | 0xFFFD => {}
            1 => {
                let v = self.src(&s[0]);
                set(self, v);
            }
            2 => {
                let (a, b) = (self.src(&s[0]), self.src(&s[1]));
                set(self, format!("{a} + {b}"));
            }
            3 => {
                let (a, b) = (self.src(&s[0]), self.src(&s[1]));
                set(self, format!("{a} - {b}"));
            }
            4 => {
                let (a, b, c) = (self.src(&s[0]), self.src(&s[1]), self.src(&s[2]));
                set(self, format!("{a} * {b} + {c}"));
            }
            5 => {
                let (a, b) = (self.src(&s[0]), self.src(&s[1]));
                set(self, format!("{a} * {b}"));
            }
            6 => {
                let a = self.scalar(&s[0]);
                set(self, format!("vec4(1.0 / {a})"));
            }
            7 => {
                let a = self.scalar(&s[0]);
                set(self, format!("vec4(inversesqrt(abs({a})))"));
            }
            8 => {
                let (a, b) = (self.src(&s[0]), self.src(&s[1]));
                set(self, format!("vec4(dot(({a}).xyz, ({b}).xyz))"));
            }
            9 => {
                let (a, b) = (self.src(&s[0]), self.src(&s[1]));
                set(self, format!("vec4(dot({a}, {b}))"));
            }
            10 | 11 => {
                let (a, b) = (self.src(&s[0]), self.src(&s[1]));
                set(self, format!("{}({a}, {b})", if ins.op == 10 { "min" } else { "max" }));
            }
            12 | 13 => {
                let (a, b) = (self.src(&s[0]), self.src(&s[1]));
                set(self, format!("vec4({}({a}, {b}))", if ins.op == 12 { "lessThan" } else { "greaterThanEqual" }));
            }
            14 | 78 => {
                let a = self.scalar(&s[0]);
                set(self, format!("vec4(exp2({a}))"));
            }
            15 | 79 => {
                let a = self.scalar(&s[0]);
                set(self, format!("vec4(log2(abs({a})))"));
            }
            16 => {
                // lit: (1, max(x,0), x > 0 ? pow(max(y,0), clamp(w,-128,128)) : 0, 1)
                let a = self.src(&s[0]);
                set(self, format!("lit9({a})"));
            }
            17 => {
                let (a, b) = (self.src(&s[0]), self.src(&s[1]));
                set(self, format!("vec4(1.0, ({a}).y * ({b}).y, ({a}).z, ({b}).w)"));
            }
            18 => {
                let (a, b, c) = (self.src(&s[0]), self.src(&s[1]), self.src(&s[2]));
                set(self, format!("mix({c}, {b}, {a})"));
            }
            19 => {
                let a = self.src(&s[0]);
                set(self, format!("fract({a})"));
            }
            20..=24 => {
                // m4x4, m4x3, m3x4, m3x3, m3x2: rows are consecutive registers from src1
                let (rows, dot3) = match ins.op {
                    20 => (4, false),
                    21 => (3, false),
                    22 => (4, true),
                    23 => (3, true),
                    _ => (2, true),
                };
                let a = self.src(&s[0]);
                let mut comps = Vec::new();
                for k in 0..rows {
                    let mut row = s[1].clone();
                    row.num += k;
                    let r = self.src(&row);
                    comps.push(if dot3 { format!("dot(({a}).xyz, ({r}).xyz)") } else { format!("dot({a}, {r})") });
                }
                while comps.len() < 4 {
                    comps.push("0.0".into());
                }
                set(self, format!("vec4({})", comps.join(", ")));
            }
            32 => {
                let (a, b) = (self.scalar(&s[0]), self.scalar(&s[1]));
                set(self, format!("vec4(pow(abs({a}), {b}))"));
            }
            33 => {
                let (a, b) = (self.src(&s[0]), self.src(&s[1]));
                set(self, format!("vec4(cross(({a}).xyz, ({b}).xyz), 0.0)"));
            }
            34 => {
                let a = self.src(&s[0]);
                set(self, format!("sign({a})"));
            }
            35 => {
                let a = self.src(&s[0]);
                set(self, format!("abs({a})"));
            }
            36 => {
                let a = self.src(&s[0]);
                set(self, format!("(({a}) * inversesqrt(max(dot(({a}).xyz, ({a}).xyz), 1e-38)))"));
            }
            37 => {
                let a = self.scalar(&s[0]);
                set(self, format!("vec4(cos({a}), sin({a}), 0.0, 0.0)"));
            }
            46 => {
                // mova: round to nearest (vs_2_0 and later)
                let a = self.src(&s[0]);
                self.uses_a0 = true;
                if let Some(d) = d {
                    let m = mask_str(d.mask);
                    self.line(&format!("a0.{m} = ivec4(floor({a} + 0.5)).{m};"));
                }
            }
            40 => {
                // if b# (a boolean constant) or if p0.c (the predicate)
                let cond = if s[0].ty == PREDICATE {
                    self.uses_pred = true;
                    let not = if s[0].modifier == 13 { "!" } else { "" };
                    format!("{not}p0.{}", ['x', 'y', 'z', 'w'][s[0].swz[0] as usize])
                } else {
                    self.src(&s[0])
                };
                self.line(&format!("if ({cond}) {{"));
                self.indent += 1;
            }
            41 => {
                let (a, b) = (self.scalar(&s[0]), self.scalar(&s[1]));
                let cond = Self::compare(ins.ctl, &a, &b);
                self.line(&format!("if {cond} {{"));
                self.indent += 1;
            }
            42 => {
                self.indent = self.indent.saturating_sub(1);
                self.line("} else {");
                self.indent += 1;
            }
            43 | 39 | 29 => {
                self.indent = self.indent.saturating_sub(1);
                if ins.op == 29 {
                    self.loop_depth -= 1;
                }
                self.line("}");
            }
            38 => {
                let n = self.src(&s[0]);
                let k = self.loop_depth;
                self.loop_depth += 1;
                self.line(&format!("for (int rep{k} = 0; rep{k} < int(({n}).x) && rep{k} < 255; rep{k}++) {{"));
                self.indent += 1;
            }
            27 => {
                // loop aL, i#: count, start, step
                let i = self.src(&s[1]);
                let k = self.loop_depth;
                self.loop_depth += 1;
                self.uses_al = true;
                self.line(&format!("aL = int(({i}).y);"));
                self.line(&format!("for (int loop{k} = 0; loop{k} < int(({i}).x) && loop{k} < 255; loop{k}++, aL += int(({i}).z)) {{"));
                self.indent += 1;
            }
            44 => self.line("break;"),
            45 => {
                let (a, b) = (self.scalar(&s[0]), self.scalar(&s[1]));
                let cond = Self::compare(ins.ctl, &a, &b);
                self.line(&format!("if {cond} break;"));
            }
            96 => {
                self.uses_pred = true;
                let comp = ['x', 'y', 'z', 'w'][s[0].swz[0] as usize];
                let not = if s[0].modifier == 13 { "!" } else { "" };
                self.line(&format!("if ({not}p0.{comp}) break;"));
            }
            94 => {
                // setp: per-component compare into the predicate register
                let (a, b) = (self.src(&s[0]), self.src(&s[1]));
                let f = match ins.ctl {
                    1 => "greaterThan",
                    2 => "equal",
                    3 => "greaterThanEqual",
                    4 => "lessThan",
                    5 => "notEqual",
                    _ => "lessThanEqual",
                };
                self.uses_pred = true;
                if let Some(d) = d {
                    let m = mask_str(d.mask);
                    self.line(&format!("p0.{m} = {f}({a}, {b}).{m};"));
                }
            }
            25 => {
                let l = s[0].num;
                self.line(&format!("sub{l}();"));
            }
            26 => {
                let l = s[0].num;
                let c = self.src(&s[1]);
                let cond = if s[1].ty == PREDICATE { "p0.x".to_string() } else { c };
                self.line(&format!("if ({cond}) sub{l}();"));
            }
            28 => self.line("return;"),
            30 => {}
            65 => {
                // texkill: discard if any written component of the register is negative
                if let Some(d) = d {
                    let name = self.dst_name(&d);
                    let m = mask_str(d.mask);
                    let n = m.len();
                    let cmp = if n == 1 { format!("{name}.{m} < 0.0") } else { format!("any(lessThan({name}.{m}, vec{n}(0.0)))") };
                    self.line(&format!("if ({cmp}) discard;"));
                }
            }
            66 => {
                // texld / texldp / texldb
                let c = self.src(&s[0]);
                let smp = s[1].num;
                let kind = self.samplers.get(&smp).copied().unwrap_or(2);
                let v = match (ins.ctl & 3, kind) {
                    (_, 3) => format!("texture(s{smp}, ({c}).xyz)"),
                    (_, 4) => format!("texture(s{smp}, ({c}).xyz)"),
                    (1, _) => format!("textureProj(s{smp}, ({c}).xyw)"),
                    (2, _) => format!("texture(s{smp}, ({c}).xy, ({c}).w)"),
                    _ => format!("texture(s{smp}, ({c}).xy)"),
                };
                set(self, v);
            }
            95 => {
                let c = self.src(&s[0]);
                let smp = s[1].num;
                set(self, format!("textureLod(s{smp}, ({c}).xy, ({c}).w)"));
            }
            93 => {
                let c = self.src(&s[0]);
                let smp = s[1].num;
                let (dx, dy) = (self.src(&s[2]), self.src(&s[3]));
                set(self, format!("textureGrad(s{smp}, ({c}).xy, ({dx}).xy, ({dy}).xy)"));
            }
            88 => {
                let (a, b, c) = (self.src(&s[0]), self.src(&s[1]), self.src(&s[2]));
                set(self, format!("mix({c}, {b}, greaterThanEqual({a}, vec4(0.0)))"));
            }
            80 => {
                let (a, b, c) = (self.src(&s[0]), self.src(&s[1]), self.src(&s[2]));
                set(self, format!("mix({c}, {b}, greaterThan({a}, vec4(0.5)))"));
            }
            90 => {
                let (a, b, c) = (self.src(&s[0]), self.src(&s[1]), self.scalar(&s[2]));
                set(self, format!("vec4(dot(({a}).xy, ({b}).xy) + {c})"));
            }
            91 | 92 => {
                let a = self.src(&s[0]);
                set(self, format!("{}({a})", if ins.op == 91 { "dFdx" } else { "dFdy" }));
            }
            op => return Err(format!("opcode {op} is not supported")),
        }
        Ok(())
    }
}

/// Translates a shader's token stream (version token .. END) to GLSL.
pub fn translate(tokens: &[u32]) -> Result<String, String> {
    let (vertex, major, instrs) = parse(tokens)?;
    let mut g = Gen {
        vertex,
        major,
        defs: BTreeMap::new(),
        defi: BTreeMap::new(),
        defb: BTreeMap::new(),
        temps: BTreeSet::new(),
        inputs: BTreeMap::new(),
        outputs: BTreeMap::new(),
        samplers: BTreeMap::new(),
        varyings: BTreeSet::new(),
        uses_a0: false,
        uses_al: false,
        uses_pred: false,
        loop_depth: 0,
        body: String::new(),
        indent: 1,
    };
    // Declarations and constants first: they are referenced by the code.
    for ins in &instrs {
        let Some(d) = ins.dst else { continue };
        match ins.op {
            31 => {
                let (usage, index, stype) = (ins.extra[0], ins.extra[1], ins.extra[2]);
                match d.ty {
                    SAMPLER => {
                        g.samplers.insert(d.num, stype);
                    }
                    // shader model 2 pixel shaders: t# are texture coordinates and v# colors
                    // whatever the dcl says
                    ADDR if !vertex && major < 3 => {
                        g.inputs.insert((d.ty, d.num), (5, d.num));
                    }
                    INPUT if !vertex && major < 3 => {
                        g.inputs.insert((d.ty, d.num), (10, d.num));
                    }
                    INPUT | ADDR => {
                        g.inputs.insert((d.ty, d.num), (usage, index));
                    }
                    OUTPUT if major >= 3 => {
                        g.outputs.insert(d.num, (usage, index));
                    }
                    _ => {}
                }
            }
            81 => {
                let w: Vec<String> = ins.extra.iter().take(4).map(|&b| float_lit(b)).collect();
                g.defs.insert(d.num, format!("vec4({})", w.join(", ")));
            }
            48 => {
                let w: Vec<String> = ins.extra.iter().take(4).map(|&b| (b as i32).to_string()).collect();
                g.defi.insert(d.num, format!("ivec4({})", w.join(", ")));
            }
            47 => {
                g.defb.insert(d.num, ins.extra.first().copied().unwrap_or(0) != 0);
            }
            _ => {}
        }
    }
    // Pixel shader inputs of shader model 2: v0/v1 are the colors, t# texture coordinates.
    if !vertex && major < 3 {
        for ins in &instrs {
            for s in ins.src.iter() {
                match s.ty {
                    INPUT => {
                        g.inputs.entry((INPUT, s.num)).or_insert((10, s.num));
                    }
                    ADDR => {
                        g.inputs.entry((ADDR, s.num)).or_insert((5, s.num));
                    }
                    _ => {}
                }
            }
        }
    }
    // Code: main, then subroutines (label .. ret).
    let mut main_end = instrs.len();
    let mut subs: Vec<(u32, usize, usize)> = Vec::new();
    for (k, ins) in instrs.iter().enumerate() {
        if ins.op == 30 {
            if main_end == instrs.len() {
                main_end = k;
            }
            if let Some(last) = subs.last_mut() {
                last.2 = k;
            }
            subs.push((ins.src.first().map(|s| s.num).unwrap_or(0), k + 1, instrs.len()));
        }
    }
    for ins in &instrs[..main_end] {
        g.instr(ins)?;
    }
    let main_body = std::mem::take(&mut g.body);
    let mut sub_bodies = String::new();
    for &(label, from, to) in &subs {
        for ins in &instrs[from..to] {
            g.instr(ins)?;
        }
        let b = std::mem::take(&mut g.body);
        let _ = writeln!(sub_bodies, "void sub{label}() {{\n{b}}}");
    }

    let mut out = String::new();
    out.push_str("#version 300 es\nprecision highp float;\nprecision highp int;\n");
    let (cn, cs, iname, bname) = if vertex { ("vc", 256, "vi", "vb") } else { ("pc", 224, "pi", "pb") };
    let _ = writeln!(out, "uniform vec4 {cn}[{cs}];\nuniform ivec4 {iname}[16];\nuniform bool {bname}[16];");
    for (&s, &kind) in &g.samplers {
        let t = match kind {
            3 => "samplerCube",
            4 => "sampler3D",
            _ => "sampler2D",
        };
        let _ = writeln!(out, "uniform highp {t} s{s};");
    }
    if vertex {
        out.push_str("uniform vec2 u_fixup;\nuniform int u_bgra;\n");
        for (&(_, v), &(usage, index)) in &g.inputs {
            let _ = writeln!(out, "in vec4 a_{}; // v{v}", usage_name(usage, index));
        }
        // vs_3_0 output registers become varyings by their declared usage
        for &(usage, index) in g.outputs.values() {
            if usage != 0 && usage != 4 {
                g.varyings.insert(usage_name(usage, index));
            }
        }
        for v in &g.varyings {
            let _ = writeln!(out, "out vec4 v_{v};");
        }
        out.push_str("// @varyings\n");
    } else {
        out.push_str("uniform int u_atest;\nuniform float u_aref;\n");
        for (&(ty, num), &(usage, index)) in &g.inputs {
            let name = usage_name(usage, index);
            if g.varyings.insert(name.clone()) {
                let _ = writeln!(out, "in vec4 v_{name}; // {}{num}", if ty == ADDR { "t" } else { "v" });
            }
        }
        out.push_str("out vec4 o_frag;\n");
    }
    // registers
    for &t in &g.temps {
        let _ = writeln!(out, "vec4 r{t} = vec4(0.0);");
    }
    if g.uses_a0 {
        out.push_str("ivec4 a0 = ivec4(0);\n");
    }
    if g.uses_al {
        out.push_str("int aL = 0;\n");
    }
    if g.uses_pred {
        out.push_str("bvec4 p0 = bvec4(false);\n");
    }
    if vertex {
        out.push_str("vec4 o_pos = vec4(0.0, 0.0, 0.0, 1.0);\nvec4 o_psize = vec4(1.0);\n");
        for &k in g.outputs.keys() {
            let _ = writeln!(out, "vec4 o{k} = vec4(0.0);");
        }
        for &(_, v) in g.inputs.keys() {
            let _ = writeln!(out, "vec4 in_v{v};");
        }
    } else {
        out.push_str("vec4 o_color0 = vec4(0.0);\nvec4 o_color1 = vec4(0.0);\nvec4 o_color2 = vec4(0.0);\nvec4 o_color3 = vec4(0.0);\nvec4 o_depth = vec4(0.0);\n");
        for &(ty, num) in g.inputs.keys() {
            let _ = writeln!(out, "vec4 in_{}{num};", if ty == ADDR { "t" } else { "v" });
        }
    }
    out.push_str("vec4 lit9(vec4 s) {\n  float d = max(s.x, 0.0);\n  float sp = s.x > 0.0 && s.y > 0.0 ? pow(s.y, clamp(s.w, -128.0, 128.0)) : 0.0;\n  return vec4(1.0, d, sp, 1.0);\n}\n");
    if !vertex {
        // D3D9 still applies the alpha test (a render state) after a pixel shader
        out.push_str("bool atest9(float a) {\n  float v = floor(clamp(a, 0.0, 1.0) * 255.0 + 0.5);\n  if (u_atest == 1) return false; if (u_atest == 2) return v < u_aref; if (u_atest == 3) return v == u_aref;\n  if (u_atest == 4) return v <= u_aref; if (u_atest == 5) return v > u_aref; if (u_atest == 6) return v != u_aref;\n  if (u_atest == 7) return v >= u_aref; return true;\n}\n");
    }
    out.push_str(&sub_bodies);
    out.push_str("void main() {\n");
    if vertex {
        out.push_str("  // @init\n");
        for (&(_, v), &(usage, index)) in &g.inputs {
            let _ = writeln!(out, "  in_v{v} = ((u_bgra >> {v}) & 1) != 0 ? a_{n}.zyxw : a_{n};", n = usage_name(usage, index));
        }
    } else {
        for (&(ty, num), &(usage, index)) in &g.inputs {
            let name = usage_name(usage, index);
            let clamp = usage == 10 && major < 3;
            let r = if ty == ADDR { "t" } else { "v" };
            let _ = writeln!(out, "  in_{r}{num} = {};", if clamp { format!("clamp(v_{name}, 0.0, 1.0)") } else { format!("v_{name}") });
        }
    }
    out.push_str(&main_body);
    if vertex {
        for (&k, &(usage, index)) in &g.outputs {
            match usage {
                0 => {
                    let _ = writeln!(out, "  o_pos = o{k};");
                }
                4 => {
                    let _ = writeln!(out, "  o_psize = o{k};");
                }
                _ => {
                    let _ = writeln!(out, "  v_{} = o{k};", usage_name(usage, index));
                }
            }
        }
        out.push_str("  gl_Position = vec4(o_pos.x + o_pos.w * u_fixup.x, -o_pos.y + o_pos.w * u_fixup.y, 2.0 * o_pos.z - o_pos.w, o_pos.w);\n");
        out.push_str("  gl_PointSize = o_psize.x;\n");
    } else {
        out.push_str("  if (!atest9(o_color0.a)) discard;\n");
        let writes_depth = instrs.iter().any(|i| i.dst.is_some_and(|d| d.ty == DEPTHOUT));
        if writes_depth {
            out.push_str("  gl_FragDepth = clamp(o_depth.x, 0.0, 1.0);\n");
        }
        out.push_str("  o_frag = o_color0;\n");
    }
    out.push_str("}\n");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// vs_2_0: dcl_position v0; dcl_texcoord v1; m4x4 oPos, v0, c0; mov oT0, v1
    #[test]
    fn simple_vertex_shader() {
        let t = vec![
            0xFFFE_0200,
            0x0200_001F, 0x8000_0000, 0x900F_0000, // dcl_position v0
            0x0200_001F, 0x8000_0005, 0x900F_0001, // dcl_texcoord v1
            0x0300_0014, 0xC00F_0000, 0x90E4_0000, 0xA0E4_0000, // m4x4 oPos, v0, c0
            0x0200_0001, 0xE00F_0000, 0x90E4_0001, // mov oT0, v1
            0x0000_FFFF,
        ];
        let g = translate(&t).unwrap();
        assert!(g.contains("in vec4 a_POSITION0"), "{g}");
        assert!(g.contains("in vec4 a_TEXCOORD0"), "{g}");
        assert!(g.contains("out vec4 v_TEXCOORD0"), "{g}");
        assert!(g.contains("dot(in_v0, vc[0])"), "{g}");
    }

    /// ps_2_0: dcl t0; dcl_2d s0; texld r0, t0, s0; mul r0, r0, c0; mov oC0, r0
    #[test]
    fn simple_pixel_shader() {
        let t = vec![
            0xFFFF_0200,
            0x0200_001F, 0x8000_0000, 0xB00F_0000, // dcl t0
            0x0200_001F, 0x9000_0000, 0xA00F_0800, // dcl_2d s0
            0x0300_0042, 0x800F_0000, 0xB0E4_0000, 0xA0E4_0800, // texld r0, t0, s0
            0x0300_0005, 0x800F_0000, 0x80E4_0000, 0xA0E4_0000, // mul r0, r0, c0
            0x0200_0001, 0x800F_0800, 0x80E4_0000, // mov oC0, r0
            0x0000_FFFF,
        ];
        let g = translate(&t).unwrap();
        assert!(g.contains("uniform highp sampler2D s0"), "{g}");
        assert!(g.contains("texture(s0, (in_t0).xy)"), "{g}");
        assert!(g.contains("in vec4 v_TEXCOORD0"), "{g}");
        assert!(g.contains("o_frag = o_color0"), "{g}");
    }
}
