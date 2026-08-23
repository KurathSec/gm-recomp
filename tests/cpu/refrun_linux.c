/*
 * Native reference executor for CPU differential tests: a freestanding 32-bit
 * Linux program (no libc), so it runs directly on the host CPU with no Wine.
 *
 * Same "GMR2" protocol as the harness expects (see crates/harness/src/proto.rs):
 *   batch:    "GMR2", u32 count, cases...
 *   case in:  u32 code_len, code[code_len], regs[8] (esp = offset into the stack
 *             window), u32 eflags, fx[512], data[DATA_SIZE], stack[STACK_SIZE]
 *   case out: u32 status, u32 fault_off, u32 stack_base, regs[8], u32 eflags,
 *             fx[512], data[DATA_SIZE], stack[STACK_SIZE]
 *
 * A case runs until its code faults; `hlt` is the normal end (SIGSEGV from #GP).
 * The signal handler reads the interrupted context (registers, EFLAGS and the
 * kernel-saved FXSAVE state), rewrites the context to resume the harness and
 * returns. Signals are delivered on an alternate stack so the test's stack
 * window is never touched by the kernel.
 *
 * Build: gcc -m32 -O1 -nostdlib -static -ffreestanding -fno-builtin -fno-stack-protector -fno-pic
 */
typedef unsigned char u8;
typedef unsigned short u16;
typedef unsigned int u32;
typedef int i32;
typedef unsigned long ulong;

#define DATA_BASE   0x20000000u
#define DATA_SIZE   0x800u
#define CODE_BASE   0x20001000u
#define CODE_MAX    0x1000u
#define STACK_BASE  0x20002000u
#define STACK_SIZE  0x400u
#define BLOCK_SIZE  0x3000u

/* Windows-style status codes, so the harness compares the same values. */
#define ST_OK            0x00000000u
#define ST_BREAKPOINT    0x80000003u
#define ST_ACCESS        0xC0000005u
#define ST_ILLEGAL       0xC000001Du
#define ST_DIVIDE        0xC0000094u

/* ---- syscalls ---------------------------------------------------------- */
static long sys3(long n, long a, long b, long c) {
    long r;
    __asm__ volatile("int $0x80" : "=a"(r) : "a"(n), "b"(a), "c"(b), "d"(c) : "memory");
    return r;
}
static long sys4(long n, long a, long b, long c, long d) {
    long r;
    __asm__ volatile("int $0x80" : "=a"(r) : "a"(n), "b"(a), "c"(b), "d"(c), "S"(d) : "memory");
    return r;
}
#define SYS_exit 1
#define SYS_read 3
#define SYS_write 4
#define SYS_open 5
#define SYS_close 6
#define SYS_mmap 90
#define SYS_rt_sigreturn 173
#define SYS_rt_sigaction 174
#define SYS_sigaltstack 186

static void sys_exit(int code) { sys3(SYS_exit, code, 0, 0); for (;;) {} }
static long sys_write(int fd, const void *p, u32 n) { return sys3(SYS_write, fd, (long)p, n); }
static long sys_read(int fd, void *p, u32 n) { return sys3(SYS_read, fd, (long)p, n); }

static void *memcpy(void *d, const void *s, u32 n) { u8 *a = d; const u8 *b = s; while (n--) *a++ = *b++; return d; }
static void *memset(void *d, int c, u32 n) { u8 *a = d; while (n--) *a++ = (u8)c; return d; }
static u32 strlen_(const char *s) { u32 n = 0; while (s[n]) n++; return n; }
static void die(const char *msg) { sys_write(2, msg, strlen_(msg)); sys_write(2, "\n", 1); sys_exit(2); }

static int read_full(int fd, void *p, u32 n) {
    u8 *b = p;
    while (n) {
        long r = sys_read(fd, b, n);
        if (r <= 0) return 0;
        b += r; n -= r;
    }
    return 1;
}
static int write_full(int fd, const void *p, u32 n) {
    const u8 *b = p;
    while (n) {
        long r = sys_write(fd, b, n);
        if (r <= 0) return 0;
        b += r; n -= r;
    }
    return 1;
}

/* ---- kernel structures (i386) ----------------------------------------- */
struct kernel_sigaction {
    void (*handler)(int, void *, void *);
    ulong flags;
    void (*restorer)(void);
    ulong mask[2];
};
#define SA_SIGINFO   0x00000004
#define SA_ONSTACK   0x08000000
#define SA_RESTORER  0x04000000
#define SA_NODEFER   0x40000000
#define SIGILL 4
#define SIGTRAP 5
#define SIGBUS 7
#define SIGFPE 8
#define SIGSEGV 11

struct stack_t_ { void *sp; int flags; u32 size; };

/* i386 struct sigcontext */
struct sigcontext_ {
    u32 gs, fs, es, ds, edi, esi, ebp, esp, ebx, edx, ecx, eax;
    u32 trapno, err, eip, cs, eflags, esp_at_signal, ss;
    u8 *fpstate;
    u32 oldmask, cr2;
};
struct ucontext_ {
    u32 flags;
    u32 link;
    struct stack_t_ stack;
    struct sigcontext_ mc;
};

