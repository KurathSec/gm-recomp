// Direct3D 8 fixed-function rendering on WebGL2, fed by the runtime's Gpu calls
// (crates/runtime/src/hle/gpu.rs). Images are kept in D3D row order (row 0 = top) in
// every framebuffer and texture; only the final blit to the canvas flips.

// DrawCall field offsets in 32-bit words (gpu.rs `DrawCall`, #[repr(C)]).
const D = {
  prim: 0, primCount: 1, vertexCount: 2, fvf: 3, stride: 4, vertices: 5, indices: 6, indexSize: 7, minIndex: 8,
  texture: 9, texFormat: 11, texFlip: 13, targetW: 15, targetH: 16, viewport: 17, world: 23, view: 39, proj: 55,
  rs: 71, tss0: 327, tss1: 359, words: 391,
};
// DrawCall9 (crates/runtime/src/hle/gpu.rs), in 32-bit words.
const D9 = {
  prim: 0, primCount: 1, vertexCount: 2, minIndex: 3, indices: 4, indexSize: 5, streams: 6, elements: 14, elementCount: 78,
  vs: 79, ps: 80, textures: 81, samplers: 129, targetW: 353, targetH: 354, viewport: 355, scissor: 361, rs: 365,
  vsF: 621, psF: 1645, vsI: 2541, psI: 2605, vsB: 2669, psB: 2685, words: 2701,
};
const USAGE = ['POSITION', 'BLENDWEIGHT', 'BLENDINDICES', 'NORMAL', 'PSIZE', 'TEXCOORD', 'TANGENT', 'BINORMAL', 'TESSFACTOR', 'POSITIONT', 'COLOR', 'FOG', 'DEPTH', 'SAMPLE'];
// D3DDECLTYPE -> [components, GL type, normalized]
const DECL = {
  0: [1, 'FLOAT', false], 1: [2, 'FLOAT', false], 2: [3, 'FLOAT', false], 3: [4, 'FLOAT', false], 4: [4, 'UNSIGNED_BYTE', true],
  5: [4, 'UNSIGNED_BYTE', false], 6: [2, 'SHORT', false], 7: [4, 'SHORT', false], 8: [4, 'UNSIGNED_BYTE', true], 9: [2, 'SHORT', true],
  10: [4, 'SHORT', true], 11: [2, 'UNSIGNED_SHORT', true], 12: [4, 'UNSIGNED_SHORT', true], 15: [2, 'HALF_FLOAT', false], 16: [4, 'HALF_FLOAT', false],
};
const RS = { ZENABLE: 7, FILLMODE: 8, ZWRITEENABLE: 14, ALPHATESTENABLE: 15, SRCBLEND: 19, DESTBLEND: 20, CULLMODE: 22,
  ZFUNC: 23, ALPHAREF: 24, ALPHAFUNC: 25, ALPHABLENDENABLE: 27, FOGENABLE: 28, FOGCOLOR: 34, FOGTABLEMODE: 35,
  FOGSTART: 36, FOGEND: 37, FOGDENSITY: 38, TEXTUREFACTOR: 60, LIGHTING: 137, FOGVERTEXMODE: 140, COLORWRITEENABLE: 168, BLENDOP: 171,
  SCISSORTESTENABLE: 174, SEPARATEALPHABLENDENABLE: 206, SRCBLENDALPHA: 207, DESTBLENDALPHA: 208, BLENDOPALPHA: 209 };
const TSS = { COLOROP: 1, COLORARG1: 2, COLORARG2: 3, ALPHAOP: 4, ALPHAARG1: 5, ALPHAARG2: 6, ADDRESSU: 13, ADDRESSV: 14,
  MAGFILTER: 16, MINFILTER: 17, MIPFILTER: 18 };

// DrawCall words whose change ends a batch (everything but the vertices; the
// transforms, D.world to D.proj + 16, only for untransformed vertices).
const KEY_WORDS = Int32Array.from([
  D.texture, D.texture + 1, D.targetW, D.targetH, ...[0, 1, 2, 3, 4, 5].map((i) => D.viewport + i),
  ...[RS.ZENABLE, RS.ZWRITEENABLE, RS.ALPHATESTENABLE, RS.SRCBLEND, RS.DESTBLEND, RS.CULLMODE, RS.ZFUNC, RS.ALPHAREF,
    RS.ALPHAFUNC, RS.ALPHABLENDENABLE, RS.FOGENABLE, RS.FOGCOLOR, RS.FOGTABLEMODE, RS.FOGSTART, RS.FOGEND, RS.FOGDENSITY,
    RS.TEXTUREFACTOR, RS.FOGVERTEXMODE, RS.COLORWRITEENABLE, RS.BLENDOP, RS.SEPARATEALPHABLENDENABLE, RS.SRCBLENDALPHA,
    RS.DESTBLENDALPHA, RS.BLENDOPALPHA].map((i) => D.rs + i),
  ...[1, 2, 3, 4, 5, 6, 13, 14, 16, 17, 18].flatMap((i) => [D.tss0 + i, D.tss1 + i]),
]);
const MATRIX_WORDS = D.proj + 16 - D.world;

// Canonical batch vertex: x y z w | diffuse | specular | u0 v0 | u1 v1 (10 words)
const VW = 10;
const MAX_BATCH = 65536 * 3;

const VS = `#version 300 es
in vec4 a_pos; in vec4 a_diffuse; in vec4 a_specular; in vec2 a_uv0; in vec2 a_uv1;
uniform int u_rhw; uniform mat4 u_wvp; uniform vec2 u_target; uniform vec4 u_vp;
out vec4 v_diffuse; out vec4 v_specular; out vec2 v_uv0; out vec2 v_uv1; out float v_fogz;
void main() {
  if (u_rhw == 1) {
    // Pre-transformed: window coordinates with D3D pixel centres at integers.
    float w = a_pos.w > 0.0 ? 1.0 / a_pos.w : 1.0;
    vec2 ndc = (a_pos.xy + 0.5) * 2.0 / u_target - 1.0;
    gl_Position = vec4(ndc * w, (a_pos.z * 2.0 - 1.0) * w, w);
    v_fogz = a_pos.z;
  } else {
    vec4 c = u_wvp * vec4(a_pos.xyz, 1.0);
    // D3D viewport mapping (y down, depth 0..1, centres at integers) in GL terms.
    gl_Position = vec4(c.x + c.w / u_vp.z, -c.y + c.w / u_vp.w, 2.0 * c.z - c.w, c.w);
    v_fogz = c.w;
  }
  v_diffuse = a_diffuse.bgra; v_specular = a_specular.bgra; v_uv0 = a_uv0; v_uv1 = a_uv1;
}`;

