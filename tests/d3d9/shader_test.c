/* Direct3D 9 shader pipeline test, run through the runtime like a game.
 *
 * Compiles HLSL at run time with d3dcompiler_43 (as GameMaker: Studio does), draws a
 * textured quad through a vertex and a pixel shader into a render target, reads it
 * back with GetRenderTargetData and compares sampled pixels with the same arithmetic
 * done on the CPU. Covers vs/ps 2.0 and 3.0: constants (a matrix that mirrors x),
 * vertex colour and texture coordinates, point sampling, dot, lerp, frac and a branch.
 * Prints one line per check and "SHADER TEST PASS" / "SHADER TEST FAIL"; exits 0 on
 * success. Build: i686-w64-mingw32-gcc -O2 -o shader_test.exe shader_test.c -ld3d9
 */
#define COBJMACROS
#include <windows.h>
#include <d3d9.h>
#include <math.h>
#include <stdio.h>

typedef struct Blob Blob;
typedef struct {
    HRESULT(WINAPI *QueryInterface)(Blob *, REFIID, void **);
    ULONG(WINAPI *AddRef)(Blob *);
    ULONG(WINAPI *Release)(Blob *);
    void *(WINAPI *GetBufferPointer)(Blob *);
    SIZE_T(WINAPI *GetBufferSize)(Blob *);
} BlobVtbl;
struct Blob { const BlobVtbl *v; };
typedef HRESULT(WINAPI *Compile)(const void *, SIZE_T, const char *, const void *, void *, const char *, const char *, UINT,
                                 UINT, Blob **, Blob **);

static const char *VS =
    "float4x4 m : register(c0);\n"
    "struct V { float4 pos : POSITION; float4 col : COLOR0; float2 uv : TEXCOORD0; };\n"
    "V main(V i) { V o; o.pos = mul(i.pos, m); o.col = i.col; o.uv = i.uv; return o; }\n";
static const char *PS =
    "sampler2D s0 : register(s0);\n"
    "float4 k : register(c0);\n"
    "float4 main(float4 col : COLOR0, float2 uv : TEXCOORD0) : COLOR0 {\n"
    "  float4 t = tex2D(s0, uv);\n"
    "  float g = dot(t.rgb, float3(0.299, 0.587, 0.114));\n"
    "  float4 r = lerp(t * col, float4(g, g, g, t.a), k.x);\n"
    "  if (uv.x > 0.5) r.b = frac(uv.y * 4);\n"
    "  return saturate(r * k.y + k.z);\n"
    "}\n";

#define W 64
#define TEX 4
static const float K[4] = {0.25f, 0.9f, 0.05f, 0.0f};
static const float COL[4] = {0.8f, 0.6f, 0.4f, 1.0f}; /* r g b a */

static unsigned texel(int x, int y) { /* A8R8G8B8 */
    unsigned a = 255 - 40 * x, r = 10 + 60 * x, g = 20 + 60 * y, b = 5 + 25 * (x + y);
    return a << 24 | r << 16 | g << 8 | b;
}

static float clamp01(float v) { return v < 0 ? 0 : v > 1 ? 1 : v; }

/* Expected RGBA (0..255) at pixel (px, py) for texture coordinates sampled at that
 * pixel: u, v in [0, 1). The x mirror of the vertex shader is applied by the caller. */
static void expect(float u, float v, float out[4]) {
    int tx = (int)(u * TEX), ty = (int)(v * TEX);
    unsigned c = texel(tx, ty);
    float t[4] = {((c >> 16) & 255) / 255.0f, ((c >> 8) & 255) / 255.0f, (c & 255) / 255.0f, (c >> 24) / 255.0f};
    float g = t[0] * 0.299f + t[1] * 0.587f + t[2] * 0.114f;
    float r[4];
    for (int i = 0; i < 4; i++) {
        float a = t[i] * COL[i], b = i < 3 ? g : t[3];
        r[i] = a + (b - a) * K[0];
    }
    if (u > 0.5f) r[2] = v * 4 - floorf(v * 4);
    for (int i = 0; i < 4; i++) out[i] = clamp01(r[i] * K[1] + K[2]) * 255.0f;
}