/* i386 struct _fpstate offsets */
#define FP_CW 0
#define FP_SW 4
#define FP_TAG 8
#define FP_ST 28        /* 8 x 10 bytes */
#define FP_MAGIC 110    /* u16: 0 => FXSR data follows */
#define FP_MXCSR 136
#define FP_FXSR_ST 144  /* 8 x 16 */
#define FP_XMM 272      /* 8 x 16 */

/* ---- protocol buffers --------------------------------------------------- */
#pragma pack(push, 1)
struct case_in_body {
    u32 regs[8];
    u32 eflags;
    u8 fx[512];
    u8 data[DATA_SIZE];
    u8 stack[STACK_SIZE];
};
struct case_out {
    u32 status;
    u32 fault_off;
    u32 stack_base;
    u32 regs[8];
    u32 eflags;
    u8 fx[512];
    u8 data[DATA_SIZE];
    u8 stack[STACK_SIZE];
};
#pragma pack(pop)

static struct case_out g_out;
static struct case_in_body g_ci;
static u8 g_code[CODE_MAX];
u8 g_fx_in[512] __attribute__((aligned(16)));
u32 g_saved_esp, g_saved_ebp, g_saved_ebx, g_saved_esi, g_saved_edi;
u32 g_in_regs[8], g_in_eflags;
u32 g_code_entry = CODE_BASE;
static u8 g_altstack[65536] __attribute__((aligned(16)));
static int g_debug;
extern void resume_point(void);

static void fxsave_from_fpstate(const u8 *fp, u8 *fx) {
    memset(fx, 0, 512);
    if (!fp) return;
    u32 cw = *(u32 *)(fp + FP_CW), sw = *(u32 *)(fp + FP_SW), tag = *(u32 *)(fp + FP_TAG);
    *(u16 *)(fx + 0) = (u16)cw;
    *(u16 *)(fx + 2) = (u16)sw;
    u8 abridged = 0;
    for (int i = 0; i < 8; i++)
        if (((tag >> (2 * i)) & 3) != 3) abridged |= 1 << i;
    fx[4] = abridged;
    u16 magic = *(u16 *)(fp + FP_MAGIC);
    if (magic == 0) {
        *(u32 *)(fx + 24) = *(u32 *)(fp + FP_MXCSR);
        *(u32 *)(fx + 28) = 0xffff;
        memcpy(fx + 32, fp + FP_FXSR_ST, 128);
        memcpy(fx + 160, fp + FP_XMM, 128);
    } else {
        for (int i = 0; i < 8; i++) memcpy(fx + 32 + 16 * i, fp + FP_ST + 10 * i, 10);
    }
}

static void handler(int sig, void *info, void *ucp) {
    (void)info;
    struct ucontext_ *uc = ucp;
    struct sigcontext_ *c = &uc->mc;
    u32 eip = c->eip;
    if (g_debug) {
        char msg[] = "[refrun] signal 00 eip 00000000\n";
        msg[16] = '0' + sig / 10; msg[17] = '0' + sig % 10;
        for (int i = 0; i < 8; i++) msg[23 + i] = "0123456789abcdef"[(eip >> (28 - 4 * i)) & 15];
        sys_write(2, msg, sizeof msg - 1);
    }
    if (eip < CODE_BASE || eip >= CODE_BASE + CODE_MAX) {
        die("fault outside the test code (harness bug)");
    }
    u32 status;
    if (sig == SIGSEGV && *(u8 *)eip == 0xF4) status = ST_OK;
    else if (sig == SIGFPE) status = ST_DIVIDE;
    else if (sig == SIGTRAP) status = ST_BREAKPOINT;
    else if (sig == SIGILL) status = ST_ILLEGAL;
    else status = ST_ACCESS;
    g_out.status = status;
    g_out.fault_off = eip - CODE_BASE;
    g_out.regs[0] = c->eax; g_out.regs[1] = c->ecx; g_out.regs[2] = c->edx; g_out.regs[3] = c->ebx;
    g_out.regs[4] = c->esp; g_out.regs[5] = c->ebp; g_out.regs[6] = c->esi; g_out.regs[7] = c->edi;
    g_out.eflags = c->eflags;
    fxsave_from_fpstate(c->fpstate, g_out.fx);
    /* resume the harness after the trampoline, with its own stack and callee-saved registers */
    c->eip = (u32)resume_point;
    c->esp = g_saved_esp;
    c->ebp = g_saved_ebp;
    c->ebx = g_saved_ebx;
    c->esi = g_saved_esi;
    c->edi = g_saved_edi;
    c->eflags = 0x202u; /* a test may have set TF/AC via popfd; never carry those into the harness */
}

