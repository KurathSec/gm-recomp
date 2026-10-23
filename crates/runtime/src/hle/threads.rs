//! Guest threads, run one at a time. The main thread runs as the host calls it;
//! secondary threads run from its blocking points (sleeps, waits, idle message
//! loops: anything that advances the virtual clock), each until it blocks again,
//! ends, or uses up a dispatch budget. A secondary thread blocks by yielding back
//! to the scheduler, which it can only do at its own top level (not inside a
//! callback the HLE made into it); there a wait that cannot be satisfied returns
//! as if it timed out. Scheduling depends only on the virtual clock and guest
//! behavior, so runs stay deterministic.

use super::{TebField, ApiResult, Ctx, Registry, Ret};
use crate::mem::Guest;
use crate::process::{Handle, Process, Thread, ThreadCtx, ThreadState, THREAD_EXIT};

const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 0x102;
const WAIT_FAILED: u32 = 0xFFFF_FFFF;
const WAIT_IO_COMPLETION: u32 = 0xC0;
const STILL_ACTIVE: u32 = 0x103;
const INFINITE: u32 = 0xFFFF_FFFF;
const CREATE_SUSPENDED: u32 = 4;

fn ok(nargs: u32, v: u32) -> ApiResult {
    Ok(Ret::stdcall(nargs, v))
}

/// Whether waiting on `h` is over; `consume` takes the signal (auto-reset events).
pub fn signaled(p: &mut Process, h: u32, consume: bool) -> bool {
    if let Some(i) = p.timers_due.iter().position(|&(t, _)| t == h) {
        if p.timers_due[i].1 <= p.clock_us {
            p.timers_due.remove(i);
            if let Some(Handle::Event { signaled, .. }) = p.handles.get_mut(&h) {
                *signaled = true;
            }
        }
    }
    let thread_done = |p: &Process, id: u32| p.threads.iter().find(|t| t.id == id).is_none_or(|t| matches!(t.state, ThreadState::Exited(_)));
    match p.handles.get(&h) {
        Some(Handle::Event { .. }) => {
            let Some(Handle::Event { signaled, manual }) = p.handles.get_mut(&h) else { unreachable!() };
            let s = *signaled;
            if s && consume && !*manual {
                *signaled = false;
            }
            s
        }
        Some(Handle::Thread { id }) => {
            let id = *id;
            thread_done(p, id)
        }
        // Mutexes and semaphores are always available (one thread runs at a time).
        _ => true,
    }
}

/// Whether secondary thread `i` can run now. With `wake`, a satisfied wait is
/// consumed and its result kept for the thread's EAX.
pub fn ready(p: &mut Process, i: usize, wake: bool) -> bool {
    let now = p.clock_us;
    let state = p.threads[i].state.clone();
    let apc = p.threads[i].alertable && !p.threads[i].apcs.is_empty();
    let result = match state {
        // a queued APC ends an alertable wait (it runs as the thread waits again)
        ThreadState::Waiting { .. } | ThreadState::Sleeping(_) if apc => Some(Some(WAIT_IO_COMPLETION)),
        ThreadState::Ready => Some(None),
        ThreadState::Sleeping(t) if t <= now => Some(None),
        ThreadState::Waiting { handles, all, until } => {
            if all {
                if handles.iter().all(|&h| signaled(p, h, false)) {
                    if wake {
                        for &h in &handles {
                            signaled(p, h, true);
                        }
                    }
                    Some(Some(WAIT_OBJECT_0))
                } else if until.is_some_and(|u| u <= now) {
                    Some(Some(WAIT_TIMEOUT))
                } else {
                    None
                }
            } else if let Some(k) = handles.iter().position(|&h| signaled(p, h, false)) {
                if wake {
                    signaled(p, handles[k], true);
                }
                Some(Some(WAIT_OBJECT_0 + k as u32))
            } else if until.is_some_and(|u| u <= now) {
                Some(Some(WAIT_TIMEOUT))
            } else {
                None
            }
        }
        ThreadState::CritSec(cs) => {
            // free (the owner left it): the thread takes it as it wakes
            if cs_owner(p, cs) == 0 { Some(None) } else { None }
        }
        // timed out (FALSE): then the critical section again, as after a wake
        ThreadState::CondVar { cs, until, .. } if until.is_some_and(|u| u <= now) => {
            p.threads[i].state = ThreadState::CritSec(cs);
            p.threads[i].resume_eax = Some(0);
            if cs_owner(p, cs) == 0 { Some(Some(0)) } else { None }
        }
        _ => None,
    };
    match result {
        Some(eax) => {
            if wake {
                let t = &mut p.threads[i];
                t.alertable = false;
                if let ThreadState::CritSec(cs) = t.state {
                    let id = t.id;
                    p.cs_pending.push((cs, id));
                }
                let t = &mut p.threads[i];
                if eax.is_some() {
                    t.resume_eax = eax;
                }
            }
            true
        }
        None => false,
    }
}