static int run(IDirect3DDevice9 *dev, Compile compile, const char *vsp, const char *psp) {
    Blob *code = 0, *err = 0;
    IDirect3DVertexShader9 *vs = 0;
    IDirect3DPixelShader9 *ps = 0;
    if (FAILED(compile(VS, strlen(VS), "vs", 0, 0, "main", vsp, 0, 0, &code, &err))) {
        printf("%s: compile failed: %s\n", vsp, err ? (char *)err->v->GetBufferPointer(err) : "?");
        return 0;
    }
    IDirect3DDevice9_CreateVertexShader(dev, code->v->GetBufferPointer(code), &vs);
    code->v->Release(code);
    if (FAILED(compile(PS, strlen(PS), "ps", 0, 0, "main", psp, 0, 0, &code, &err))) {
        printf("%s: compile failed: %s\n", psp, err ? (char *)err->v->GetBufferPointer(err) : "?");
        return 0;
    }
    IDirect3DDevice9_CreatePixelShader(dev, code->v->GetBufferPointer(code), &ps);
    code->v->Release(code);

    IDirect3DTexture9 *tex = 0;
    IDirect3DDevice9_CreateTexture(dev, TEX, TEX, 1, 0, D3DFMT_A8R8G8B8, D3DPOOL_MANAGED, &tex, 0);
    D3DLOCKED_RECT lr;
    IDirect3DTexture9_LockRect(tex, 0, &lr, 0, 0);
    for (int y = 0; y < TEX; y++)
        for (int x = 0; x < TEX; x++) ((unsigned *)((char *)lr.pBits + y * lr.Pitch))[x] = texel(x, y);
    IDirect3DTexture9_UnlockRect(tex, 0);

    IDirect3DSurface9 *rt = 0, *sys = 0, *back = 0;
    IDirect3DDevice9_CreateRenderTarget(dev, W, W, D3DFMT_A8R8G8B8, D3DMULTISAMPLE_NONE, 0, FALSE, &rt, 0);
    IDirect3DDevice9_CreateOffscreenPlainSurface(dev, W, W, D3DFMT_A8R8G8B8, D3DPOOL_SYSTEMMEM, &sys, 0);
    IDirect3DDevice9_GetRenderTarget(dev, 0, &back);
    IDirect3DDevice9_SetRenderTarget(dev, 0, rt);
    IDirect3DDevice9_Clear(dev, 0, 0, D3DCLEAR_TARGET, 0xFF000000, 1.0f, 0);

    /* mirror x: o.pos = mul(pos, m) with m = diag(-1, 1, 1, 1) */
    float m[16] = {-1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1};
    IDirect3DDevice9_SetVertexShaderConstantF(dev, 0, m, 4);
    IDirect3DDevice9_SetPixelShaderConstantF(dev, 0, K, 1);
    D3DVERTEXELEMENT9 el[] = {{0, 0, D3DDECLTYPE_FLOAT4, 0, D3DDECLUSAGE_POSITION, 0},
                              {0, 16, D3DDECLTYPE_D3DCOLOR, 0, D3DDECLUSAGE_COLOR, 0},
                              {0, 20, D3DDECLTYPE_FLOAT2, 0, D3DDECLUSAGE_TEXCOORD, 0},
                              D3DDECL_END()};
    IDirect3DVertexDeclaration9 *decl = 0;
    IDirect3DDevice9_CreateVertexDeclaration(dev, el, &decl);
    struct { float x, y, z, w; DWORD c; float u, v; } q[4];
    DWORD c = D3DCOLOR_ARGB(255, (int)(COL[0] * 255 + 0.5f), (int)(COL[1] * 255 + 0.5f), (int)(COL[2] * 255 + 0.5f));
    float pos[4][4] = {{-1, 1, 0.5f, 1}, {1, 1, 0.5f, 1}, {-1, -1, 0.5f, 1}, {1, -1, 0.5f, 1}};
    float uv[4][2] = {{0, 0}, {1, 0}, {0, 1}, {1, 1}};
    for (int i = 0; i < 4; i++) {
        q[i].x = pos[i][0]; q[i].y = pos[i][1]; q[i].z = pos[i][2]; q[i].w = pos[i][3];
        q[i].c = c; q[i].u = uv[i][0]; q[i].v = uv[i][1];
    }
    IDirect3DDevice9_BeginScene(dev);
    IDirect3DDevice9_SetVertexDeclaration(dev, decl);
    IDirect3DDevice9_SetVertexShader(dev, vs);
    IDirect3DDevice9_SetPixelShader(dev, ps);
    IDirect3DDevice9_SetTexture(dev, 0, (IDirect3DBaseTexture9 *)tex);
    IDirect3DDevice9_SetSamplerState(dev, 0, D3DSAMP_MINFILTER, D3DTEXF_POINT);
    IDirect3DDevice9_SetSamplerState(dev, 0, D3DSAMP_MAGFILTER, D3DTEXF_POINT);
    IDirect3DDevice9_SetSamplerState(dev, 0, D3DSAMP_ADDRESSU, D3DTADDRESS_CLAMP);
    IDirect3DDevice9_SetSamplerState(dev, 0, D3DSAMP_ADDRESSV, D3DTADDRESS_CLAMP);
    IDirect3DDevice9_SetRenderState(dev, D3DRS_CULLMODE, D3DCULL_NONE);
    IDirect3DDevice9_SetRenderState(dev, D3DRS_ZENABLE, FALSE);
    IDirect3DDevice9_SetRenderState(dev, D3DRS_ALPHABLENDENABLE, FALSE);
    IDirect3DDevice9_DrawPrimitiveUP(dev, D3DPT_TRIANGLESTRIP, 2, q, sizeof q[0]);
    IDirect3DDevice9_EndScene(dev);
    IDirect3DDevice9_GetRenderTargetData(dev, rt, sys);

    int ok = 1;
    IDirect3DSurface9_LockRect(sys, &lr, 0, D3DLOCK_READONLY);
    int pts[4] = {6, 26, 38, 58};
    for (int j = 0; j < 4; j++)
        for (int i = 0; i < 4; i++) {
            int px = pts[i], py = pts[j];
            unsigned got = ((unsigned *)((char *)lr.pBits + py * lr.Pitch))[px];
            float g[4] = {(got >> 16) & 255, (got >> 8) & 255, got & 255, got >> 24};
            /* either pixel-centre convention; u mirrored by the vertex shader */
            int match = 0;
            for (int h = 0; h < 2 && !match; h++) {
                float e[4], f = h * 0.5f;
                expect(1.0f - (px + f) / W, (py + f) / W, e);
                match = 1;
                for (int k = 0; k < 4; k++) match &= fabsf(e[k] - g[k]) <= 3.0f;
            }
            if (!match) {
                float e[4];
                expect(1.0f - px / (float)W, py / (float)W, e);
                printf("%s/%s (%d,%d): got %3.0f %3.0f %3.0f %3.0f, expected %3.0f %3.0f %3.0f %3.0f\n", vsp, psp, px, py,
                       g[0], g[1], g[2], g[3], e[0], e[1], e[2], e[3]);
                ok = 0;
            }
        }
    IDirect3DSurface9_UnlockRect(sys);
    printf("%s/%s: %s\n", vsp, psp, ok ? "ok" : "MISMATCH");

    /* show it */
    IDirect3DDevice9_SetRenderTarget(dev, 0, back);
    IDirect3DDevice9_StretchRect(dev, rt, 0, back, 0, D3DTEXF_POINT);
    for (int f = 0; f < 10; f++) IDirect3DDevice9_Present(dev, 0, 0, 0, 0);
    IDirect3DDevice9_SetVertexShader(dev, 0);
    IDirect3DDevice9_SetPixelShader(dev, 0);
    IDirect3DDevice9_SetTexture(dev, 0, 0);
    IDirect3DSurface9_Release(back);
    IDirect3DSurface9_Release(rt);
    IDirect3DSurface9_Release(sys);
    IDirect3DTexture9_Release(tex);
    IDirect3DVertexDeclaration9_Release(decl);
    IDirect3DVertexShader9_Release(vs);
    IDirect3DPixelShader9_Release(ps);
    return ok;
}