__asm__(
    ".text\n"
    ".globl run_case\n"
    "run_case:\n"
    "  movl %esp, g_saved_esp\n"
    "  movl %ebp, g_saved_ebp\n"
    "  movl %ebx, g_saved_ebx\n"
    "  movl %esi, g_saved_esi\n"
    "  movl %edi, g_saved_edi\n"
    "  fxrstor g_fx_in\n"
    "  pushl g_in_eflags\n"
    "  popfl\n"
    "  movl g_in_regs+4, %ecx\n"
    "  movl g_in_regs+8, %edx\n"
    "  movl g_in_regs+12, %ebx\n"
    "  movl g_in_regs+20, %ebp\n"
    "  movl g_in_regs+24, %esi\n"
    "  movl g_in_regs+28, %edi\n"
    "  movl g_in_regs+0, %eax\n"
    "  movl g_in_regs+16, %esp\n"
    "  jmp *g_code_entry\n"
    ".globl resume_point\n"
    "resume_point:\n"
    "  ret\n"
    ".globl sig_restorer\n"
    "sig_restorer:\n"
    "  movl $173, %eax\n"
    "  int $0x80\n"
);
extern void run_case(void);
extern void sig_restorer(void);

int entry(int argc, char **argv) {
    if (argc < 3) die("usage: refrun in.bin out.bin");
    for (int i = 3; i < argc; i++) if (argv[i][0] == '-' && argv[i][1] == 'd') g_debug = 1;
    long in = sys3(SYS_open, (long)argv[1], 0, 0);
    long out = sys3(SYS_open, (long)argv[2], 0x241, 0644);
    if (in < 0 || out < 0) die("cannot open files");

    struct { u32 addr, len, prot, flags, fd, off; } m = { DATA_BASE, BLOCK_SIZE, 7, 0x32 /* PRIVATE|FIXED|ANON */, (u32)-1, 0 };
    long p = sys3(SYS_mmap, (long)&m, 0, 0);
    if (p != (long)DATA_BASE) die("mmap at 0x20000000 failed");

    struct stack_t_ ss = { g_altstack, 0, sizeof g_altstack };
    if (sys3(SYS_sigaltstack, (long)&ss, 0, 0) != 0) die("sigaltstack failed");
    struct kernel_sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.handler = handler;
    sa.flags = SA_SIGINFO | SA_ONSTACK | SA_RESTORER | SA_NODEFER;
    sa.restorer = sig_restorer;
    int sigs[] = { SIGILL, SIGTRAP, SIGBUS, SIGFPE, SIGSEGV };
    for (int i = 0; i < 5; i++)
        if (sys4(SYS_rt_sigaction, sigs[i], (long)&sa, 0, 8) != 0) die("sigaction failed");

    u8 hdr[8];
    if (!read_full(in, hdr, 8) || hdr[0] != 'G' || hdr[1] != 'M' || hdr[2] != 'R' || hdr[3] != '2') die("bad batch header");
    u32 count = *(u32 *)(hdr + 4);
    for (u32 n = 0; n < count; n++) {
        u32 code_len;
        if (!read_full(in, &code_len, 4)) die("truncated case");
        if (code_len > CODE_MAX || !read_full(in, g_code, code_len)) die("bad code length");
        if (!read_full(in, &g_ci, sizeof g_ci)) die("truncated case body");
        memcpy((void *)DATA_BASE, g_ci.data, DATA_SIZE);
        memcpy((void *)STACK_BASE, g_ci.stack, STACK_SIZE);
        memcpy((void *)CODE_BASE, g_code, code_len);
        memset((void *)(CODE_BASE + code_len), 0xF4, CODE_MAX - code_len);
        memcpy(g_fx_in, g_ci.fx, 512);
        memcpy(g_in_regs, g_ci.regs, 32);
        g_in_regs[4] = STACK_BASE + (g_ci.regs[4] & (STACK_SIZE - 1));
        g_in_eflags = (g_ci.eflags & 0x0CD5u) | 0x202u;
        memset(&g_out, 0, sizeof g_out);
        g_out.status = 0xFFFFFFFFu;
        run_case();
        g_out.stack_base = STACK_BASE;
        memcpy(g_out.data, (void *)DATA_BASE, DATA_SIZE);
        memcpy(g_out.stack, (void *)STACK_BASE, STACK_SIZE);
        if (!write_full(out, &g_out, sizeof g_out)) die("write failed");
    }
    sys3(SYS_close, out, 0, 0);
    sys3(SYS_close, in, 0, 0);
    return 0;
}

__asm__(
    ".text\n"
    ".globl _start\n"
    "_start:\n"
    "  movl (%esp), %eax\n"      /* argc */
    "  leal 4(%esp), %ecx\n"     /* argv */
    "  andl $-16, %esp\n"
    "  pushl %ecx\n"
    "  pushl %eax\n"
    "  call entry\n"
    "  movl %eax, %ebx\n"
    "  movl $1, %eax\n"
    "  int $0x80\n"
);
