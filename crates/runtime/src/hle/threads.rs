//! Guest threads, run one at a time. The main thread runs as the host calls it;
//! secondary threads run from its blocking points (sleeps, waits, idle message
//! loops: anything that advances the virtual clock), each until it blocks again,
//! ends, or uses up a dispatch budget. A secondary thread blocks by yielding back
//! to the scheduler, which it can only do at its own top level (not inside a
//! callback the HLE made into it); there a wait that cannot be satisfied returns
//! as if it timed out. Scheduling depends only on the virtual clock and guest
//! behavior, so runs stay deterministic.

use super::{ApiResult, Ctx, Registry, Ret};
use crate::mem::Guest;
use crate::process::{Handle, Process, Thread, ThreadCtx, ThreadState, THREAD_EXIT};

const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 0x102;
const WAIT_FAILED: u32 = 0xFFFF_FFFF;
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
    let result = match state {
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
        _ => None,
    };
    match result {
        Some(eax) => {
            if wake {
                let t = &mut p.threads[i];
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
            ThreadState::Waiting { until, .. } => *until,
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

// ---- critical sections --------------------------------------------------------------------
// CRITICAL_SECTION: DebugInfo, LockCount, RecursionCount, OwningThread, LockSemaphore, SpinCount

fn cs_owner(p: &Process, cs: u32) -> u32 {
    p.cs_owners.get(&cs).copied().unwrap_or(0)
}

fn cs_take(c: &mut Ctx, cs: u32, tid: u32) {
    let rec = c.host.read_u32(cs + 8).unwrap_or(0);
    let rec = if cs_owner(c.p, cs) == tid { rec + 1 } else { 1 };
    c.p.cs_owners.insert(cs, tid);
    let _ = c.host.write_u32(cs + 4, 0);
    let _ = c.host.write_u32(cs + 8, rec);
    let _ = c.host.write_u32(cs + 12, tid);
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
    let rec = c.host.read_u32(cs + 8).unwrap_or(1).saturating_sub(1);
    let _ = c.host.write_u32(cs + 8, rec);
    if rec == 0 {
        c.p.cs_owners.remove(&cs);
        let _ = c.host.write_u32(cs + 12, 0);
        let _ = c.host.write_u32(cs + 4, u32::MAX);
    }
}

// ---- thread creation ----------------------------------------------------------------------

/// Allocates a stack and a TEB (with the x87 state page below it) and prepares the
/// registers that call `start(param)` returning to THREAD_EXIT.
fn create_thread(c: &mut Ctx, start: u32, param: u32, stack_size: u32, suspended: bool) -> Result<(u32, u32), String> {
    let stack_size = stack_size.max(0x10_0000).div_ceil(0x10000) * 0x10000;
    let stack = c.p.vm.alloc(0, stack_size, true, crate::mem::PAGE_READWRITE, false, "thread stack").ok_or("no memory for a thread stack")?;
    c.host.ensure_mapped(stack, stack_size).map_err(|e| e.to_string())?;
    let state = c.p.vm.alloc(0, 0x2000, true, crate::mem::PAGE_READWRITE, false, "thread state").ok_or("no memory for a TEB")?;
    c.host.ensure_mapped(state, 0x2000).map_err(|e| e.to_string())?;
    let _ = c.host.fill(state, 0x2000, 0);
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
    let peb = c.host.read_u32(main_teb + 0x30).unwrap_or(0);
    for (off, v) in [(0x00, u32::MAX), (0x04, top), (0x08, stack), (0x18, teb), (0x20, 0x1234), (0x24, id), (0x30, peb)] {
        let _ = c.host.write_u32(teb + off, v);
    }
    // static TLS: a copy of every module's template
    let templates: Vec<(u32, u32, u32)> = c.p.modules.iter().filter_map(|m| m.tls_template).collect();
    if !templates.is_empty() {
        let heap = c.p.process_heap;
        let g: &mut dyn Guest = &mut *c.host;
        let array = c.p.heap_alloc(g, heap, 4 * 64, true);
        for (i, (s, e, zero)) in templates.into_iter().enumerate() {
            let len = e.saturating_sub(s);
            let g: &mut dyn Guest = &mut *c.host;
            let b = c.p.heap_alloc(g, heap, len + zero + 16, true);
            let mut buf = vec![0u8; len as usize];
            let _ = c.host.read(s, &mut buf);
            let _ = c.host.write(b, &buf);
            let _ = c.host.write_u32(array + 4 * i as u32, b);
        }
        let _ = c.host.write_u32(teb + 0x2C, array);
    }
    let esp = top - 16 - 8;
    let _ = c.host.write_u32(esp, THREAD_EXIT);
    let _ = c.host.write_u32(esp + 4, param);
    let regs = super::Regs { eax: 0, ecx: 0, edx: 0, ebx: 0, esp, ebp: 0, esi: 0, edi: 0, eip: start, fs_base: teb };
    let h = c.p.new_handle(Handle::Thread { id });
    let ctx = ThreadCtx { regs, flags: [0; 7], xmm: [0; 8], mxcsr: 0x1F80 };
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
        let (stack, start, param, flags, pid) = (c.arg(1), c.arg(2), c.arg(3), c.arg(4), c.arg(5));
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
        sleep(c, ms);
        ok(2, 0)
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
    r.add(k, "WaitForSingleObjectEx", |c| {
        let (h, ms) = (c.arg(0), c.arg(1));
        let r = wait(c, vec![h], false, ms);
        ok(3, r)
    });
    r.add(k, "WaitForMultipleObjects", |c| {
        let (n, p, all, ms) = (c.arg(0), c.arg(1), c.arg(2) != 0, c.arg(3));
        let handles: Vec<u32> = (0..n.min(64)).map(|i| c.host.read_u32(p + 4 * i).unwrap_or(0)).collect();
        let r = wait(c, handles, all, ms);
        ok(4, r)
    });
    r.add(k, "WaitForMultipleObjectsEx", |c| {
        let (n, p, all, ms) = (c.arg(0), c.arg(1), c.arg(2) != 0, c.arg(3));
        let handles: Vec<u32> = (0..n.min(64)).map(|i| c.host.read_u32(p + 4 * i).unwrap_or(0)).collect();
        let r = wait(c, handles, all, ms);
        ok(5, r)
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
    r.alias("ntdll.dll", "RtlEnterCriticalSection", k, "EnterCriticalSection");
    r.alias("ntdll.dll", "RtlLeaveCriticalSection", k, "LeaveCriticalSection");
    r.alias("ntdll.dll", "RtlTryEnterCriticalSection", k, "TryEnterCriticalSection");
}