int main(void) {
    HMODULE dc = LoadLibraryA("d3dcompiler_43.dll");
    Compile compile = dc ? (Compile)GetProcAddress(dc, "D3DCompile") : 0;
    if (!compile) {
        printf("d3dcompiler_43.dll / D3DCompile not available\nSHADER TEST FAIL\n");
        fflush(stdout);
        ExitProcess(2);
    }
    WNDCLASSA wc = {0};
    wc.lpfnWndProc = DefWindowProcA;
    wc.hInstance = GetModuleHandleA(0);
    wc.lpszClassName = "shadertest";
    RegisterClassA(&wc);
    HWND hwnd = CreateWindowA("shadertest", "shader test", WS_OVERLAPPEDWINDOW | WS_VISIBLE, 0, 0, 256, 256, 0, 0, wc.hInstance, 0);
    IDirect3D9 *d3d = Direct3DCreate9(D3D_SDK_VERSION);
    D3DPRESENT_PARAMETERS pp = {0};
    pp.Windowed = TRUE;
    pp.SwapEffect = D3DSWAPEFFECT_DISCARD;
    pp.BackBufferWidth = W;
    pp.BackBufferHeight = W;
    pp.BackBufferFormat = D3DFMT_A8R8G8B8;
    pp.hDeviceWindow = hwnd;
    IDirect3DDevice9 *dev = 0;
    if (FAILED(IDirect3D9_CreateDevice(d3d, 0, D3DDEVTYPE_HAL, hwnd, D3DCREATE_SOFTWARE_VERTEXPROCESSING, &pp, &dev))) {
        printf("CreateDevice failed\nSHADER TEST FAIL\n");
        fflush(stdout);
        ExitProcess(2);
    }
    int ok = run(dev, compile, "vs_2_0", "ps_2_0");
    ok &= run(dev, compile, "vs_3_0", "ps_3_0");
    printf("SHADER TEST %s\n", ok ? "PASS" : "FAIL");
    fflush(stdout);
    IDirect3DDevice9_Release(dev);
    IDirect3D9_Release(d3d);
    ExitProcess(ok ? 0 : 1);
}