const FS = `#version 300 es
precision highp float;
in vec4 v_diffuse; in vec4 v_specular; in vec2 v_uv0; in vec2 v_uv1; in float v_fogz;
uniform sampler2D u_tex0; uniform sampler2D u_tex1;
uniform ivec2 u_swz; // per stage: 1 the texture holds B, G, R, A (as uploaded), 2 B, G, R, X
uniform ivec4 u_stage0; uniform ivec4 u_stage1; // colorop, colorarg1, colorarg2, alphaop
uniform ivec4 u_alpha0; uniform ivec4 u_alpha1; // alphaarg1, alphaarg2, has texture, -
uniform vec4 u_tfactor;
uniform int u_atest; uniform float u_aref;
uniform int u_fog; uniform vec4 u_fogcolor; uniform vec3 u_fogparams; // start, end, density
out vec4 o;
vec4 arg(int a, vec4 cur, vec4 tex) {
  vec4 v;
  int s = a & 7;
  if (s == 0) v = v_diffuse; else if (s == 1) v = cur; else if (s == 2) v = tex;
  else if (s == 3) v = u_tfactor; else if (s == 4) v = v_specular; else v = cur;
  if ((a & 0x20) != 0) v = vec4(v.a);
  if ((a & 0x10) != 0) v = 1.0 - v;
  return v;
}
vec3 combine(int op, vec3 a1, vec3 a2, vec4 d, vec4 t, vec4 cur, vec4 f) {
  if (op == 2) return a1;
  if (op == 3) return a2;
  if (op == 4) return a1 * a2;
  if (op == 5) return min(a1 * a2 * 2.0, 1.0);
  if (op == 6) return min(a1 * a2 * 4.0, 1.0);
  if (op == 7) return min(a1 + a2, 1.0);
  if (op == 8) return clamp(a1 + a2 - 0.5, 0.0, 1.0);
  if (op == 9) return clamp((a1 + a2 - 0.5) * 2.0, 0.0, 1.0);
  if (op == 10) return max(a1 - a2, 0.0);
  if (op == 11) return min(a1 + a2 - a1 * a2, 1.0);
  if (op == 12) return mix(a2, a1, d.a);
  if (op == 13) return mix(a2, a1, t.a);
  if (op == 14) return mix(a2, a1, f.a);
  if (op == 15) return min(a1 + a2 * (1.0 - t.a), 1.0);
  if (op == 16) return mix(a2, a1, cur.a);
  return a1;
}
float combineA(int op, float a1, float a2, vec4 d, vec4 t, vec4 cur, vec4 f) {
  return combine(op, vec3(a1), vec3(a2), d, t, cur, f).x;
}
vec4 stage(ivec4 s, ivec4 sa, vec4 cur, vec4 tex) {
  if (s.x == 1) return cur;
  vec4 c1 = arg(s.y, cur, tex), c2 = arg(s.z, cur, tex);
  vec4 a1 = arg(sa.x, cur, tex), a2 = arg(sa.y, cur, tex);
  vec3 rgb = combine(s.x, c1.rgb, c2.rgb, v_diffuse, tex, cur, u_tfactor);
  float al = s.w == 1 ? cur.a : combineA(s.w, a1.a, a2.a, v_diffuse, tex, cur, u_tfactor);
  return vec4(rgb, al);
}
vec4 swz(vec4 t, int s) { return s == 0 ? t : vec4(t.bgr, s == 2 ? 1.0 : t.a); }
bool cmp(int f, float a, float r) {
  if (f == 1) return false; if (f == 2) return a < r; if (f == 3) return a == r; if (f == 4) return a <= r;
  if (f == 5) return a > r; if (f == 6) return a != r; if (f == 7) return a >= r; return true;
}
void main() {
  vec4 t0 = u_alpha0.z == 1 ? swz(texture(u_tex0, v_uv0), u_swz.x) : vec4(1.0);
  vec4 cur = stage(u_stage0, u_alpha0, v_diffuse, t0);
  if (u_stage1.x != 1) {
    vec4 t1 = u_alpha1.z == 1 ? swz(texture(u_tex1, v_uv1), u_swz.y) : vec4(1.0);
    cur = stage(u_stage1, u_alpha1, cur, t1);
  }
  if (u_fog != 0) {
    float f = 1.0;
    float z = v_fogz;
    if (u_fog == 3) f = clamp((u_fogparams.y - z) / (u_fogparams.y - u_fogparams.x), 0.0, 1.0);
    else if (u_fog == 1) f = exp(-u_fogparams.z * z);
    else if (u_fog == 2) f = exp(-(u_fogparams.z * z) * (u_fogparams.z * z));
    cur.rgb = mix(u_fogcolor.rgb, cur.rgb, f);
  }
  // D3D compares the 8-bit alpha against ALPHAREF.
  if (u_atest != 8 && !cmp(u_atest, floor(cur.a * 255.0 + 0.5), u_aref)) discard;
  o = cur;
}`;


function compile(gl, type, src) {
  const s = gl.createShader(type);
  gl.shaderSource(s, src);
  gl.compileShader(s);
  if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(s));
  return s;
}

function mul(a, b) {
  const r = new Float32Array(16);
  for (let i = 0; i < 4; i++)
    for (let j = 0; j < 4; j++) r[i * 4 + j] = a[i * 4] * b[j] + a[i * 4 + 1] * b[4 + j] + a[i * 4 + 2] * b[8 + j] + a[i * 4 + 3] * b[12 + j];
  return r;
}

/** Converts D3D texel data to RGBA8. */
function toRGBA(fmt, src, w, h, pitch) {
  const out = new Uint8Array(w * h * 4);
  // A8R8G8B8 and X8R8G8B8, nearly every texture: a pixel per word, B and R swapped
  if ((fmt === 21 || fmt === 22) && src.byteOffset % 4 === 0 && pitch % 4 === 0) {
    const s32 = new Uint32Array(src.buffer, src.byteOffset, (pitch >> 2) * h);
    const o32 = new Uint32Array(out.buffer);
    const opaque = fmt === 22 ? 0xff000000 : 0;
    for (let y = 0, d = 0; y < h; y++) {
      for (let s = y * (pitch >> 2), end = s + w; s < end; s++, d++) {
        const v = s32[s];
        o32[d] = (v & 0xff00ff00) | ((v >>> 16) & 0xff) | ((v & 0xff) << 16) | opaque;
      }
    }
    return out;
  }
  for (let y = 0; y < h; y++) {
    let s = y * pitch;
    let d = y * w * 4;
    for (let x = 0; x < w; x++, d += 4) {
      switch (fmt) {
        case 21: // A8R8G8B8: B G R A in memory
          out[d] = src[s + 2]; out[d + 1] = src[s + 1]; out[d + 2] = src[s]; out[d + 3] = src[s + 3]; s += 4; break;
        case 22: // X8R8G8B8
          out[d] = src[s + 2]; out[d + 1] = src[s + 1]; out[d + 2] = src[s]; out[d + 3] = 255; s += 4; break;
        case 20: // R8G8B8
          out[d] = src[s + 2]; out[d + 1] = src[s + 1]; out[d + 2] = src[s]; out[d + 3] = 255; s += 3; break;
        case 23: { // R5G6B5
          const v = src[s] | (src[s + 1] << 8); s += 2;
          out[d] = ((v >> 11) & 31) * 255 / 31; out[d + 1] = ((v >> 5) & 63) * 255 / 63; out[d + 2] = (v & 31) * 255 / 31; out[d + 3] = 255; break;
        }
        case 24: case 25: { // X1R5G5B5 / A1R5G5B5
          const v = src[s] | (src[s + 1] << 8); s += 2;
          out[d] = ((v >> 10) & 31) * 255 / 31; out[d + 1] = ((v >> 5) & 31) * 255 / 31; out[d + 2] = (v & 31) * 255 / 31;
          out[d + 3] = fmt === 25 ? ((v >> 15) ? 255 : 0) : 255; break;
        }
        case 26: case 30: { // A4R4G4B4 / X4R4G4B4
          const v = src[s] | (src[s + 1] << 8); s += 2;
          out[d] = ((v >> 8) & 15) * 17; out[d + 1] = ((v >> 4) & 15) * 17; out[d + 2] = (v & 15) * 17;
          out[d + 3] = fmt === 26 ? ((v >> 12) & 15) * 17 : 255; break;
        }
        case 28: // A8
          out[d] = out[d + 1] = out[d + 2] = 0; out[d + 3] = src[s]; s += 1; break;
        case 50: // L8
          out[d] = out[d + 1] = out[d + 2] = src[s]; out[d + 3] = 255; s += 1; break;
        case 51: // A8L8
          out[d] = out[d + 1] = out[d + 2] = src[s]; out[d + 3] = src[s + 1]; s += 2; break;
        default:
          out[d] = out[d + 1] = out[d + 2] = 255; out[d + 3] = 255; s += 4;
      }
    }
  }
  return out;
}