/// The next virtual time a sleeping or timed-out thread wakes, if any.
fn next_wake(p: &Process) -> Option<u64> {
    p.threads[1..]
        .iter()
        .filter_map(|t| match &t.state {
            ThreadState::Sleeping(at) => Some(*at),
            ThreadState::Waiting { until, .. } | ThreadState::CondVar { until, .. } => *until,
            _ => None,
        })
        .chain(p.timers_due.iter().map(|&(_, at)| at))
        .filter(|&at| at > p.clock_us)
        .min()
}

/// The running secondary thread can block here (it is at its own top level).
fn can_yield(c: &Ctx) -> bool {
    c.p.current != 0 && Some(c.p.nest) == c.p.slice_nest
}

/// Blocks the running secondary thread in `state` (the API returns, then the
/// scheduler takes over).
fn block(c: &mut Ctx, state: ThreadState) {
    c.p.current_thread_mut().spin_polls = 0;
    c.p.current_thread_mut().state = state;
    c.host.yield_thread();
}

/// The main thread waits until `done` (taking its result) or `until`: the clock
/// moves to each moment something can change, running the threads that wake.
fn main_wait(c: &mut Ctx, until: Option<u64>, mut done: impl FnMut(&mut Process) -> Option<u32>) -> u32 {
    loop {
        if let Some(r) = done(c.p) {
            return r;
        }
        c.host.run_threads(c.p);
        if let Some(r) = done(c.p) {
            return r;
        }
        let next = match (next_wake(c.p), until) {
            (Some(a), Some(b)) => a.min(b),
            (a, b) => a.or(b).unwrap_or(c.p.clock_us),
        };

        if next <= c.p.clock_us {
            if until.is_some_and(|u| u <= c.p.clock_us) {
                return WAIT_TIMEOUT;
            }
            // Nothing can change any more: a deadlock in the guest.
            c.p.log("wait: no thread can make progress".into());
            return WAIT_FAILED;
        }
        let step = next - c.p.clock_us;
        c.advance_clock(step);
    }
}

/// An NT timeout (LARGE_INTEGER*, NULL = infinite; negative = relative, in 100 ns
/// units; an absolute time counts as already due) in milliseconds.
fn nt_timeout_ms(c: &Ctx, p: u32) -> u32 {
    if p == 0 {
        return INFINITE;
    }
    let t = c.host.read_u64(p).unwrap_or(0) as i64;
    if t >= 0 { 0 } else { ((-t) / 10_000).min(INFINITE as i64 - 1) as u32 }
}

fn timeout_until(c: &Ctx, ms: u32) -> Option<u64> {
    (ms != INFINITE).then(|| c.p.clock_us + ms as u64 * 1000)
}

/// Sleep / SleepEx.
fn sleep(c: &mut Ctx, ms: u32) {
    if c.p.current == 0 {
        if ms == 0 {
            c.host.run_threads(c.p);
            return;
        }
        let target = c.p.clock_us + ms as u64 * 1000;
        while c.p.clock_us < target {
            let next = next_wake(c.p).map_or(target, |t| t.min(target));
            let step = next.max(c.p.clock_us + 1) - c.p.clock_us;
            c.advance_clock(step);
        }
    } else if can_yield(c) {
        let at = c.p.clock_us + ms as u64 * 1000;
        block(c, if ms == 0 { ThreadState::Ready } else { ThreadState::Sleeping(at) });
    }
}

/// Runs the APCs queued to the running thread (QueueUserAPC); whether there were any.
fn run_apcs(c: &mut Ctx) -> Result<bool, super::ApiError> {
    let mut any = false;
    while !c.p.current_thread_mut().apcs.is_empty() {
        let (f, data) = c.p.current_thread_mut().apcs.remove(0);
        c.call_guest64(f, &[data])?;
        any = true;
    }
    Ok(any)
}

