/*
 * In-process native reference for the 64-bit CPU differential tester. The host is
 * x86-64 Linux, so a generated case runs directly on the CPU: its code is copied to a
 * fixed region, a trampoline loads the registers, flags and SSE/x87 state and jumps to
 * it, and a signal handler (the case ends with `hlt`, a #GP) reads the register and
 * FXSAVE state the CPU left and resumes the harness.
 *
 * Linked into the harness (crates/harness/src/native64.rs) — an ordinary glibc program,
 * not freestanding like refrun_linux.c. Addresses match the wasm side: with the runner
 * image's upper address half zero in the tests, 64-bit effective addresses equal their
 * low 32 bits, so a single sub-4 GB region holds data, code and the stack.
 */
#define _GNU_SOURCE
#include <stdint.h>
#include <string.h>
#include <signal.h>
#include <sys/mman.h>
#include <ucontext.h>

#define DATA_BASE 0x20000000u
#define DATA_SIZE 0x800u
#define CODE_BASE 0x20001000u
#define CODE_MAX 0x1000u
#define STACK_BASE 0x20002000u
#define STACK_SIZE 0x400u
#define REGION 0x4000u

/* Windows-style status codes, as refrun_linux.c reports them. */
#define ST_OK 0x00000000u
#define ST_BREAKPOINT 0x80000003u
#define ST_ACCESS 0xC0000005u
#define ST_ILLEGAL 0xC000001Du
#define ST_DIVIDE 0xC0000094u

struct case_in64 {
    uint64_t regs[16]; /* rax, rcx, rdx, rbx, rsp, rbp, rsi, rdi, r8..r15 */
    uint64_t rflags;
    uint8_t fx[512];
    uint8_t data[DATA_SIZE];
    uint8_t code[CODE_MAX];
    uint32_t code_len;
    uint8_t stack[STACK_SIZE];
};

struct case_out64 {
    uint64_t regs[16];
    uint64_t rflags;
    uint8_t fx[512];
    uint8_t data[DATA_SIZE];
    uint8_t stack[STACK_SIZE];
    uint32_t status;
    uint32_t fault_off;
};

static struct case_out64 *g_out;
/* Touched by the top-level asm, so not `static`: the compiler would drop a static it
 * cannot see referenced. */
uint64_t g_resume_rsp, g_resume_rbx, g_resume_rbp, g_resume_r12, g_resume_r13, g_resume_r14, g_resume_r15;
uint8_t g_fx_in[512] __attribute__((aligned(16)));
uint64_t g_in_regs[16];
uint64_t g_in_rflags;
uint64_t g_code_entry = CODE_BASE;
static uint8_t g_altstack[65536] __attribute__((aligned(16)));

extern void run_case64(void);
extern void resume_point64(void);

/* The trampoline. resume_point64 restores the harness's callee-saved registers (the
 * handler returns to it with the harness stack) and returns from run_case64. */
__asm__(
    ".text\n"
    ".globl run_case64\n"
    "run_case64:\n"
    "  movq %rsp, g_resume_rsp(%rip)\n"
    "  movq %rbx, g_resume_rbx(%rip)\n"
    "  movq %rbp, g_resume_rbp(%rip)\n"
    "  movq %r12, g_resume_r12(%rip)\n"
    "  movq %r13, g_resume_r13(%rip)\n"
    "  movq %r14, g_resume_r14(%rip)\n"
    "  movq %r15, g_resume_r15(%rip)\n"
    "  fxrstor g_fx_in(%rip)\n"
    "  movq g_in_rflags(%rip), %r11\n"
    "  pushq %r11\n"
    "  popfq\n"
    "  leaq g_in_regs(%rip), %r11\n"
    "  movq 0(%r11), %rax\n"
    "  movq 8(%r11), %rcx\n"
    "  movq 16(%r11), %rdx\n"
    "  movq 24(%r11), %rbx\n"
    "  movq 40(%r11), %rbp\n"
    "  movq 48(%r11), %rsi\n"
    "  movq 56(%r11), %rdi\n"
    "  movq 64(%r11), %r8\n"
    "  movq 72(%r11), %r9\n"
    "  movq 80(%r11), %r10\n"
    "  movq 96(%r11), %r12\n"
    "  movq 104(%r11), %r13\n"
    "  movq 112(%r11), %r14\n"
    "  movq 120(%r11), %r15\n"
    "  movq 32(%r11), %rsp\n"
    "  movq 88(%r11), %r11\n"
    "  jmp *g_code_entry(%rip)\n"
    ".globl resume_point64\n"
    "resume_point64:\n"
    "  movq g_resume_rbx(%rip), %rbx\n"
    "  movq g_resume_rbp(%rip), %rbp\n"
    "  movq g_resume_r12(%rip), %r12\n"
    "  movq g_resume_r13(%rip), %r13\n"
    "  movq g_resume_r14(%rip), %r14\n"
    "  movq g_resume_r15(%rip), %r15\n"
    "  ret\n");