/** The canvas's WebGL2 context attributes (a page may create the context early with them). */
export const GL_OPTIONS = { antialias: false, alpha: false, depth: false, stencil: false, premultipliedAlpha: false, desynchronized: true, preserveDrawingBuffer: false };

export class D3DGL {
  /**
   * @param {OffscreenCanvas|HTMLCanvasElement} canvas
   * @param {() => ArrayBuffer} guestBuffer  current guest memory buffer
   * @param {() => ArrayBuffer} rtBuffer  current runtime memory buffer (DrawCall records)
   * @param {(line: string) => void} [print]  where problems are reported
   */
  constructor(canvas, guestBuffer, rtBuffer, print = console.warn) {
    this.print = print;
    this.canvas = canvas;
    this.guestBuffer = guestBuffer;
    this.rtBuffer = rtBuffer;
    const gl = canvas.getContext('webgl2', GL_OPTIONS);
    if (!gl) throw new Error('WebGL2 is not available');
    this.gl = gl;
    // which GPU draws (a software renderer explains a slow game); Firefox names it
    // in RENDERER, Chrome only through the debug extension
    let renderer = gl.getParameter(gl.RENDERER);
    if (/^WebKit/.test(renderer)) {
      const info = gl.getExtension('WEBGL_debug_renderer_info');
      if (info) renderer = gl.getParameter(info.UNMASKED_RENDERER_WEBGL);
    }
    print(`WebGL renderer: ${renderer}`);
    const p = gl.createProgram();
    gl.attachShader(p, compile(gl, gl.VERTEX_SHADER, VS));
    gl.attachShader(p, compile(gl, gl.FRAGMENT_SHADER, FS));
    for (const [i, n] of ['a_pos', 'a_diffuse', 'a_specular', 'a_uv0', 'a_uv1'].entries()) gl.bindAttribLocation(p, i, n);
    gl.linkProgram(p);
    if (!gl.getProgramParameter(p, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(p));
    this.prog = p;
    gl.useProgram(p);
    this.u = {};
    for (const n of ['u_rhw', 'u_wvp', 'u_target', 'u_vp', 'u_tex0', 'u_tex1', 'u_swz', 'u_stage0', 'u_stage1', 'u_alpha0', 'u_alpha1', 'u_tfactor', 'u_atest', 'u_aref', 'u_fog', 'u_fogcolor', 'u_fogparams'])
      this.u[n] = gl.getUniformLocation(p, n);
    gl.uniform1i(this.u.u_tex0, 0);
    gl.uniform1i(this.u.u_tex1, 1);
    this.vbo = gl.createBuffer();
    this.vao = gl.createVertexArray();
    gl.bindVertexArray(this.vao);
    gl.bindBuffer(gl.ARRAY_BUFFER, this.vbo);
    gl.enableVertexAttribArray(0); gl.vertexAttribPointer(0, 4, gl.FLOAT, false, VW * 4, 0);
    gl.enableVertexAttribArray(1); gl.vertexAttribPointer(1, 4, gl.UNSIGNED_BYTE, true, VW * 4, 16);
    gl.enableVertexAttribArray(2); gl.vertexAttribPointer(2, 4, gl.UNSIGNED_BYTE, true, VW * 4, 20);
    gl.enableVertexAttribArray(3); gl.vertexAttribPointer(3, 2, gl.FLOAT, false, VW * 4, 24);
    gl.enableVertexAttribArray(4); gl.vertexAttribPointer(4, 2, gl.FLOAT, false, VW * 4, 32);
    this.batch = new ArrayBuffer(MAX_BATCH * VW * 4);
    this.bf = new Float32Array(this.batch);
    this.bu = new Uint32Array(this.batch);
    this.count = 0;
    this.mode = gl.TRIANGLES;
    // the state the batch was drawn with (draw): mode, rhw and KEY_WORDS then the
    // transforms; `keyValid` false when the GL state may differ from it
    this.keyValid = false;
    this.keyMode = 0;
    this.keyRhw = false;
    this.keyWords = new Uint32Array(KEY_WORDS.length + MATRIX_WORDS);
    this.textures = new Map(); // id -> {tex, w, h, levels, uploaded, params, fbo, depth}
    this.white = this.makeTexture();
    gl.bindTexture(gl.TEXTURE_2D, this.white);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA8, 1, 1, 0, gl.RGBA, gl.UNSIGNED_BYTE, new Uint8Array([255, 255, 255, 255]));
    this.back = null;
    this.target = 0;
    this.targetW = 1;
    this.targetH = 1;
    this.frames = 0;
    this.draws = 0;
    this.states = 0; // batches started by a state change
  }