/// A wait or sleep that can be alertable: the APCs queued to the thread run first and
/// end it with WAIT_IO_COMPLETION, and one queued while a secondary thread is blocked in
/// it wakes the thread with that result (the APC runs as it waits again: BASS's thread
/// waits alertably in a loop).
pub fn alertable_wait(c: &mut Ctx, alertable: bool, wait: impl FnOnce(&mut Ctx) -> u32) -> Result<u32, super::ApiError> {
    if alertable && run_apcs(c)? {
        return Ok(WAIT_IO_COMPLETION);
    }
    c.p.current_thread_mut().alertable = alertable;
    let r = wait(c);
    let t = c.p.current_thread_mut();
    if t.state == ThreadState::Running {
        t.alertable = false;
    }
    Ok(r)
}

fn wait(c: &mut Ctx, handles: Vec<u32>, all: bool, ms: u32) -> u32 {
    let until = timeout_until(c, ms);
    let check = |p: &mut Process| -> Option<u32> {
        if all {
            if handles.iter().all(|&h| signaled(p, h, false)) {
                for &h in &handles {
                    signaled(p, h, true);
                }
                return Some(WAIT_OBJECT_0);
            }
            None
        } else {
            handles.iter().position(|&h| signaled(p, h, true)).map(|k| WAIT_OBJECT_0 + k as u32)
        }
    };
    if c.p.current == 0 {
        if ms == 0 {
            let chk = check;
            return chk(c.p).unwrap_or(WAIT_TIMEOUT);
        }
        return main_wait(c, until, check);
    }
    let chk = check;
    if let Some(r) = chk(c.p) {
        return r;
    }
    if ms == 0 || !can_yield(c) {
        return WAIT_TIMEOUT;
    }
    block(c, ThreadState::Waiting { handles, all, until });
    WAIT_TIMEOUT // replaced by the result when the thread wakes
}

/// MsgWaitForMultipleObjects(Ex): `handles` as `wait` (WAIT_OBJECT_0 + k), or a queued
/// message of the kinds `mask` names (WAIT_OBJECT_0 + n). Sent messages are delivered as
/// they are sent here, so a mask of QS_SENDMESSAGE alone (Delphi's TThread.WaitFor) waits
/// for the objects only. The main thread lets the other threads run meanwhile; without
/// handles, or when nothing can change, the clock moves on by the timeout (10 s for
/// INFINITE, which then reports a message) and the timers are pumped.
pub fn msg_wait(c: &mut Ctx, handles: Vec<u32>, all: bool, ms: u32, mask: u32) -> u32 {
    const QS_SENDMESSAGE: u32 = 0x40;
    let n = handles.len() as u32;
    let posted = mask & !QS_SENDMESSAGE != 0;
    let mut check = |p: &mut Process| -> Option<u32> {
        if all {
            if !handles.is_empty() && handles.iter().all(|&h| signaled(p, h, false)) {
                for &h in &handles {
                    signaled(p, h, true);
                }
                return Some(WAIT_OBJECT_0);
            }
        } else if let Some(k) = handles.iter().position(|&h| signaled(p, h, true)) {
            return Some(WAIT_OBJECT_0 + k as u32);
        }
        (posted && !p.messages.is_empty()).then_some(WAIT_OBJECT_0 + n)
    };
    if let Some(r) = check(c.p) {
        return r;
    }
    let until = c.p.clock_us + if ms == INFINITE { 10_000 } else { ms as u64 } * 1000;
    if c.p.current == 0 && n > 0 && ms != 0 {
        let r = main_wait(c, Some(until), &mut check);
        if r != WAIT_TIMEOUT && r != WAIT_FAILED {
            return r;
        }
    }
    if until > c.p.clock_us {
        c.advance_clock(until - c.p.clock_us);
    }
    super::user32::pump_timers(c);
    if let Some(r) = check(c.p) {
        return r;
    }
    if ms == INFINITE { WAIT_OBJECT_0 + n } else { WAIT_TIMEOUT }
}

// ---- critical sections --------------------------------------------------------------------
// CRITICAL_SECTION: DebugInfo, LockCount, RecursionCount, OwningThread, LockSemaphore, SpinCount

fn cs_owner(p: &Process, cs: u32) -> u32 {
    p.cs_owners.get(&cs).copied().unwrap_or(0)
}

/// CRITICAL_SECTION offsets of LockCount, RecursionCount and OwningThread.
pub fn cs_fields(c: &Ctx) -> [u32; 3] {
    if c.x64() { [8, 12, 16] } else { [4, 8, 12] }
}