static void handler(int sig, siginfo_t *info, void *ucp) {
    (void)info;
    ucontext_t *uc = ucp;
    greg_t *g = uc->uc_mcontext.gregs;
    uint64_t rip = (uint64_t)g[REG_RIP];
    /* every register from the interrupted context */
    g_out->regs[0] = g[REG_RAX];
    g_out->regs[1] = g[REG_RCX];
    g_out->regs[2] = g[REG_RDX];
    g_out->regs[3] = g[REG_RBX];
    g_out->regs[4] = g[REG_RSP];
    g_out->regs[5] = g[REG_RBP];
    g_out->regs[6] = g[REG_RSI];
    g_out->regs[7] = g[REG_RDI];
    g_out->regs[8] = g[REG_R8];
    g_out->regs[9] = g[REG_R9];
    g_out->regs[10] = g[REG_R10];
    g_out->regs[11] = g[REG_R11];
    g_out->regs[12] = g[REG_R12];
    g_out->regs[13] = g[REG_R13];
    g_out->regs[14] = g[REG_R14];
    g_out->regs[15] = g[REG_R15];
    g_out->rflags = g[REG_EFL];
    /* glibc's fpregs is the 512-byte FXSAVE image (xmm at offset 160) */
    if (uc->uc_mcontext.fpregs) {
        memcpy(g_out->fx, uc->uc_mcontext.fpregs, 512);
    }
    uint32_t status;
    if (sig == SIGSEGV && rip >= CODE_BASE && rip < CODE_BASE + CODE_MAX && *(uint8_t *)rip == 0xF4) {
        status = ST_OK;
    } else if (sig == SIGFPE) {
        status = ST_DIVIDE;
    } else if (sig == SIGTRAP) {
        status = ST_BREAKPOINT;
    } else if (sig == SIGILL) {
        status = ST_ILLEGAL;
    } else {
        status = ST_ACCESS;
    }
    g_out->status = status;
    g_out->fault_off = (rip >= CODE_BASE && rip < CODE_BASE + CODE_MAX) ? (uint32_t)(rip - CODE_BASE) : 0xFFFFFFFFu;
    /* resume the harness: its stack, the callee-saved registers restored by the
     * trampoline's tail; drop any TF/AC a popfq set */
    g[REG_RIP] = (greg_t)(uintptr_t)resume_point64;
    g[REG_RSP] = (greg_t)g_resume_rsp;
    g[REG_EFL] = 0x202;
}

/* Maps the fixed region and installs the fault handlers; 0 on success. */
int refrun64_init(void) {
    void *p = mmap((void *)(uintptr_t)DATA_BASE, REGION, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    if (p != (void *)(uintptr_t)DATA_BASE) {
        return 1;
    }
    stack_t ss = {.ss_sp = g_altstack, .ss_flags = 0, .ss_size = sizeof g_altstack};
    if (sigaltstack(&ss, 0) != 0) {
        return 2;
    }
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = handler;
    sa.sa_flags = SA_SIGINFO | SA_ONSTACK | SA_NODEFER;
    sigemptyset(&sa.sa_mask);
    int sigs[] = {SIGILL, SIGTRAP, SIGBUS, SIGFPE, SIGSEGV};
    for (int i = 0; i < 5; i++) {
        if (sigaction(sigs[i], &sa, 0) != 0) {
            return 3;
        }
    }
    return 0;
}

/* Runs one case. rsp comes as an offset into the stack window. */
void refrun64_run(const struct case_in64 *in, struct case_out64 *out) {
    memcpy((void *)(uintptr_t)DATA_BASE, in->data, DATA_SIZE);
    memset((void *)(uintptr_t)(DATA_BASE + DATA_SIZE), 0, CODE_BASE - DATA_BASE - DATA_SIZE);
    memcpy((void *)(uintptr_t)STACK_BASE, in->stack, STACK_SIZE);
    uint32_t len = in->code_len < CODE_MAX ? in->code_len : CODE_MAX;
    memcpy((void *)(uintptr_t)CODE_BASE, in->code, len);
    memset((void *)(uintptr_t)(CODE_BASE + len), 0xF4, CODE_MAX - len);

    memcpy(g_fx_in, in->fx, 512);
    memcpy(g_in_regs, in->regs, sizeof g_in_regs);
    g_in_regs[4] = STACK_BASE + (in->regs[4] & (STACK_SIZE - 1));
    g_in_rflags = (in->rflags & 0x0CD5u) | 0x202u;

    memset(out, 0, sizeof *out);
    out->status = 0xFFFFFFFFu;
    g_out = out;
    run_case64();

    memcpy(out->data, (void *)(uintptr_t)DATA_BASE, DATA_SIZE);
    memcpy(out->stack, (void *)(uintptr_t)STACK_BASE, STACK_SIZE);
}