  makeTexture() {
    const gl = this.gl;
    const t = gl.createTexture();
    gl.bindTexture(gl.TEXTURE_2D, t);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.NEAREST);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.NEAREST);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAX_LEVEL, 0);
    return t;
  }

  framebuffer(w, h, colorTex) {
    const gl = this.gl;
    const fbo = gl.createFramebuffer();
    gl.bindFramebuffer(gl.FRAMEBUFFER, fbo);
    gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, colorTex, 0);
    const depth = gl.createRenderbuffer();
    gl.bindRenderbuffer(gl.RENDERBUFFER, depth);
    gl.renderbufferStorage(gl.RENDERBUFFER, gl.DEPTH24_STENCIL8, w, h);
    gl.framebufferRenderbuffer(gl.FRAMEBUFFER, gl.DEPTH_STENCIL_ATTACHMENT, gl.RENDERBUFFER, depth);
    return { fbo, depth };
  }

  // ---- Gpu interface ----------------------------------------------------------------

  resize(w, h) {
    const gl = this.gl;
    this.flush();
    if (this.back) {
      gl.deleteFramebuffer(this.back.fbo);
      gl.deleteRenderbuffer(this.back.depth);
      gl.deleteTexture(this.back.tex);
    }
    const tex = this.makeTexture();
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA8, w, h, 0, gl.RGBA, gl.UNSIGNED_BYTE, null);
    this.back = { tex, w, h, ...this.framebuffer(w, h, tex) };
    this.bindTarget(0, w, h);
  }

  textureCreate(id, w, h, levels, fmt, rt) {
    const gl = this.gl;
    const tex = this.makeTexture();
    // Undefined contents read as transparent black until uploaded or rendered.
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA8, w, h, 0, gl.RGBA, gl.UNSIGNED_BYTE, null);
    // swz: how the texture holds its texels, decided by the first upload (-1 before):
    // 0 R, G, B, A; 1 or 2 as the guest has them (see textureUpload)
    const t = { tex, w, h, levels, fmt, uploaded: 1, params: '', fbo: null, depth: null, swz: rt ? 0 : -1, src: null };
    if (rt) Object.assign(t, this.framebuffer(w, h, tex));
    this.textures.set(id, t);
    this.keyValid = false;
  }

  textureUpload(id, level, w, h, fmt, pitch, bits) {
    const t = this.textures.get(id);
    if (!t) return;
    this.flush();
    const gl = this.gl;
    const src = new Uint8Array(this.guestBuffer(), bits, pitch * h);
    // A8R8G8B8 and X8R8G8B8 go up as they are in guest memory (B, G, R, A bytes), and
    // the fixed-function shader swaps R and B (u_swz): converting a 2048x2048 page
    // here took 20 ms, a dropped frame where a room first shows it. Other readers of
    // the texture get it converted first (normalize).
    const raw = (fmt === 21 || fmt === 22) && pitch % 4 === 0 && t.swz !== 0;
    if (t.swz > 0 && !raw) this.normalize(t);
    gl.bindTexture(gl.TEXTURE_2D, t.tex);
    gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);
    if (raw) {
      const swz = fmt === 22 ? 2 : 1;
      if (t.swz !== swz) this.keyValid = false;
      t.swz = swz;
      (t.src ??= [])[level] = { w, h, fmt, pitch, bits };
      gl.pixelStorei(gl.UNPACK_ROW_LENGTH, pitch >> 2);
      gl.texImage2D(gl.TEXTURE_2D, level, gl.RGBA8, w, h, 0, gl.RGBA, gl.UNSIGNED_BYTE, src);
      gl.pixelStorei(gl.UNPACK_ROW_LENGTH, 0);
    } else {
      if (t.swz !== 0) this.keyValid = false;
      t.swz = 0;
      gl.texImage2D(gl.TEXTURE_2D, level, gl.RGBA8, w, h, 0, gl.RGBA, gl.UNSIGNED_BYTE, toRGBA(fmt, src, w, h, pitch));
    }
    if (level + 1 > t.uploaded && level === t.uploaded) t.uploaded = level + 1;
    t.params = '';
  }

  /** Texture `t` converted to R, G, B, A from the guest memory its levels came from (still
   * theirs: uploads happen when a draw uses levels the guest changed), for readers other
   * than the fixed-function shader. */
  normalize(t) {
    if (!(t.swz > 0)) return;
    this.flush();
    const gl = this.gl;
    t.swz = 0;
    gl.bindTexture(gl.TEXTURE_2D, t.tex);
    gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);
    for (const [level, s] of (t.src ?? []).entries()) {
      if (s) gl.texImage2D(gl.TEXTURE_2D, level, gl.RGBA8, s.w, s.h, 0, gl.RGBA, gl.UNSIGNED_BYTE, toRGBA(s.fmt, new Uint8Array(this.guestBuffer(), s.bits, s.pitch * s.h), s.w, s.h, s.pitch));
    }
    t.src = null;
    this.keyValid = false;
  }

  textureDelete(id) {
    const t = this.textures.get(id);
    if (!t) return;
    this.flush();
    const gl = this.gl;
    gl.deleteTexture(t.tex);
    if (t.fbo) {
      gl.deleteFramebuffer(t.fbo);
      gl.deleteRenderbuffer(t.depth);
    }
    this.textures.delete(id);
    if (this.target === id) this.bindTarget(0, this.back.w, this.back.h);
  }

  bindTarget(id, w, h) {
    const gl = this.gl;
    const t = id ? this.textures.get(id) : this.back;
    gl.bindFramebuffer(gl.FRAMEBUFFER, t && t.fbo ? t.fbo : this.back.fbo);
    this.target = id;
    this.targetW = w;
    this.targetH = h;
  }

  setTarget(id, w, h) {
    this.flush();
    this.bindTarget(id, w, h);
  }

  clear(flags, color, z, stencil, x, y, w, h) {
    this.flush();
    const gl = this.gl;
    gl.enable(gl.SCISSOR_TEST);
    gl.scissor(x, y, w, h);
    let bits = 0;
    if (flags & 1) {
      gl.colorMask(true, true, true, true);
      gl.clearColor(((color >>> 16) & 255) / 255, ((color >>> 8) & 255) / 255, (color & 255) / 255, ((color >>> 24) & 255) / 255);
      bits |= gl.COLOR_BUFFER_BIT;
    }
    if (flags & 2) {
      gl.depthMask(true);
      gl.clearDepth(z);
      bits |= gl.DEPTH_BUFFER_BIT;
    }
    if (flags & 4) {
      gl.clearStencil(stencil);
      bits |= gl.STENCIL_BUFFER_BIT;
    }
    gl.clear(bits);
    gl.disable(gl.SCISSOR_TEST);
    this.keyValid = false;
  }

  /** One DrawCall record at `ptr` in runtime memory. */
  draw(ptr) {
    const w = new Uint32Array(this.rtBuffer(), ptr, D.words);
    const f = new Float32Array(this.rtBuffer(), ptr, D.words);
    this.draws++;
    const prim = w[D.prim];
    const mode = prim === 1 ? this.gl.POINTS : prim <= 3 ? this.gl.LINES : this.gl.TRIANGLES;
    const fvf = w[D.fvf];
    const rhw = (fvf & 0xe) === 0x4;
    // the same state as the batch so far (compared word by word: every draw asks)
    const k = this.keyWords;
    let same = this.keyValid && this.keyMode === mode && this.keyRhw === rhw;
    for (let i = 0; same && i < KEY_WORDS.length; i++) same = w[KEY_WORDS[i]] === k[i];
    if (!rhw) for (let i = 0; same && i < MATRIX_WORDS; i++) same = w[D.world + i] === k[KEY_WORDS.length + i];
    if (!same || this.count + 6 * w[D.vertexCount] > MAX_BATCH) {
      this.flush();
      this.keyValid = true;
      this.keyMode = mode;
      this.keyRhw = rhw;
      for (let i = 0; i < KEY_WORDS.length; i++) k[i] = w[KEY_WORDS[i]];
      k.set(w.subarray(D.world, D.world + MATRIX_WORDS), KEY_WORDS.length);
      this.applyState(w, f, mode, rhw);
      this.states++;
    }
    this.appendVertices(w, prim);
  }

  applyState(w, f, mode, rhw) {
    const gl = this.gl;
    this.mode = mode;
    const rs = (i) => w[D.rs + i];
    const vp = Array.from(f.subarray(D.viewport, D.viewport + 6));
    gl.viewport(vp[0], vp[1], vp[2], vp[3]);
    gl.depthRange(vp[4], vp[5]);
    gl.uniform1i(this.u.u_rhw, rhw ? 1 : 0);
    gl.uniform2f(this.u.u_target, w[D.targetW], w[D.targetH]);
    gl.uniform4f(this.u.u_vp, vp[0], vp[1], vp[2], vp[3]);
    if (!rhw) {
      const m = mul(mul(f.subarray(D.world, D.world + 16), f.subarray(D.view, D.view + 16)), f.subarray(D.proj, D.proj + 16));
      gl.uniformMatrix4fv(this.u.u_wvp, false, m);
    }
    const swz = [0, 0];
    for (let s = 0; s < 2; s++) {
      const base = s ? D.tss1 : D.tss0;
      const ts = (i) => w[base + i];
      const id = w[D.texture + s];
      const t = id ? this.textures.get(id) : null;
      gl.activeTexture(gl.TEXTURE0 + s);
      gl.bindTexture(gl.TEXTURE_2D, t ? t.tex : this.white);
      if (t) this.samplerParams(t, ts(TSS.ADDRESSU), ts(TSS.ADDRESSV), ts(TSS.MAGFILTER), ts(TSS.MINFILTER), ts(TSS.MIPFILTER));
      swz[s] = t ? Math.max(0, t.swz) : 0;
      // A stage without a texture reads TEXTURE as opaque white.
      gl.uniform4i(s ? this.u.u_stage1 : this.u.u_stage0, ts(TSS.COLOROP), ts(TSS.COLORARG1), ts(TSS.COLORARG2), ts(TSS.ALPHAOP));
      gl.uniform4i(s ? this.u.u_alpha1 : this.u.u_alpha0, ts(TSS.ALPHAARG1), ts(TSS.ALPHAARG2), t ? 1 : 0, 0);
    }
    gl.activeTexture(gl.TEXTURE0);
    gl.uniform2i(this.u.u_swz, swz[0], swz[1]);
    const tf = rs(RS.TEXTUREFACTOR);
    gl.uniform4f(this.u.u_tfactor, ((tf >>> 16) & 255) / 255, ((tf >>> 8) & 255) / 255, (tf & 255) / 255, ((tf >>> 24) & 255) / 255);
    gl.uniform1i(this.u.u_atest, rs(RS.ALPHATESTENABLE) ? rs(RS.ALPHAFUNC) : 8);
    gl.uniform1f(this.u.u_aref, rs(RS.ALPHAREF) & 255);
    // Fog: table mode (pixel) or vertex mode, linear/exp/exp2 (D3DFOG 1..3).
    const fogMode = rs(RS.FOGENABLE) ? (rs(RS.FOGTABLEMODE) || rs(RS.FOGVERTEXMODE)) : 0;
    gl.uniform1i(this.u.u_fog, fogMode === 3 ? 3 : fogMode);
    const fc = rs(RS.FOGCOLOR);
    gl.uniform4f(this.u.u_fogcolor, ((fc >>> 16) & 255) / 255, ((fc >>> 8) & 255) / 255, (fc & 255) / 255, 1);
    gl.uniform3f(this.u.u_fogparams, f[D.rs + RS.FOGSTART], f[D.rs + RS.FOGEND], f[D.rs + RS.FOGDENSITY]);
    this.renderState(rs, mode, null);
  }

  /** Blending, color mask, depth, culling (and the scissor rectangle, when given). */
  renderState(rs, mode, scissor) {
    const gl = this.gl;
    if (rs(RS.ALPHABLENDENABLE)) {
      gl.enable(gl.BLEND);
      const b = (x) => [0, gl.ZERO, gl.ONE, gl.SRC_COLOR, gl.ONE_MINUS_SRC_COLOR, gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA, gl.DST_ALPHA,
        gl.ONE_MINUS_DST_ALPHA, gl.DST_COLOR, gl.ONE_MINUS_DST_COLOR, gl.SRC_ALPHA_SATURATE][x] ?? gl.ONE;
      const pair = (src, dst) => {
        if (src === 12) return [5, 6]; // BOTHSRCALPHA
        if (src === 13) return [6, 5]; // BOTHINVSRCALPHA
        return [src, dst];
      };
      const eq = (op) => [0, gl.FUNC_ADD, gl.FUNC_SUBTRACT, gl.FUNC_REVERSE_SUBTRACT, gl.MIN, gl.MAX][op] ?? gl.FUNC_ADD;
      const [src, dst] = pair(rs(RS.SRCBLEND), rs(RS.DESTBLEND));
      if (rs(RS.SEPARATEALPHABLENDENABLE)) {
        const [sa, da] = pair(rs(RS.SRCBLENDALPHA), rs(RS.DESTBLENDALPHA));
        gl.blendFuncSeparate(b(src), b(dst), b(sa), b(da));
        gl.blendEquationSeparate(eq(rs(RS.BLENDOP)), eq(rs(RS.BLENDOPALPHA)));
      } else {
        gl.blendFunc(b(src), b(dst));
        gl.blendEquation(eq(rs(RS.BLENDOP)));
      }
    } else gl.disable(gl.BLEND);
    const cw = rs(RS.COLORWRITEENABLE);
    gl.colorMask(!!(cw & 1), !!(cw & 2), !!(cw & 4), !!(cw & 8));
    // Depth
    if (rs(RS.ZENABLE)) {
      gl.enable(gl.DEPTH_TEST);
      gl.depthFunc(gl.NEVER + Math.max(0, rs(RS.ZFUNC) - 1));
    } else gl.disable(gl.DEPTH_TEST);
    gl.depthMask(!!rs(RS.ZWRITEENABLE));
    // Culling: stored rows keep D3D's orientation, so D3D clockwise is GL counter-clockwise.
    const cull = rs(RS.CULLMODE);
    gl.frontFace(gl.CCW);
    const tri = mode === gl.TRIANGLES || mode === gl.TRIANGLE_STRIP || mode === gl.TRIANGLE_FAN;
    if (cull === 1 || !tri) gl.disable(gl.CULL_FACE);
    else {
      gl.enable(gl.CULL_FACE);
      gl.cullFace(cull === 3 ? gl.BACK : gl.FRONT);
    }
    if (scissor && rs(RS.SCISSORTESTENABLE)) {
      gl.enable(gl.SCISSOR_TEST);
      gl.scissor(scissor[0], scissor[1], Math.max(0, scissor[2]), Math.max(0, scissor[3]));
    } else gl.disable(gl.SCISSOR_TEST);
  }

  samplerParams(t, u, v, mag, min, mip) {
    const key = `${u},${v},${mag},${min},${mip}`;
    if (t.params === key) return;
    const gl = this.gl;
    t.params = key;
    const wrap = (a) => [gl.REPEAT, gl.REPEAT, gl.MIRRORED_REPEAT, gl.CLAMP_TO_EDGE, gl.CLAMP_TO_EDGE, gl.MIRRORED_REPEAT][a] ?? gl.REPEAT;
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, wrap(u));
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, wrap(v));
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, mag >= 2 ? gl.LINEAR : gl.NEAREST);
    const levels = mip ? t.uploaded : 1;
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAX_LEVEL, levels - 1);
    const lin = min >= 2;
    let mf = lin ? gl.LINEAR : gl.NEAREST;
    if (levels > 1) mf = mip === 1 ? (lin ? gl.LINEAR_MIPMAP_NEAREST : gl.NEAREST_MIPMAP_NEAREST) : lin ? gl.LINEAR_MIPMAP_LINEAR : gl.NEAREST_MIPMAP_LINEAR;
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, mf);
  }

  /** Decodes FVF vertices from guest memory into the batch as a primitive list. */
  appendVertices(w, prim) {
    const fvf = w[D.fvf];
    const stride = w[D.stride];
    const buf = this.guestBuffer();
    const g8 = new DataView(buf);
    const base = w[D.vertices];
    // Offsets inside one FVF vertex.
    const posKind = fvf & 0xe;
    let off = posKind === 0x2 ? 12 : posKind === 0x4 ? 16 : posKind === 0x6 ? 16 : posKind === 0x8 ? 20 : posKind === 0xa ? 24 : posKind === 0xc ? 28 : posKind === 0xe ? 32 : 0;
    if (fvf & 0x10) off += 12;
    if (fvf & 0x20) off += 4;
    const diffuse = fvf & 0x40 ? off : -1;
    if (fvf & 0x40) off += 4;
    const specular = fvf & 0x80 ? off : -1;
    if (fvf & 0x80) off += 4;
    const ntex = (fvf >> 8) & 15;
    const uv = [];
    for (let i = 0; i < ntex; i++) {
      uv.push(off);
      const sz = (fvf >> (16 + 2 * i)) & 3;
      off += sz === 0 ? 8 : sz === 1 ? 12 : sz === 2 ? 16 : 4;
    }
    const rhw = posKind === 0x4;
    let put = (vi) => {
      const p = base + vi * stride;
      const o = this.count * VW;
      this.bf[o] = g8.getFloat32(p, true);
      this.bf[o + 1] = g8.getFloat32(p + 4, true);
      this.bf[o + 2] = g8.getFloat32(p + 8, true);
      this.bf[o + 3] = rhw ? g8.getFloat32(p + 12, true) : 1;
      this.bu[o + 4] = diffuse >= 0 ? g8.getUint32(p + diffuse, true) : 0xffffffff;
      this.bu[o + 5] = specular >= 0 ? g8.getUint32(p + specular, true) : 0;
      this.bf[o + 6] = uv.length > 0 ? g8.getFloat32(p + uv[0], true) : 0;
      this.bf[o + 7] = uv.length > 0 ? g8.getFloat32(p + uv[0] + 4, true) : 0;
      this.bf[o + 8] = uv.length > 1 ? g8.getFloat32(p + uv[1], true) : this.bf[o + 6];
      this.bf[o + 9] = uv.length > 1 ? g8.getFloat32(p + uv[1] + 4, true) : this.bf[o + 7];
      this.count++;
    };
    // Vertices at 4-byte boundaries (nearly all): word views instead of a DataView
    if (((base | stride | Math.max(diffuse, 0) | Math.max(specular, 0) | uv.reduce((a, b) => a | b, 0)) & 3) === 0) {
      if (this.gbuf !== buf) {
        this.gbuf = buf;
        this.gf = new Float32Array(buf);
        this.gu = new Uint32Array(buf);
      }
      const gf = this.gf, gu = this.gu, bf = this.bf, bu = this.bu;
      const sw = stride >> 2, dw = diffuse >> 2, sp = specular >> 2, u0 = uv.length > 0 ? uv[0] >> 2 : -1, u1 = uv.length > 1 ? uv[1] >> 2 : -1;
      put = (vi) => {
        const p = (base >> 2) + vi * sw;
        const o = this.count * VW;
        bf[o] = gf[p];
        bf[o + 1] = gf[p + 1];
        bf[o + 2] = gf[p + 2];
        bf[o + 3] = rhw ? gf[p + 3] : 1;
        bu[o + 4] = dw >= 0 ? gu[p + dw] : 0xffffffff;
        bu[o + 5] = sp >= 0 ? gu[p + sp] : 0;
        bf[o + 6] = u0 >= 0 ? gf[p + u0] : 0;
        bf[o + 7] = u0 >= 0 ? gf[p + u0 + 1] : 0;
        bf[o + 8] = u1 >= 0 ? gf[p + u1] : bf[o + 6];
        bf[o + 9] = u1 >= 0 ? gf[p + u1 + 1] : bf[o + 7];
        this.count++;
      };
    }
    const n = w[D.primCount];
    const ip = w[D.indices];
    const isz = w[D.indexSize];
    const idx = ip ? (i) => (isz === 4 ? g8.getUint32(ip + 4 * i, true) : g8.getUint16(ip + 2 * i, true)) : (i) => i;
    switch (prim) {
      case 1: for (let i = 0; i < n; i++) put(idx(i)); break;
      case 2: for (let i = 0; i < 2 * n; i++) put(idx(i)); break;
      case 3: for (let i = 0; i < n; i++) { put(idx(i)); put(idx(i + 1)); } break;
      case 4: for (let i = 0; i < 3 * n; i++) put(idx(i)); break;
      case 5:
        for (let i = 0; i < n; i++) {
          // Strips alternate winding; keep D3D's orientation for culling.
          if (i & 1) { put(idx(i + 1)); put(idx(i)); put(idx(i + 2)); } else { put(idx(i)); put(idx(i + 1)); put(idx(i + 2)); }
        }
        break;
      case 6: for (let i = 0; i < n; i++) { put(idx(0)); put(idx(i + 1)); put(idx(i + 2)); } break;
    }
  }

  flush() {
    if (!this.count) return;
    const gl = this.gl;
    gl.bindBuffer(gl.ARRAY_BUFFER, this.vbo);
    gl.bufferData(gl.ARRAY_BUFFER, this.bf.subarray(0, this.count * VW), gl.STREAM_DRAW);
    gl.drawArrays(this.mode, 0, this.count);
    this.count = 0;
  }

  /** The framebuffer of target `id` (0: back buffer), made for plain textures on demand. */
  targetFbo(id) {
    if (!id) return this.back?.fbo;
    const t = this.textures.get(id);
    if (!t) return null;
    this.normalize(t);
    if (!t.fbo) Object.assign(t, this.framebuffer(t.w, t.h, t.tex));
    return t.fbo;
  }

  /** StretchRect: both sides hold rows in D3D order, so no flip. */
  blit(src, sx, sy, sw, sh, dst, dx, dy, dw, dh, linear) {
    this.flush();
    const gl = this.gl;
    const s = this.targetFbo(src);
    const d = this.targetFbo(dst);
    if (!s || !d) return;
    gl.bindFramebuffer(gl.READ_FRAMEBUFFER, s);
    gl.bindFramebuffer(gl.DRAW_FRAMEBUFFER, d);
    gl.disable(gl.SCISSOR_TEST);
    gl.colorMask(true, true, true, true);
    gl.blitFramebuffer(sx, sy, sx + sw, sy + sh, dx, dy, dx + dw, dy + dh, gl.COLOR_BUFFER_BIT, linear ? gl.LINEAR : gl.NEAREST);
    this.bindTarget(this.target, this.targetW, this.targetH);
    this.keyValid = false;
  }

  /** GetRenderTargetData: target pixels into guest memory as B, G, R, A bytes. */
  readTarget(id, w, h, dst, pitch) {
    this.flush();
    const gl = this.gl;
    const f = this.targetFbo(id);
    if (!f) return;
    gl.bindFramebuffer(gl.READ_FRAMEBUFFER, f);
    const px = new Uint8Array(w * h * 4);
    gl.readPixels(0, 0, w, h, gl.RGBA, gl.UNSIGNED_BYTE, px);
    this.bindTarget(this.target, this.targetW, this.targetH);
    const out = new Uint8Array(this.guestBuffer());
    for (let y = 0; y < h; y++) {
      const o = dst + y * pitch;
      for (let x = 0; x < w; x++) {
        const i = 4 * (y * w + x);
        out[o + 4 * x] = px[i + 2];
        out[o + 4 * x + 1] = px[i + 1];
        out[o + 4 * x + 2] = px[i];
        out[o + 4 * x + 3] = px[i + 3];
      }
    }
  }

  shaderCreate(id, vertex, glsl) {
    (this.shaders ??= new Map()).set(id, { vertex, glsl });
    // Compile now so a translation problem shows up when the game creates the shader.
    const gl = this.gl;
    const s = gl.createShader(vertex ? gl.VERTEX_SHADER : gl.FRAGMENT_SHADER);
    gl.shaderSource(s, glsl.replace('// @varyings', '').replace('// @init', ''));
    gl.compileShader(s);
    if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) this.print(`shader ${id.toString(16)}: ${gl.getShaderInfoLog(s)}`);
    gl.deleteShader(s);
  }

  shaderDelete(id) {
    this.shaders?.delete(id);
    for (const [k, p] of this.programs ?? []) {
      if (k.split('|').includes(String(id))) {
        this.gl.deleteProgram(p.prog);
        this.programs.delete(k);
      }
    }
  }

  /** The program for a vertex/pixel shader pair (translated by crates/runtime/src/hle/d3d9_shader.rs). */
  program9(vsId, psId) {
    const key = `${vsId}|${psId}`;
    this.programs ??= new Map();
    let p = this.programs.get(key);
    if (p !== undefined) return p;
    const gl = this.gl;
    const vs = this.shaders?.get(vsId), ps = this.shaders?.get(psId);
    p = null;
    if (vs && ps) {
      // Pixel shader inputs the vertex shader does not write read as zero.
      const written = new Set([...vs.glsl.matchAll(/^out vec4 (v_\w+);/gm)].map((m) => m[1]));
      const missing = [...ps.glsl.matchAll(/^in vec4 (v_\w+);/gm)].map((m) => m[1]).filter((v) => !written.has(v));
      const vsrc = vs.glsl
        .replace('// @varyings', missing.map((v) => `out vec4 ${v};`).join('\n'))
        .replace('// @init', missing.map((v) => `${v} = vec4(0.0);`).join('\n'));
      try {
        const prog = gl.createProgram();
        gl.attachShader(prog, compile(gl, gl.VERTEX_SHADER, vsrc));
        gl.attachShader(prog, compile(gl, gl.FRAGMENT_SHADER, ps.glsl));
        gl.linkProgram(prog);
        if (!gl.getProgramParameter(prog, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(prog));
        const u = (n) => gl.getUniformLocation(prog, n);
        // attribute name -> (location, input register)
        const attrs = new Map();
        for (const m of vs.glsl.matchAll(/^in vec4 a_(\w+); \/\/ v(\d+)/gm)) attrs.set(m[1], { loc: gl.getAttribLocation(prog, `a_${m[1]}`), v: Number(m[2]) });
        const samplers = [...ps.glsl.matchAll(/uniform highp \w+ s(\d+);/g)].map((m) => Number(m[1]));
        gl.useProgram(prog);
        for (const s of samplers) gl.uniform1i(u(`s${s}`), s);
        p = { prog, attrs, samplers, vc: u('vc'), vi: u('vi'), vb: u('vb'), pc: u('pc'), pi: u('pi'), pb: u('pb'), fixup: u('u_fixup'), bgra: u('u_bgra'), atest: u('u_atest'), aref: u('u_aref') };
      } catch (e) {
        this.print(`shaders ${vsId.toString(16)}/${psId.toString(16)}: ${e.message}`);
      }
    }
    this.programs.set(key, p);
    return p;
  }

  /** One DrawCall9 record: a draw through translated vertex and pixel shaders. */
  draw9(ptr) {
    const w = new Uint32Array(this.rtBuffer(), ptr, D9.words);
    const f = new Float32Array(this.rtBuffer(), ptr, D9.words);
    const i32 = new Int32Array(this.rtBuffer(), ptr, D9.words);
    const p = this.program9(w[D9.vs], w[D9.ps]);
    if (!p) return;
    this.flush();
    this.draws++;
    const gl = this.gl;
    const guest = this.guestBuffer();
    gl.useProgram(p.prog);
    this.vao9 ??= gl.createVertexArray();
    gl.bindVertexArray(this.vao9);
    this.vbo9 ??= [0, 1, 2, 3].map(() => gl.createBuffer());
    this.ibo9 ??= gl.createBuffer();
    const prim = w[D9.prim], n = w[D9.primCount], min = w[D9.minIndex], count = w[D9.vertexCount];
    // Vertex data of each stream used, from vertex `min` on.
    const used = new Set();
    for (let e = 0; e < w[D9.elementCount]; e++) used.add(w[D9.elements + 4 * e]);
    for (const s of used) {
      const addr = w[D9.streams + 2 * s], stride = w[D9.streams + 2 * s + 1];
      if (!addr || !stride) continue;
      gl.bindBuffer(gl.ARRAY_BUFFER, this.vbo9[s]);
      gl.bufferData(gl.ARRAY_BUFFER, new Uint8Array(guest, addr + min * stride, count * stride), gl.STREAM_DRAW);
    }
    let bgra = 0;
    const bound = new Set();
    for (let e = 0; e < w[D9.elementCount]; e++) {
      const [stream, offset, type, use] = [0, 1, 2, 3].map((k) => w[D9.elements + 4 * e + k]);
      const a = p.attrs.get(`${USAGE[use & 0xff] ?? 'UNKNOWN'}${use >> 8}`);
      if (!a || a.loc < 0 || bound.has(a.loc)) continue;
      const fmt = DECL[type];
      if (!fmt) continue;
      bound.add(a.loc);
      gl.bindBuffer(gl.ARRAY_BUFFER, this.vbo9[stream]);
      gl.enableVertexAttribArray(a.loc);
      gl.vertexAttribPointer(a.loc, fmt[0], gl[fmt[1]], fmt[2], w[D9.streams + 2 * stream + 1], offset);
      if (type === 4) bgra |= 1 << a.v;
    }
    for (const a of p.attrs.values()) {
      if (a.loc >= 0 && !bound.has(a.loc)) {
        gl.disableVertexAttribArray(a.loc);
        gl.vertexAttrib4f(a.loc, 0, 0, 0, 1);
      }
    }
    // Constants
    gl.uniform4fv(p.vc, f.subarray(D9.vsF, D9.vsF + 1024));
    gl.uniform4iv(p.vi, i32.subarray(D9.vsI, D9.vsI + 64));
    gl.uniform1iv(p.vb, i32.subarray(D9.vsB, D9.vsB + 16));
    gl.uniform4fv(p.pc, f.subarray(D9.psF, D9.psF + 896));
    gl.uniform4iv(p.pi, i32.subarray(D9.psI, D9.psI + 64));
    gl.uniform1iv(p.pb, i32.subarray(D9.psB, D9.psB + 16));
    const vp = Array.from(f.subarray(D9.viewport, D9.viewport + 6));
    gl.viewport(vp[0], vp[1], vp[2], vp[3]);
    gl.depthRange(vp[4], vp[5]);
    gl.uniform2f(p.fixup, 1 / Math.max(1, vp[2]), 1 / Math.max(1, vp[3]));
    gl.uniform1i(p.bgra, bgra);
    const rs = (i) => w[D9.rs + i];
    gl.uniform1i(p.atest, rs(RS.ALPHATESTENABLE) ? rs(RS.ALPHAFUNC) : 8);
    gl.uniform1f(p.aref, rs(RS.ALPHAREF) & 255);
    // Textures with their sampler states (in R, G, B, A for the game's shaders)
    for (const s of p.samplers) {
      const id = w[D9.textures + 3 * s];
      const t = id ? this.textures.get(id) : null;
      if (t) this.normalize(t);
    }
    for (const s of p.samplers) {
      const id = w[D9.textures + 3 * s];
      const t = id ? this.textures.get(id) : null;
      gl.activeTexture(gl.TEXTURE0 + s);
      gl.bindTexture(gl.TEXTURE_2D, t ? t.tex : this.white);
      const sm = (k) => w[D9.samplers + 14 * s + k];
      if (t) this.samplerParams(t, sm(1), sm(2), sm(5), sm(6), sm(7));
    }
    gl.activeTexture(gl.TEXTURE0);
    const mode = [0, gl.POINTS, gl.LINES, gl.LINE_STRIP, gl.TRIANGLES, gl.TRIANGLE_STRIP, gl.TRIANGLE_FAN][prim] ?? gl.TRIANGLES;
    this.renderState(rs, mode, Array.from(i32.subarray(D9.scissor, D9.scissor + 4)));
    const vcount = [0, n, 2 * n, n + 1, 3 * n, n + 2, n + 2][prim] ?? 0;
    if (w[D9.indices]) {
      const size = w[D9.indexSize];
      const src = size === 4 ? new Uint32Array(guest.slice(w[D9.indices], w[D9.indices] + 4 * vcount)) : new Uint16Array(guest.slice(w[D9.indices], w[D9.indices] + 2 * vcount));
      if (min) for (let k = 0; k < src.length; k++) src[k] -= min;
      gl.bindBuffer(gl.ELEMENT_ARRAY_BUFFER, this.ibo9);
      gl.bufferData(gl.ELEMENT_ARRAY_BUFFER, src, gl.STREAM_DRAW);
      gl.drawElements(mode, vcount, size === 4 ? gl.UNSIGNED_INT : gl.UNSIGNED_SHORT, 0);
    } else {
      gl.drawArrays(mode, 0, vcount);
    }
    // Back to the fixed-function renderer's program and vertex layout.
    gl.useProgram(this.prog);
    gl.bindVertexArray(this.vao);
    gl.bindBuffer(gl.ARRAY_BUFFER, this.vbo);
    this.keyValid = false;
  }

  /** The back buffer as a PNG (ArrayBuffer). */
  async snapshot() {
    this.flush();
    const gl = this.gl;
    const b = this.back;
    const [sx, sy, sw, sh] = this.shown ?? [0, 0, b.w, b.h];
    gl.bindFramebuffer(gl.READ_FRAMEBUFFER, b.fbo);
    const px = new Uint8Array(sw * sh * 4);
    gl.readPixels(sx, sy, sw, sh, gl.RGBA, gl.UNSIGNED_BYTE, px);
    this.bindTarget(this.target, this.targetW, this.targetH);
    const c = new OffscreenCanvas(sw, sh);
    const img = new ImageData(new Uint8ClampedArray(px.buffer), sw, sh);
    // Rows are stored top-down already (D3D order); the frame is opaque.
    for (let i = 3; i < img.data.length; i += 4) img.data[i] = 255;
    c.getContext('2d').putImageData(img, 0, 0);
    return (await c.convertToBlob({ type: 'image/png' })).arrayBuffer();
  }

  /** Shows rectangle (x, y, w, h) of the back buffer at dw x dh, flipping D3D row order. */
  present(x = 0, y = 0, w = this.back?.w ?? 0, h = this.back?.h ?? 0, dw = w, dh = h) {
    this.flush();
    const gl = this.gl;
    const b = this.back;
    if (!b) return;
    if (this.canvas.width !== dw || this.canvas.height !== dh) {
      this.canvas.width = dw;
      this.canvas.height = dh;
    }
    this.shown = [x, y, w, h];
    gl.bindFramebuffer(gl.READ_FRAMEBUFFER, b.fbo);
    gl.bindFramebuffer(gl.DRAW_FRAMEBUFFER, null);
    gl.disable(gl.SCISSOR_TEST);
    gl.colorMask(true, true, true, true);
    // Stored rows are D3D rows: source row y is at the top of the canvas.
    gl.blitFramebuffer(x, y, x + w, y + h, 0, dh, dw, 0, gl.COLOR_BUFFER_BIT, gl.NEAREST);
    this.bindTarget(this.target, this.targetW, this.targetH);
    this.keyValid = false;
    this.frames++;
  }

}