/// SleepConditionVariable*: lets go of `cs` and sleeps until a wake on `cv` or `ms`
/// pass, then takes `cs` again. TRUE when woken; FALSE (ERROR_TIMEOUT) at the timeout.
/// A secondary thread blocks (it gets `cs` back as it wakes); the main thread lets time
/// and the other threads run meanwhile.
fn sleep_cv(c: &mut Ctx, cv: u32, cs: u32, ms: u32) -> u32 {
    let until = timeout_until(c, ms);
    leave_cs(c, cs);
    if c.p.current == 0 {
        c.p.main_cv = Some((cv, false));
        let r = main_wait(c, until, |p| matches!(p.main_cv, Some((_, true))).then_some(0));
        c.p.main_cv = None;
        enter_cs(c, cs, false);
        if r != 0 {
            c.set_last_error(1460);
            return 0;
        }
        return 1;
    }
    if can_yield(c) {
        // (TRUE unless the timeout comes first: `ready`)
        c.p.current_thread_mut().resume_eax = Some(1);
        block(c, ThreadState::CondVar { cv, cs, until });
        return 1;
    }
    enter_cs(c, cs, false);
    1
}

/// Wakes one (or every) thread sleeping in `cv`: it waits for its critical section.
fn wake_cv(c: &mut Ctx, cv: u32, all: bool) {
    for i in 1..c.p.threads.len() {
        if let ThreadState::CondVar { cv: v, cs, .. } = c.p.threads[i].state {
            if v == cv {
                c.p.threads[i].state = ThreadState::CritSec(cs);
                c.p.threads[i].resume_eax = Some(1);
                if !all {
                    return;
                }
            }
        }
    }
    if let Some((v, woken)) = c.p.main_cv.as_mut() {
        if *v == cv {
            *woken = true;
        }
    }
}

/// The critical section a slim reader/writer lock is taken as (made at first use).
fn srw_cs(c: &mut Ctx, srw: u32) -> u32 {
    if let Some(&cs) = c.p.srw_locks.get(&srw) {
        return cs;
    }
    let heap = c.p.process_heap;
    let g: &mut dyn Guest = &mut *c.host;
    let cs = c.p.heap_alloc(g, heap, 40, true);
    let lock = cs_fields(c)[0];
    let _ = c.host.write_u32(cs + lock, u32::MAX);
    c.p.srw_locks.insert(srw, cs);
    cs
}

fn cs_take(c: &mut Ctx, cs: u32, tid: u32) {
    let [lock, recursion, owner] = cs_fields(c);
    let rec = c.host.read_u32(cs + recursion).unwrap_or(0);
    let rec = if cs_owner(c.p, cs) == tid { rec + 1 } else { 1 };
    c.p.cs_owners.insert(cs, tid);
    let _ = c.host.write_u32(cs + lock, 0);
    let _ = c.host.write_u32(cs + recursion, rec);
    let _ = c.host.write_u32(cs + owner, tid);
}

/// Hands critical sections to the threads that woke up waiting for them.
fn take_pending(c: &mut Ctx) {
    let pending = std::mem::take(&mut c.p.cs_pending);
    for (cs, tid) in pending {
        cs_take(c, cs, tid);
    }
}

fn enter_cs(c: &mut Ctx, cs: u32, try_only: bool) -> bool {
    take_pending(c);
    let tid = c.p.current_thread_id();
    let owner = cs_owner(c.p, cs);
    if owner == 0 || owner == tid {
        cs_take(c, cs, tid);
        return true;
    }
    if try_only {
        return false;
    }
    if c.p.current == 0 {
        let r = main_wait(c, None, |p| (cs_owner(p, cs) == 0).then_some(0));
        if r == 0 {
            cs_take(c, cs, tid);
            return true;
        }
        // the owner never lets go: take it anyway rather than hang
        cs_take(c, cs, tid);
        return true;
    }
    if can_yield(c) {
        block(c, ThreadState::CritSec(cs));
        return true;
    }
    cs_take(c, cs, tid);
    true
}

fn leave_cs(c: &mut Ctx, cs: u32) {
    take_pending(c);
    let [lock, recursion, owner] = cs_fields(c);
    let rec = c.host.read_u32(cs + recursion).unwrap_or(1).saturating_sub(1);
    let _ = c.host.write_u32(cs + recursion, rec);
    if rec == 0 {
        c.p.cs_owners.remove(&cs);
        let _ = c.host.write_u32(cs + owner, 0);
        let _ = c.host.write_u32(cs + lock, u32::MAX);
    }
}

// ---- thread creation ----------------------------------------------------------------------

