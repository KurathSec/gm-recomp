//! Win64 exceptions, which are table-based: each 64-bit image lists its functions in
//! .pdata (RUNTIME_FUNCTION: begin, end, unwind info RVAs) with unwind codes saying how
//! to undo each prolog, and optionally a language handler (`__C_specific_handler`,
//! `__CxxFrameHandler4`...). Dispatch walks the frames from the faulting context
//! calling the handlers that want exceptions; a handler that takes one calls
//! RtlUnwindEx, which walks again calling the unwind handlers up to the target frame
//! and continues there. As Wine's ntdll does it (dlls/ntdll/signal_x86_64.c,
//! unwind.c), on CONTEXT records in guest memory.

use super::{ApiError, ApiResult, Ctx, Ret, TebField};

/// CONTEXT (AMD64): size, and offsets of ContextFlags, MxCsr, EFlags, Rax (the integer
/// registers follow in encoding order), Rip and Xmm0 (the 16 follow).
pub const CONTEXT_SIZE: u32 = 0x4D0;
const CTX_FLAGS: u32 = 0x30;
const CTX_MXCSR: u32 = 0x34;
const CTX_EFLAGS: u32 = 0x44;
const CTX_RAX: u32 = 0x78;
const CTX_RIP: u32 = 0xF8;
const CTX_FLT_MXCSR: u32 = 0x100 + 0x18;
const CTX_XMM0: u32 = 0x1A0;
/// CONTEXT_AMD64 | CONTROL | INTEGER | FLOATING_POINT
const CONTEXT_FULL: u32 = 0x0010_000B;

/// EXCEPTION_RECORD64: code, flags, nested record, address, parameter count, parameters.
pub const RECORD_SIZE: u32 = 152;
/// DISPATCHER_CONTEXT (AMD64).
const DISPATCH_SIZE: u32 = 80;

const EH_NONCONTINUABLE: u32 = 0x01;
const EH_UNWINDING: u32 = 0x02;
const EH_EXIT_UNWIND: u32 = 0x04;
const EH_TARGET_UNWIND: u32 = 0x20;
const EH_COLLIDED_UNWIND: u32 = 0x40;

pub const UNW_FLAG_EHANDLER: u32 = 1;
pub const UNW_FLAG_UHANDLER: u32 = 2;
const UNW_FLAG_CHAININFO: u32 = 4;

const STATUS_UNWIND: u32 = 0xC000_0027;
const STATUS_UNWIND_CONSOLIDATE: u32 = 0x8000_0029;

const EXCEPTION_CONTINUE_EXECUTION: u64 = 0;
const EXCEPTION_COLLIDED_UNWIND: u64 = 3;

/// A CONTEXT held host-side while frames are walked: the integer registers, Rip, the
/// XMM registers (xmm6..15 are callee-saved) and the flags words.
#[derive(Clone)]
pub struct Context64 {
    pub r: [u64; 16],
    pub rip: u64,
    pub xmm: [u128; 16],
    pub eflags: u32,
    pub mxcsr: u32,
}

impl Context64 {
    pub fn read(c: &Ctx, at: u32) -> Self {
        let mut r = [0u64; 16];
        for (i, v) in r.iter_mut().enumerate() {
            *v = c.host.read_u64(at + CTX_RAX + 8 * i as u32).unwrap_or(0);
        }
        let mut xmm = [0u128; 16];
        for (i, v) in xmm.iter_mut().enumerate() {
            let mut b = [0u8; 16];
            let _ = c.host.read(at + CTX_XMM0 + 16 * i as u32, &mut b);
            *v = u128::from_le_bytes(b);
        }
        Context64 {
            r,
            rip: c.host.read_u64(at + CTX_RIP).unwrap_or(0),
            xmm,
            eflags: c.host.read_u32(at + CTX_EFLAGS).unwrap_or(0x202),
            mxcsr: c.host.read_u32(at + CTX_MXCSR).unwrap_or(0x1F80),
        }
    }