/// Allocates a stack and a TEB (with the x87 state page below it) and prepares the
/// registers that call `start(param)` returning to THREAD_EXIT.
fn create_thread(c: &mut Ctx, start: u32, param: u64, stack_size: u32, suspended: bool) -> Result<(u32, u32), String> {
    let stack_size = stack_size.max(0x10_0000).div_ceil(0x10000) * 0x10000;
    let stack = c.p.vm.alloc(0, stack_size, true, crate::mem::PAGE_READWRITE, false, "thread stack").ok_or("no memory for a thread stack")?;
    c.host.ensure_mapped(stack, stack_size).map_err(|e| e.to_string())?;
    // (the x87 state's page, then the TEB: two pages in a 64-bit process)
    let state_size = if c.x64() { 0x3000 } else { 0x2000 };
    let state = c.p.vm.alloc(0, state_size, true, crate::mem::PAGE_READWRITE, false, "thread state").ok_or("no memory for a TEB")?;
    c.host.ensure_mapped(state, state_size).map_err(|e| e.to_string())?;
    let _ = c.host.fill(state, state_size, 0);
    let teb = state + 0x1000;
    let id = 0x1300 + 4 * c.p.threads.len() as u32;
    let top = stack + stack_size;
    // x87: as Windows starts threads (53-bit precision, exceptions masked)
    let mut f = fpu::Fpu::new();
    f.set_control_word(0x027F);
    let mut block = [0u8; fpu::block::SIZE];
    f.to_block(&mut block);
    let _ = c.host.write(teb - fpu::block::OFFSET, &block);
    let main_teb = c.p.threads[0].teb;
    let peb = c.host.read_u32(main_teb + c.teb_off(TebField::Peb)).unwrap_or(0);
    let x64 = c.x64();
    for (f, v) in [(TebField::ExceptionList, if x64 { 0 } else { u32::MAX }), (TebField::StackBase, top), (TebField::StackLimit, stack), (TebField::Self_, teb), (TebField::ProcessId, 0x1234), (TebField::ThreadId, id), (TebField::Peb, peb)] {
        let _ = c.host.write_u32(teb + f.offset(x64), v);
    }
    // static TLS: a copy of every module's template
    let templates: Vec<(u32, u32, u32)> = c.p.modules.iter().filter_map(|m| m.tls_template).collect();
    if !templates.is_empty() {
        let heap = c.p.process_heap;
        let g: &mut dyn Guest = &mut *c.host;
        let ps = if x64 { 8 } else { 4 };
        let array = c.p.heap_alloc(g, heap, ps * 64, true);
        for (i, (s, e, zero)) in templates.into_iter().enumerate() {
            let len = e.saturating_sub(s);
            let g: &mut dyn Guest = &mut *c.host;
            let b = c.p.heap_alloc(g, heap, len + zero + 16, true);
            let mut buf = vec![0u8; len as usize];
            let _ = c.host.read(s, &mut buf);
            let _ = c.host.write(b, &buf);
            let _ = c.host.write_u32(array + ps * i as u32, b);
        }
        let _ = c.host.write_u32(teb + c.teb_off(TebField::TlsPointer), array);
    }
    let regs = if x64 {
        // Win64: the parameter in rcx, the return address below 32 bytes of shadow space
        let rsp = top - 0x40 - 8;
        let _ = c.host.write_u64(rsp, THREAD_EXIT as u64);
        let mut r64 = [0u64; 16];
        r64[1] = param;
        r64[4] = rsp as u64;
        super::Regs { ecx: param as u32, esp: rsp, eip: start, fs_base: teb, r64: Some(r64), ..Default::default() }
    } else {
        let esp = top - 16 - 8;
        let _ = c.host.write_u32(esp, THREAD_EXIT);
        let _ = c.host.write_u32(esp + 4, param as u32);
        super::Regs { eax: 0, ecx: 0, edx: 0, ebx: 0, esp, ebp: 0, esi: 0, edi: 0, eip: start, fs_base: teb, r64: None }
    };
    let h = c.p.new_handle(Handle::Thread { id });
    let ctx = ThreadCtx { regs, flags: [0; 7], xmm: [0; 16], mxcsr: 0x1F80 };
    c.p.threads.push(Thread {
        id,
        teb,
        last_error: 0,
        stack_base: top,
        stack_limit: stack,
        handle: h,
        state: if suspended { ThreadState::Suspended(1) } else { ThreadState::Ready },
        ctx: Some(Box::new(ctx)),
        resume_eax: None,
        spin_polls: 0,
        apcs: Vec::new(),
        alertable: false,
    });
    c.p.log(format!("CreateThread(start={start:#x}, param={param:#x}) = thread {id:#x}"));
    Ok((h, id))
}

fn thread_index(p: &Process, h: u32) -> Option<usize> {
    if h == 0xFFFF_FFFE {
        return Some(p.current);
    }
    match p.handles.get(&h) {
        Some(Handle::Thread { id }) => p.threads.iter().position(|t| t.id == *id),
        _ => None,
    }
}

pub fn register(r: &mut Registry) {
    let k = "kernel32.dll";
    r.add(k, "CreateThread", |c| {
        let (stack, start, param, flags, pid) = (c.arg(1), c.arg(2), c.arg64(3), c.arg(4), c.arg(5));
        match create_thread(c, start, param, stack, flags & CREATE_SUSPENDED != 0) {
            Ok((h, id)) => {
                if pid != 0 {
                    let _ = c.host.write_u32(pid, id);
                }
                ok(6, h)
            }
            Err(e) => {
                c.p.log(format!("CreateThread failed: {e}"));
                c.set_last_error(8);
                ok(6, 0)
            }
        }
    });
    r.add(k, "ExitThread", |c| {
        let code = c.arg(0);
        if c.p.current == 0 {
            c.p.exit_code = Some(code);
            c.host.request_exit(code);
        } else {
            block(c, ThreadState::Exited(code));
        }
        ok(1, 0)
    });
    r.add(k, "FreeLibraryAndExitThread", |c| {
        // FreeLibrary does nothing here (modules stay loaded).
        let code = c.arg(1);
        if c.p.current == 0 {
            c.p.exit_code = Some(code);
            c.host.request_exit(code);
        } else {
            block(c, ThreadState::Exited(code));
        }
        ok(2, 0)
    });
    r.add(k, "TerminateThread", |c| {
        let (h, code) = (c.arg(0), c.arg(1));
        if let Some(i) = thread_index(c.p, h) {
            if i != 0 {
                c.p.threads[i].state = ThreadState::Exited(code);
                if i == c.p.current {
                    c.host.yield_thread();
                }
            }
        }
        ok(2, 1)
    });
    r.add(k, "GetExitCodeThread", |c| {
        let (h, out) = (c.arg(0), c.arg(1));
        let code = match thread_index(c.p, h).map(|i| &c.p.threads[i].state) {
            Some(ThreadState::Exited(code)) => *code,
            Some(_) => STILL_ACTIVE,
            None => return ok(2, 0),
        };
        let _ = c.host.write_u32(out, code);
        ok(2, 1)
    });
    r.add(k, "ResumeThread", |c| {
        let h = c.arg(0);
        let Some(i) = thread_index(c.p, h) else { return ok(1, u32::MAX) };
        let t = &mut c.p.threads[i];
        let prev = match t.state {
            ThreadState::Suspended(n) => {
                t.state = if n <= 1 { ThreadState::Ready } else { ThreadState::Suspended(n - 1) };
                n
            }
            _ => 0,
        };
        ok(1, prev)
    });
    r.add(k, "SuspendThread", |c| {
        let h = c.arg(0);
        let Some(i) = thread_index(c.p, h) else { return ok(1, u32::MAX) };
        if i == 0 || i == c.p.current {
            // a thread suspending itself, or the main thread: not supported
            return ok(1, 0);
        }
        let t = &mut c.p.threads[i];
        let prev = match t.state {
            ThreadState::Suspended(n) => {
                t.state = ThreadState::Suspended(n + 1);
                n
            }
            ThreadState::Ready | ThreadState::Sleeping(_) => {
                t.state = ThreadState::Suspended(1);
                0
            }
            _ => 0,
        };
        ok(1, prev)
    });
    r.add(k, "GetThreadId", |c| {
        let h = c.arg(0);
        let id = thread_index(c.p, h).map(|i| c.p.threads[i].id).unwrap_or(0);
        ok(1, id)
    });
    r.add(k, "Sleep", |c| {
        let ms = c.arg(0);
        sleep(c, ms);
        ok(1, 0)
    });
    r.add(k, "SleepEx", |c| {
        let ms = c.arg(0);
        let r = alertable_wait(c, c.arg(1) != 0, |c| {
            sleep(c, ms);
            0
        })?;
        ok(2, r)
    });
    r.add(k, "SwitchToThread", |c| {
        sleep(c, 0);
        ok(0, 1)
    });
    r.add(k, "WaitForSingleObject", |c| {
        let (h, ms) = (c.arg(0), c.arg(1));
        let r = wait(c, vec![h], false, ms);
        ok(2, r)
    });
    r.add("ntdll.dll", "NtDelayExecution", |c| {
        let ms = nt_timeout_ms(c, c.arg(1));
        let r = alertable_wait(c, c.arg(0) != 0, |c| {
            sleep(c, if ms == INFINITE { 0 } else { ms });
            0
        })?;
        ok(2, r) // STATUS_USER_APC is WAIT_IO_COMPLETION's value
    });
    r.add("ntdll.dll", "NtWaitForSingleObject", |c| {
        let (h, ms) = (c.arg(0), nt_timeout_ms(c, c.arg(2)));
        let r = alertable_wait(c, c.arg(1) != 0, |c| wait(c, vec![h], false, ms))?;
        ok(3, r) // STATUS_WAIT_0 / STATUS_TIMEOUT have the Win32 values
    });
    r.add(k, "WaitForSingleObjectEx", |c| {
        let (h, ms) = (c.arg(0), c.arg(1));
        let r = alertable_wait(c, c.arg(2) != 0, |c| wait(c, vec![h], false, ms))?;
        ok(3, r)
    });
    r.add(k, "WaitForMultipleObjects", |c| {
        let (n, p, all, ms) = (c.arg(0), c.arg(1), c.arg(2) != 0, c.arg(3));
        let handles: Vec<u32> = (0..n.min(64)).map(|i| c.host.read_u32(p + c.ptr_size() * i).unwrap_or(0)).collect();
        let r = wait(c, handles, all, ms);
        ok(4, r)
    });
    r.add(k, "WaitForMultipleObjectsEx", |c| {
        let (n, p, all, ms) = (c.arg(0), c.arg(1), c.arg(2) != 0, c.arg(3));
        let handles: Vec<u32> = (0..n.min(64)).map(|i| c.host.read_u32(p + c.ptr_size() * i).unwrap_or(0)).collect();
        let r = alertable_wait(c, c.arg(4) != 0, |c| wait(c, handles, all, ms))?;
        ok(5, r)
    });
    // I/O completion ports, for packets the program posts itself (no I/O completes here:
    // files are read at once and sockets have no overlapped I/O). A get with none
    // waiting fails with WAIT_TIMEOUT, as a timeout does; a secondary thread sleeps a
    // little first, as Boost.Asio's thread asks again at once.
    r.add_on_demand(k, "CreateIoCompletionPort", |c| {
        let (file, port) = (c.arg(0), c.arg(1));
        if port != 0 {
            return ok(4, if c.p.ports.contains_key(&port) { port } else { 0 });
        }
        let _ = file;
        let h = c.p.new_handle(Handle::Event { manual: true, signaled: false });
        c.p.ports.insert(h, Default::default());
        ok(4, h)
    });
    r.add_on_demand(k, "PostQueuedCompletionStatus", |c| {
        let (port, bytes, key, ov) = (c.arg(0), c.arg(1), c.arg64(2), c.arg64(3));
        let Some(q) = c.p.ports.get_mut(&port) else { return ok(4, 0) };
        q.push_back((bytes, key, ov));
        if let Some(Handle::Event { signaled, .. }) = c.p.handles.get_mut(&port) {
            *signaled = true;
        }
        ok(4, 1)
    });
    r.add_on_demand(k, "GetQueuedCompletionStatus", |c| {
        let (port, p_bytes, p_key, p_ov, ms) = (c.arg(0), c.arg(1), c.arg(2), c.arg(3), c.arg(4));
        let take = |c: &mut Ctx| -> bool {
            let Some(q) = c.p.ports.get_mut(&port) else { return false };
            let Some((bytes, key, ov)) = q.pop_front() else { return false };
            let empty = q.is_empty();
            if empty && let Some(Handle::Event { signaled, .. }) = c.p.handles.get_mut(&port) {
                *signaled = false;
            }
            let _ = c.host.write_u32(p_bytes, bytes);
            for (at, v) in [(p_key, key), (p_ov, ov)] {
                let _ = if c.x64() { c.host.write_u64(at, v) } else { c.host.write_u32(at, v as u32) };
            }
            true
        };
        if !c.p.ports.contains_key(&port) {
            c.set_last_error(6); // ERROR_INVALID_HANDLE
            return ok(5, 0);
        }
        if take(c) {
            return ok(5, 1);
        }
        let _ = c.host.write_u32(p_bytes, 0);
        c.write_ptr(p_key, 0);
        c.write_ptr(p_ov, 0);
        if ms != 0 {
            if c.p.current == 0 {
                if wait(c, vec![port], false, ms) == WAIT_OBJECT_0 && take(c) {
                    return ok(5, 1);
                }
            } else {
                sleep(c, ms.min(100));
            }
        }
        c.set_last_error(WAIT_TIMEOUT);
        ok(5, 0)
    });
    // An APC for a thread: it runs at the thread's next alertable wait.
    r.add_on_demand(k, "QueueUserAPC", |c| {
        let (f, h, data) = (c.arg(0), c.arg(1), c.arg64(2));
        let Some(i) = thread_index(c.p, h) else { return ok(3, 0) };
        if matches!(c.p.threads[i].state, ThreadState::Exited(_)) {
            return ok(3, 0);
        }
        c.p.threads[i].apcs.push((f, data));
        ok(3, 1)
    });
    r.add(k, "EnterCriticalSection", |c| {
        let cs = c.arg(0);
        enter_cs(c, cs, false);
        ok(1, 0)
    });
    r.add(k, "TryEnterCriticalSection", |c| {
        let cs = c.arg(0);
        let got = enter_cs(c, cs, true);
        ok(1, got as u32)
    });
    r.add(k, "LeaveCriticalSection", |c| {
        let cs = c.arg(0);
        leave_cs(c, cs);
        ok(1, 0)
    });
    r.add("ntdll.dll", "RtlIsCriticalSectionLockedByThread", |c| {
        let tid = c.p.current_thread_id();
        ok(1, (cs_owner(c.p, c.arg(0)) == tid) as u32)
    });
    // Condition variables: a sleep releases the critical section, lets the other
    // threads run, and takes it back. Wakeups may be spurious (callers re-check their
    // predicate), so Wake* have nothing to do.
    r.add(k, "InitializeConditionVariable", |c| {
        let _ = c.host.write_u32(c.arg(0), 0);
        ok(1, 0)
    });
    r.add(k, "SleepConditionVariableCS", |c| {
        let (cv, cs, ms) = (c.arg(0), c.arg(1), c.arg(2));
        let r = sleep_cv(c, cv, cs, ms);
        ok(3, r)
    });
    // Slim reader/writer locks, shared or not, are taken as a critical section of their
    // own (recursive for the owner, which a shared lock may be); the lock word says
    // whether one is held.
    r.add_on_demand(k, "InitializeSRWLock", |c| {
        let _ = c.host.write_u32(c.arg(0), 0);
        ok(1, 0)
    });
    for name in ["AcquireSRWLockExclusive", "AcquireSRWLockShared"] {
        r.add_on_demand(k, name, |c| {
            let srw = c.arg(0);
            let cs = srw_cs(c, srw);
            enter_cs(c, cs, false);
            let _ = c.host.write_u32(srw, 1);
            ok(1, 0)
        });
    }
    for name in ["TryAcquireSRWLockExclusive", "TryAcquireSRWLockShared"] {
        r.add_on_demand(k, name, |c| {
            let srw = c.arg(0);
            let cs = srw_cs(c, srw);
            let got = enter_cs(c, cs, true);
            if got {
                let _ = c.host.write_u32(srw, 1);
            }
            ok(1, got as u32)
        });
    }
    for name in ["ReleaseSRWLockExclusive", "ReleaseSRWLockShared"] {
        r.add_on_demand(k, name, |c| {
            let srw = c.arg(0);
            let cs = srw_cs(c, srw);
            leave_cs(c, cs);
            if cs_owner(c.p, cs) == 0 {
                let _ = c.host.write_u32(srw, 0);
            }
            ok(1, 0)
        });
    }
    r.add_on_demand(k, "SleepConditionVariableSRW", |c| {
        let (cv, srw, ms) = (c.arg(0), c.arg(1), c.arg(2));
        let cs = srw_cs(c, srw);
        let r = sleep_cv(c, cv, cs, ms);
        let _ = c.host.write_u32(srw, 1);
        ok(4, r)
    });
    r.add(k, "WakeConditionVariable", |c| {
        wake_cv(c, c.arg(0), false);
        ok(1, 0)
    });
    r.add(k, "WakeAllConditionVariable", |c| {
        wake_cv(c, c.arg(0), true);
        ok(1, 0)
    });
    r.alias("ntdll.dll", "RtlEnterCriticalSection", k, "EnterCriticalSection");
    r.alias("ntdll.dll", "RtlLeaveCriticalSection", k, "LeaveCriticalSection");
    r.alias("ntdll.dll", "RtlTryEnterCriticalSection", k, "TryEnterCriticalSection");
}