    pub fn write(&self, c: &mut Ctx, at: u32) {
        let _ = c.host.fill(at, CONTEXT_SIZE, 0);
        let _ = c.host.write_u32(at + CTX_FLAGS, CONTEXT_FULL);
        let _ = c.host.write_u32(at + CTX_MXCSR, self.mxcsr);
        let _ = c.host.write_u32(at + CTX_FLT_MXCSR, self.mxcsr);
        let _ = c.host.write_u32(at + CTX_EFLAGS, self.eflags);
        for (i, v) in self.r.iter().enumerate() {
            let _ = c.host.write_u64(at + CTX_RAX + 8 * i as u32, *v);
        }
        let _ = c.host.write_u64(at + CTX_RIP, self.rip);
        for (i, v) in self.xmm.iter().enumerate() {
            let _ = c.host.write(at + CTX_XMM0 + 16 * i as u32, &v.to_le_bytes());
        }
    }

    /// The registers of the API's caller as they are after the API returns (Rip the
    /// return address, Rsp above it).
    pub fn of_caller(c: &mut Ctx) -> Self {
        let mut r = c.regs.r64.unwrap_or_default();
        let rsp = r[4];
        let rip = c.host.read_u64(rsp as u32).unwrap_or(0);
        r[4] = rsp + 8;
        let mut xmm = [0u128; 16];
        for (i, v) in xmm.iter_mut().enumerate() {
            *v = c.host.xmm_reg(i as u32);
        }
        let mxcsr = c.host.mxcsr();
        Context64 { r, rip, xmm, eflags: 0x202, mxcsr }
    }

    /// Continues the guest in this context (the API returns `Ret::resume`).
    pub fn resume(&self, c: &mut Ctx) -> ApiResult {
        for (i, v) in self.xmm.iter().enumerate() {
            c.host.set_xmm_reg(i as u32, *v);
        }
        c.host.set_mxcsr(self.mxcsr);
        let r = self.r;
        *c.regs = super::Regs {
            eax: r[0] as u32,
            ecx: r[1] as u32,
            edx: r[2] as u32,
            ebx: r[3] as u32,
            esp: r[4] as u32,
            ebp: r[5] as u32,
            esi: r[6] as u32,
            edi: r[7] as u32,
            eip: self.rip as u32,
            fs_base: c.regs.fs_base,
            r64: Some(r),
        };
        Ok(Ret::resume())
    }
}

/// The function table entry covering `pc`: (image base as the guest sees it, the
/// entry's address in guest memory, its begin RVA, unwind info RVA).
pub fn lookup(c: &Ctx, pc: u64) -> Option<(u64, u32, u32, u32)> {
    let m = c.p.module_containing(pc as u32)?;
    let (table, n) = m.pdata;
    let rva = (pc as u32).wrapping_sub(m.base);
    let base = (m.base_high as u64) << 32 | m.base as u64;
    // (sorted by begin address)
    let (mut lo, mut hi) = (0u32, n);
    while lo < hi {
        let mid = (lo + hi) / 2;
        let e = table + 12 * mid;
        let begin = c.host.read_u32(e).ok()?;
        let end = c.host.read_u32(e + 4).ok()?;
        if rva < begin {
            hi = mid;
        } else if rva >= end {
            lo = mid + 1;
        } else {
            return Some((base, e, begin, c.host.read_u32(e + 8).ok()?));
        }
    }
    None
}

/// What a frame's unwind gives: its establisher frame, and the language handler of
/// the kind asked for with its data (both full addresses) if it has one and `pc` is
/// past its prolog.
pub struct Unwound {
    pub frame: u64,
    pub handler: Option<(u64, u64)>,
}

/// RtlVirtualUnwind: undoes the prolog of the function whose entry is at `entry`
/// (image `base`) in `ctx`, which ends as its caller's context.
pub fn virtual_unwind(c: &Ctx, kind: u32, base: u64, pc: u64, entry: u32, ctx: &mut Context64) -> Result<Unwound, String> {
    let img = base as u32;
    let rd8 = |a: u32| c.host.read_u8(a).unwrap_or(0) as u32;
    let rd16 = |a: u32| c.host.read_u16(a).unwrap_or(0) as u32;
    let rd32 = |a: u32| c.host.read_u32(a).unwrap_or(0);
    let rd64 = |a: u64| c.host.read_u64(a as u32).unwrap_or(0);
    // (an entry whose unwind data has bit 0 set stands for the entry at that RVA)
    let entry = if rd32(entry + 8) & 1 != 0 { img + (rd32(entry + 8) & !1) } else { entry };
    let mut func_begin = rd32(entry);
    let mut info = img + rd32(entry + 8);
    let mut mach_frame = false;
    let mut frame;
    // (the handler is the last info's: a chained info is a fragment's, which points to
    // its function's)
    let (mut flags_of_last, mut handler_at, mut in_prolog);
    loop {
        let (version, flags) = (rd8(info) & 7, rd8(info) >> 3);
        if version != 1 && version != 2 {
            return Err(format!("unwind info version {version} at {info:#x}"));
        }
        let (prolog, count, fr) = (rd8(info + 1), rd8(info + 2), rd8(info + 3));
        let (frame_reg, frame_off) = ((fr & 15) as usize, fr >> 4);
        frame = if frame_reg != 0 { ctx.r[frame_reg].wrapping_sub(16 * frame_off as u64) } else { ctx.r[4] };
        let start = base + func_begin as u64;
        let prolog_offset = if pc < start || pc >= start + prolog as u64 { u32::MAX } else { (pc - start) as u32 };
        let codes = info + 4;
        flags_of_last = flags;
        handler_at = codes + 2 * ((count + 1) & !1);
        in_prolog = prolog_offset != u32::MAX;
        let mut i = 0;
        while i < count {
            let at = codes + 2 * i;
            let (offset, op) = (rd8(at), rd8(at + 1));
            let (code, opinfo) = (op & 15, (op >> 4) as usize);
            let slots = match code {
                1 => if opinfo == 0 { 2 } else { 3 },
                4 | 6 | 8 => 2,
                5 | 9 => 3,
                _ => 1,
            };
            if prolog_offset < offset {
                i += slots;
                continue;
            }
            match code {
                // UWOP_PUSH_NONVOL
                0 => {
                    ctx.r[opinfo] = rd64(ctx.r[4]);
                    ctx.r[4] += 8;
                }
                // UWOP_ALLOC_LARGE
                1 => ctx.r[4] += if opinfo == 0 { rd16(at + 2) as u64 * 8 } else { rd32(at + 2) as u64 },
                // UWOP_ALLOC_SMALL
                2 => ctx.r[4] += (opinfo as u64 + 1) * 8,
                // UWOP_SET_FPREG
                3 => ctx.r[4] = frame,
                // UWOP_SAVE_NONVOL(_FAR)
                4 => ctx.r[opinfo] = rd64(frame + rd16(at + 2) as u64 * 8),
                5 => ctx.r[opinfo] = rd64(frame + rd32(at + 2) as u64),
                // UWOP_SAVE_XMM128(_FAR)
                8 | 9 => {
                    let off = if code == 8 { rd16(at + 2) as u64 * 16 } else { rd32(at + 2) as u64 };
                    let mut b = [0u8; 16];
                    let _ = c.host.read((frame + off) as u32, &mut b);
                    ctx.xmm[opinfo] = u128::from_le_bytes(b);
                }
                // UWOP_PUSH_MACHFRAME: an interrupt frame (with an error code first if 1)
                10 => {
                    mach_frame = true;
                    let rsp = ctx.r[4] + if opinfo != 0 { 8 } else { 0 };
                    ctx.rip = rd64(rsp);
                    ctx.eflags = rd64(rsp + 16) as u32;
                    ctx.r[4] = rd64(rsp + 24);
                }
                // UWOP_EPILOG (version 2): where the epilogs are, nothing to undo
                6 => {}
                _ => return Err(format!("unwind code {code} at {at:#x}")),
            }
            i += slots;
        }
        if flags & UNW_FLAG_CHAININFO == 0 {
            break;
        }
        // the chained RUNTIME_FUNCTION follows the codes
        let chained = codes + 2 * ((count + 1) & !1);
        func_begin = rd32(chained);
        info = img + rd32(chained + 8);
    }
    if !mach_frame {
        ctx.rip = rd64(ctx.r[4]);
        ctx.r[4] += 8;
    }
    let handler = (flags_of_last & kind != 0 && !in_prolog).then(|| {
        let h = rd32(handler_at);
        (base + h as u64, (handler_at + 4) as u64 | (base & !0xFFFF_FFFF))
    });
    Ok(Unwound { frame, handler })
}

/// One step of a frame walk: a function with table entry is unwound by its codes, any
/// other code is a leaf (its return address on top of the stack), and a return to the
/// host (a callback an API made) goes on as the API's caller (`Process::host_frames64`).
/// Returns the frame's pc and entry (image base, entry address) with the unwind.
fn step(c: &Ctx, kind: u32, ctx: &mut Context64) -> Result<(u64, Option<(u64, u32)>, Unwound), String> {
    let pc = ctx.rip;
    if let Some(&(_, r, rip)) = c.p.host_frames64.iter().rev().find(|f| f.0 as u64 == pc) {
        let frame = ctx.r[4];
        ctx.r = r;
        ctx.rip = rip;
        return Ok((pc, None, Unwound { frame, handler: None }));
    }
    match lookup(c, pc) {
        Some((base, entry, _, _)) => {
            let u = virtual_unwind(c, kind, base, pc, entry, ctx)?;
            Ok((pc, Some((base, entry)), u))
        }
        None => {
            let frame = ctx.r[4];
            ctx.rip = c.host.read_u64(ctx.r[4] as u32).unwrap_or(0);
            ctx.r[4] += 8;
            Ok((pc, None, Unwound { frame, handler: None }))
        }
    }
}

/// Whether `frame` is a frame of the current thread's stack.
fn on_stack(c: &Ctx, frame: u64) -> bool {
    let teb = c.regs.fs_base;
    let base = c.host.read_u64(teb + TebField::StackBase.offset(true)).unwrap_or(0);
    let limit = c.host.read_u64(teb + TebField::StackLimit.offset(true)).unwrap_or(0);
    frame & 7 == 0 && frame >= limit && frame < base
}

/// Guest scratch memory below the stack pointer for records the handlers get: an
/// exception record, two contexts and a dispatcher context. The handlers run below it.
fn scratch(c: &mut Ctx) -> u32 {
    let rsp = c.regs.r64.map_or(c.regs.esp, |r| r[4] as u32);
    let at = (rsp - 0x100 - RECORD_SIZE - 2 * CONTEXT_SIZE - DISPATCH_SIZE) & !0xF;
    if let Some(r) = c.regs.r64.as_mut() {
        r[4] = at as u64;
    }
    c.regs.esp = at;
    at
}

fn write_dispatch(c: &mut Ctx, at: u32, pc: u64, entry: Option<(u64, u32)>, frame: u64, target_ip: u64, ctx_at: u32, handler: (u64, u64)) {
    let (base, entry_at) = entry.unwrap_or((0, 0));
    let full_entry = if entry_at == 0 { 0 } else { (base & !0xFFFF_FFFF) | entry_at as u64 };
    for (off, v) in [(0, pc), (8, base), (16, full_entry), (24, frame), (32, target_ip), (40, ctx_at as u64), (48, handler.0), (56, handler.1), (64, 0)] {
        let _ = c.host.write_u64(at + off, v);
    }
    let _ = c.host.write_u32(at + 72, 0); // ScopeIndex
    let _ = c.host.write_u32(at + 76, 0);
}

/// Writes an EXCEPTION_RECORD64.
pub fn write_record(c: &mut Ctx, at: u32, code: u32, flags: u32, addr: u64, params: &[u64]) {
    let _ = c.host.fill(at, RECORD_SIZE, 0);
    let _ = c.host.write_u32(at, code);
    let _ = c.host.write_u32(at + 4, flags);
    let _ = c.host.write_u64(at + 16, addr);
    let _ = c.host.write_u32(at + 24, params.len().min(15) as u32);
    for (i, p) in params.iter().take(15).enumerate() {
        let _ = c.host.write_u64(at + 32 + 8 * i as u32, *p);
    }
}

/// RtlDispatchException: offers the exception at `ctx` to the handlers of the frames
/// from there out. A handler that takes it unwinds to its frame (RtlUnwindEx: no return
/// here); one that continues execution resumes `ctx` (as its handler left it).
pub fn dispatch(c: &mut Ctx, code: u32, flags: u32, params: &[u64], ctx: &Context64) -> ApiResult {
    let base = scratch(c);
    let (rec_at, ctx_at, work_at, disp_at) = (base, base + RECORD_SIZE, base + RECORD_SIZE + CONTEXT_SIZE, base + RECORD_SIZE + 2 * CONTEXT_SIZE);
    write_record(c, rec_at, code, flags, ctx.rip, params);
    ctx.write(c, ctx_at);
    let mut walk = ctx.clone();
    c.p.log(format!("exception {code:#x} {params:x?} at {:#x}: dispatching", ctx.rip));
    for _ in 0..1000 {
        let (pc, entry, u) = step(c, UNW_FLAG_EHANDLER, &mut walk).map_err(ApiError)?;
        if !on_stack(c, u.frame) {
            break;
        }
        if let Some(h) = u.handler {
            walk.write(c, work_at);
            write_dispatch(c, disp_at, pc, entry, u.frame, 0, work_at, h);
            let r = c.call_guest64(h.0 as u32, &[rec_at as u64, u.frame, ctx_at as u64, disp_at as u64])?;
            if r as u32 as u64 == EXCEPTION_CONTINUE_EXECUTION {
                if flags & EH_NONCONTINUABLE != 0 {
                    return Err(ApiError(format!("noncontinuable exception {code:#x} continued")));
                }
                let back = Context64::read(c, ctx_at);
                return back.resume(c);
            }
            // (continue search; nested and collided dispatches are taken as that)
        }
        // (out of the guest's code: the thread's start, or nothing sensible)
        let next = walk.rip as u32;
        if next == 0 || next == crate::process::THREAD_EXIT || c.p.module_containing(next).is_none() && !c.p.host_frames64.iter().any(|f| f.0 == next) {
            break;
        }
    }
    Err(ApiError(format!("unhandled exception {code:#x} {params:x?} at {:#x} in 64-bit code", ctx.rip)))
}

/// RtlUnwindEx(TargetFrame, TargetIp, ExceptionRecord, ReturnValue, ContextRecord,
/// HistoryTable): calls the unwind handlers of the frames from the caller out to
/// `TargetFrame` and continues there at `TargetIp` with RAX `ReturnValue`; with
/// STATUS_UNWIND_CONSOLIDATE, at what the record's callback returns instead (C++ catch
/// blocks).
pub fn unwind_ex(c: &mut Ctx) -> ApiResult {
    let (target_frame, target_ip, rec_arg, retval) = (c.arg64(0), c.arg64(1), c.arg(2), c.arg64(3));
    let mut cur = Context64::of_caller(c);
    let base = scratch(c);
    let (own_rec, ctx_at, disp_at) = (base, base + RECORD_SIZE, base + RECORD_SIZE + 2 * CONTEXT_SIZE);
    let rec = if rec_arg != 0 {
        rec_arg
    } else {
        write_record(c, own_rec, STATUS_UNWIND, 0, cur.rip, &[]);
        own_rec
    };
    let mut flags = c.host.read_u32(rec + 4).unwrap_or(0) | EH_UNWINDING | if target_frame == 0 { EH_EXIT_UNWIND } else { 0 };
    for _ in 0..1000 {
        let mut next = cur.clone();
        let (pc, entry, u) = step(c, UNW_FLAG_UHANDLER, &mut next).map_err(ApiError)?;
        if !on_stack(c, u.frame) {
            return Err(ApiError(format!("RtlUnwindEx: frame {:#x} off the stack (target {target_frame:#x})", u.frame)));
        }
        if let Some(h) = u.handler {
            if target_frame != 0 && u.frame > target_frame {
                return Err(ApiError(format!("RtlUnwindEx: passed the target frame {target_frame:#x} at {:#x}", u.frame)));
            }
            if u.frame == target_frame {
                flags |= EH_TARGET_UNWIND;
            }
            let _ = c.host.write_u32(rec + 4, flags);
            cur.write(c, ctx_at);
            write_dispatch(c, disp_at, pc, entry, u.frame, target_ip, ctx_at, h);
            let r = c.call_guest64(h.0 as u32, &[rec as u64, u.frame, ctx_at as u64, disp_at as u64])?;
            if r as u32 as u64 == EXCEPTION_COLLIDED_UNWIND {
                return Err(ApiError("RtlUnwindEx: collided unwind".into()));
            }
            flags &= !EH_COLLIDED_UNWIND;
        }
        if u.frame == target_frame {
            break;
        }
        cur = next;
    }
    cur.r[0] = retval;
    cur.rip = target_ip;
    // C++ catch blocks: the record's callback runs the catch and says where to go on
    let code = c.host.read_u32(rec).unwrap_or(0);
    if code == STATUS_UNWIND_CONSOLIDATE && c.host.read_u32(rec + 24).unwrap_or(0) >= 1 {
        let callback = c.host.read_u64(rec + 32).unwrap_or(0);
        cur.rip = c.call_guest64(callback as u32, &[rec as u64])?;
    }
    c.trace(format!("(frame {target_frame:#x}) -> {:#x}", cur.rip));
    cur.resume(c)
}

/// RtlLookupFunctionEntry(ControlPc, ImageBase*, HistoryTable): the entry's full address.
pub fn lookup_function_entry(c: &mut Ctx) -> ApiResult {
    let (pc, base_out) = (c.arg64(0), c.arg(1));
    match lookup(c, pc) {
        Some((base, entry, _, _)) => {
            if base_out != 0 {
                let _ = c.host.write_u64(base_out, base);
            }
            Ok(Ret::ptr(3, (base & !0xFFFF_FFFF) | entry as u64))
        }
        None => {
            if base_out != 0 {
                let _ = c.host.write_u64(base_out, 0);
            }
            Ok(Ret::ptr(3, 0))
        }
    }
}

/// RtlVirtualUnwind(HandlerType, ImageBase, ControlPc, FunctionEntry, ContextRecord,
/// HandlerData*, EstablisherFrame*, ContextPointers): the language handler or 0.
pub fn virtual_unwind_api(c: &mut Ctx) -> ApiResult {
    let (kind, base, pc, entry, ctx_at, data_out, frame_out) = (c.arg(0), c.arg64(1), c.arg64(2), c.arg(3), c.arg(4), c.arg(5), c.arg(6));
    let mut ctx = Context64::read(c, ctx_at);
    let u = virtual_unwind(c, kind, base, pc, entry, &mut ctx).map_err(ApiError)?;
    ctx.write(c, ctx_at);
    if frame_out != 0 {
        let _ = c.host.write_u64(frame_out, u.frame);
    }
    let (h, data) = u.handler.unwrap_or((0, 0));
    if data_out != 0 {
        let _ = c.host.write_u64(data_out, data);
    }
    Ok(Ret::ptr(8, h))
}

/// RtlCaptureContext(ContextRecord): the caller's registers as after the call.
pub fn capture_context(c: &mut Ctx) -> ApiResult {
    let at = c.arg(0);
    let ctx = Context64::of_caller(c);
    ctx.write(c, at);
    Ok(Ret::stdcall(1, 0))
}

/// RtlRestoreContext(ContextRecord, ExceptionRecord): continues in that context.
pub fn restore_context(c: &mut Ctx) -> ApiResult {
    let at = c.arg(0);
    let ctx = Context64::read(c, at);
    ctx.resume(c)
}

/// RtlCaptureStackBackTrace(FramesToSkip, FramesToCapture, BackTrace, BackTraceHash):
/// the return addresses of the caller's frames.
pub fn capture_stack_back_trace(c: &mut Ctx) -> ApiResult {
    let (skip, want, out) = (c.arg(0), c.arg(1), c.arg(2));
    let mut ctx = Context64::of_caller(c);
    let mut n = 0;
    for i in 0..(skip + want).min(256) {
        if ctx.rip == 0 || c.p.module_containing(ctx.rip as u32).is_none() {
            break;
        }
        if i >= skip {
            let _ = c.host.write_u64(out + 8 * n, ctx.rip);
            n += 1;
        }
        let Ok((_, _, u)) = step(c, 0, &mut ctx) else { break };
        if !on_stack(c, u.frame) {
            break;
        }
    }
    Ok(Ret::stdcall(4, n))
}
