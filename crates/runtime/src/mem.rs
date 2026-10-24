//! Guest memory access and the virtual address-space map (VirtualAlloc & co).

use std::collections::BTreeMap;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fault {
    pub addr: u32,
    pub write: bool,
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "guest memory fault at {:#010x} ({})", self.addr, if self.write { "write" } else { "read" })
    }
}

/// Raw access to the guest's linear memory. The host grows the backing wasm memory
/// through `ensure_mapped` before the runtime touches new regions.
pub trait Guest {
    fn read(&self, addr: u32, buf: &mut [u8]) -> Result<(), Fault>;
    fn write(&mut self, addr: u32, data: &[u8]) -> Result<(), Fault>;
    fn ensure_mapped(&mut self, addr: u32, size: u32) -> Result<(), Fault>;

    fn read_u8(&self, addr: u32) -> Result<u8, Fault> {
        let mut b = [0u8; 1];
        self.read(addr, &mut b)?;
        Ok(b[0])
    }
    fn read_u16(&self, addr: u32) -> Result<u16, Fault> {
        let mut b = [0u8; 2];
        self.read(addr, &mut b)?;
        Ok(u16::from_le_bytes(b))
    }
    fn read_u32(&self, addr: u32) -> Result<u32, Fault> {
        let mut b = [0u8; 4];
        self.read(addr, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }
    fn read_u64(&self, addr: u32) -> Result<u64, Fault> {
        let mut b = [0u8; 8];
        self.read(addr, &mut b)?;
        Ok(u64::from_le_bytes(b))
    }
    fn write_u8(&mut self, addr: u32, v: u8) -> Result<(), Fault> {
        self.write(addr, &[v])
    }
    fn write_u16(&mut self, addr: u32, v: u16) -> Result<(), Fault> {
        self.write(addr, &v.to_le_bytes())
    }
    fn write_u32(&mut self, addr: u32, v: u32) -> Result<(), Fault> {
        self.write(addr, &v.to_le_bytes())
    }
    fn write_u64(&mut self, addr: u32, v: u64) -> Result<(), Fault> {
        self.write(addr, &v.to_le_bytes())
    }
    /// Copies `size` bytes from `src` to `dst` (overlapping ranges as memmove).
    fn copy_within(&mut self, dst: u32, src: u32, size: u32) -> Result<(), Fault> {
        let mut buf = vec![0u8; size as usize];
        self.read(src, &mut buf)?;
        self.write(dst, &buf)
    }
    fn fill(&mut self, addr: u32, size: u32, byte: u8) -> Result<(), Fault> {
        let chunk = vec![byte; 65536];
        let mut done = 0;
        while done < size {
            let n = (size - done).min(65536);
            self.write(addr + done, &chunk[..n as usize])?;
            done += n;
        }
        Ok(())
    }
    /// NUL-terminated byte string (at most 64 KiB).
    fn read_cstr(&self, addr: u32) -> Result<Vec<u8>, Fault> {
        let mut out = Vec::new();
        let mut a = addr;
        loop {
            let mut chunk = [0u8; 64];
            let n = chunk.len() as u32;
            self.read(a, &mut chunk).or_else(|_| {
                // near the end of mapped memory: read byte by byte
                for (k, c) in chunk.iter_mut().enumerate() {
                    *c = self.read_u8(a + k as u32)?;
                }
                Ok(())
            })?;
            if let Some(p) = chunk.iter().position(|&b| b == 0) {
                out.extend_from_slice(&chunk[..p]);
                return Ok(out);
            }
            out.extend_from_slice(&chunk);
            a += n;
            if out.len() > 65536 {
                return Ok(out);
            }
        }
    }
    fn read_cstr_lossy(&self, addr: u32) -> String {
        self.read_cstr(addr).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default()
    }
    /// NUL-terminated UTF-16 string.
    fn read_wstr(&self, addr: u32) -> Result<String, Fault> {
        let mut units = Vec::new();
        let mut a = addr;
        loop {
            let u = self.read_u16(a)?;
            if u == 0 || units.len() > 32768 {
                break;
            }
            units.push(u);
            a += 2;
        }
        Ok(String::from_utf16_lossy(&units))
    }
    fn write_cstr(&mut self, addr: u32, s: &[u8]) -> Result<(), Fault> {
        self.write(addr, s)?;
        self.write_u8(addr + s.len() as u32, 0)
    }
    fn write_wstr(&mut self, addr: u32, s: &str) -> Result<(), Fault> {
        let mut a = addr;
        for u in s.encode_utf16() {
            self.write_u16(a, u)?;
            a += 2;
        }
        self.write_u16(a, 0)
    }
}

pub const PAGE: u32 = 0x1000;
pub const GRANULARITY: u32 = 0x10000;
/// Where the address space a large-address-aware exe has beyond 2 GB starts.
pub const HIGH_BASE: u32 = 0x8000_0000;
/// Its top (Windows on 64 bits gives such an exe all but the last 64 KB of 4 GB).
pub const HIGH_MAX: u32 = 0xFFFE_0000;

pub const MEM_COMMIT: u32 = 0x1000;
pub const MEM_RESERVE: u32 = 0x2000;
pub const MEM_DECOMMIT: u32 = 0x4000;
pub const MEM_RELEASE: u32 = 0x8000;
pub const MEM_FREE: u32 = 0x10000;
pub const MEM_PRIVATE: u32 = 0x20000;
pub const MEM_TOP_DOWN: u32 = 0x100000;
pub const PAGE_NOACCESS: u32 = 0x01;
pub const PAGE_READONLY: u32 = 0x02;
pub const PAGE_READWRITE: u32 = 0x04;
pub const PAGE_EXECUTE_READ: u32 = 0x20;
pub const PAGE_EXECUTE_READWRITE: u32 = 0x40;

#[derive(Debug, Clone)]
pub struct Region {
    pub size: u32,
    /// Allocation base this region belongs to (regions split by protect/commit changes).
    pub alloc_base: u32,
    pub committed: bool,
    pub protect: u32,
    pub tag: &'static str,
}

/// The address-space map. Backed storage is the host's single linear memory, so this only
/// tracks what the guest believes is reserved/committed and where free space is.
#[derive(Debug, Default)]
pub struct Vm {
    pub regions: BTreeMap<u32, Region>,
    /// Lowest address handed out by bottom-up searches.
    pub min_addr: u32,
    /// Highest usable address + 1 (user space top).
    pub max_addr: u32,
    /// The top of the space above 2 GB a large-address-aware 32-bit exe has as well (0:
    /// none). It is handed out once the space below is full, as Windows gives out low
    /// addresses first (and the guest's memory grows past 2 GB only then).
    pub high_max: u32,
    /// Every range ever handed out (disjoint, merged): memory outside them was never
    /// anyone's, so it still holds the zeros it was mapped with.
    pub used: BTreeMap<u32, u64>,
    /// The last `alloc` returned a range outside `used` (it needs no zeroing).
    pub last_fresh: bool,
}

impl Vm {
    pub fn new() -> Self {
        Vm { regions: BTreeMap::new(), min_addr: 0x0001_0000, max_addr: 0x7FFE_0000, high_max: 0, used: BTreeMap::new(), last_fresh: false }
    }

    /// Whether any of [start, end) was handed out before.
    fn used_before(&self, start: u32, end: u64) -> bool {
        if self.used.range(..=start).next_back().is_some_and(|(_, &e)| e > start as u64) {
            return true;
        }
        self.used.range(start..).next().is_some_and(|(&s, _)| (s as u64) < end)
    }

    fn mark_used(&mut self, start: u32, end: u64) {
        let (mut s, mut e) = (start, end);
        if let Some((&ps, &pe)) = self.used.range(..=s).next_back() {
            if pe >= s as u64 {
                s = ps;
                e = e.max(pe);
            }
        }
        let inside: Vec<u32> = self.used.range(s..).take_while(|kv| *kv.0 as u64 <= e).map(|kv| *kv.0).collect();
        for k in inside {
            e = e.max(self.used.remove(&k).unwrap());
        }
        self.used.insert(s, e);
    }

    fn overlaps(&self, base: u32, size: u32) -> bool {
        let end = base.saturating_add(size);
        if let Some((&b, r)) = self.regions.range(..end).next_back() {
            if b + r.size > base {
                return true;
            }
        }
        false
    }

    /// Finds a free range of `size` bytes aligned to the allocation granularity.
    pub fn find_free(&self, size: u32, top_down: bool, min: u32) -> Option<u32> {
        self.find_free_below(size, top_down, min, self.max_addr).or_else(|| {
            // (from the bottom up there too: memory grows to what is used)
            (self.high_max != 0).then(|| self.find_free_below(size, false, min.max(HIGH_BASE), self.high_max)).flatten()
        })
    }

    fn find_free_below(&self, size: u32, top_down: bool, min: u32, max: u32) -> Option<u32> {
        let size = size.div_ceil(PAGE) * PAGE;
        if top_down {
            let mut end = max & !(GRANULARITY - 1);
            loop {
                let base = end.checked_sub(size)? & !(GRANULARITY - 1);
                if base < min.max(self.min_addr) {
                    return None;
                }
                if !self.overlaps(base, size) {
                    return Some(base);
                }
                // skip below the region that overlaps
                let (&b, _) = self.regions.range(..base + size).next_back()?;
                end = b;
            }
        } else {
            let mut base = min.max(self.min_addr).div_ceil(GRANULARITY) * GRANULARITY;
            loop {
                if base.saturating_add(size) > max {
                    return None;
                }
                match self.regions.range(..base + size).next_back() {
                    Some((&b, r)) if b + r.size > base => base = (b + r.size).div_ceil(GRANULARITY) * GRANULARITY,
                    _ => return Some(base),
                }
            }
        }
    }

    /// Reserves (and optionally commits) a range. `base == 0` lets the manager choose.
    pub fn alloc(&mut self, base: u32, size: u32, commit: bool, protect: u32, top_down: bool, tag: &'static str) -> Option<u32> {
        if size == 0 {
            return None;
        }
        let size = size.div_ceil(PAGE) * PAGE;
        let base = if base == 0 {
            self.find_free(size, top_down, 0)?
        } else {
            let b = base & !(GRANULARITY - 1);
            // (a 64-bit DLL's preferred base above 4 GB can land past the guest's memory)
            let top = if b >= HIGH_BASE && self.high_max != 0 { self.high_max } else { self.max_addr };
            if self.overlaps(b, size) || b < self.min_addr || b as u64 + size as u64 > top as u64 {
                return None;
            }
            b
        };
        self.regions.insert(base, Region { size, alloc_base: base, committed: commit, protect, tag });
        let end = base as u64 + size as u64;
        self.last_fresh = !self.used_before(base, end);
        self.mark_used(base, end);
        Some(base)
    }

    /// Commits pages inside an existing reservation (splitting regions as needed).
    pub fn commit(&mut self, addr: u32, size: u32, protect: u32) -> bool {
        let start = addr & !(PAGE - 1);
        let end = (addr + size).div_ceil(PAGE) * PAGE;
        let Some((&rb, r)) = self.regions.range(..=start).next_back() else { return false };
        if rb + r.size <= start {
            return false;
        }
        // within one reservation, whose earlier commits may have split it into pieces
        // (Delphi 7's memory manager commits across them)
        let ab = r.alloc_base;
        let mut a = rb + r.size;
        while a < end {
            match self.regions.get(&a) {
                Some(n) if n.alloc_base == ab => a += n.size,
                _ => return false,
            }
        }
        self.split(start);
        self.split(end);
        let mut a = start;
        while a < end {
            let r = self.regions.get_mut(&a).unwrap();
            r.committed = true;
            r.protect = protect;
            a += r.size;
        }
        true
    }

    fn split(&mut self, at: u32) {
        if let Some((&rb, r)) = self.regions.range(..at).next_back() {
            if rb < at && at < rb + r.size {
                let mut r2 = r.clone();
                let first = at - rb;
                r2.size = r.size - first;
                self.regions.get_mut(&rb).unwrap().size = first;
                self.regions.insert(at, r2);
            }
        }
    }

    pub fn release(&mut self, base: u32) -> bool {
        let Some(r) = self.regions.get(&base) else { return false };
        let ab = r.alloc_base;
        let keys: Vec<u32> = self.regions.iter().filter(|(_, r)| r.alloc_base == ab).map(|(&k, _)| k).collect();
        for k in keys {
            self.regions.remove(&k);
        }
        true
    }

    pub fn decommit(&mut self, addr: u32, size: u32) -> bool {
        let start = addr & !(PAGE - 1);
        let end = (addr + size).div_ceil(PAGE) * PAGE;
        self.split(start);
        self.split(end);
        let mut a = start;
        while a < end {
            match self.regions.get_mut(&a) {
                Some(r) => {
                    r.committed = false;
                    a += r.size;
                }
                None => return false,
            }
        }
        true
    }

    pub fn protect(&mut self, addr: u32, size: u32, protect: u32) -> Option<u32> {
        let start = addr & !(PAGE - 1);
        let end = (addr + size).div_ceil(PAGE) * PAGE;
        let old = self.regions.range(..=start).next_back().map(|(_, r)| r.protect)?;
        self.split(start);
        self.split(end);
        let mut a = start;
        while a < end {
            let r = self.regions.get_mut(&a)?;
            r.protect = protect;
            a += r.size;
        }
        Some(old)
    }

    /// (base, size, state, protect, alloc_base) for VirtualQuery.
    pub fn query(&self, addr: u32) -> (u32, u32, u32, u32, u32) {
        let page = addr & !(PAGE - 1);
        if let Some((&b, r)) = self.regions.range(..=page).next_back() {
            if b + r.size > page {
                let state = if r.committed { MEM_COMMIT } else { MEM_RESERVE };
                return (b, r.size, state, if r.committed { r.protect } else { 0 }, r.alloc_base);
            }
        }
        // free: extends to the next region
        let top = if page >= HIGH_BASE && self.high_max != 0 { self.high_max } else { self.max_addr };
        let next = self.regions.range(page..).next().map(|(&b, _)| b).unwrap_or(top);
        (page, next - page, MEM_FREE, PAGE_NOACCESS, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_ranges() {
        let mut vm = Vm::new();
        let a = vm.alloc(0, 0x3000, true, PAGE_READWRITE, false, "t").unwrap();
        assert!(vm.last_fresh);
        assert!(vm.release(a));
        // the same range again was someone's before
        let b = vm.alloc(a, 0x1000, true, PAGE_READWRITE, false, "t").unwrap();
        assert_eq!(b, a);
        assert!(!vm.last_fresh);
        // beyond everything handed out so far
        let c = vm.alloc(0x2000_0000, 0x1000, true, PAGE_READWRITE, false, "t").unwrap();
        assert!(vm.last_fresh);
        // overlapping only the end of an earlier range
        assert!(vm.release(c));
        assert!(vm.alloc(0x1FFF_0000, 0x11000, true, PAGE_READWRITE, false, "t").is_some());
        assert!(!vm.last_fresh);
        assert_eq!(vm.used.len(), 2);
    }

    #[test]
    fn commit_across_pieces() {
        let mut vm = Vm::new();
        let a = vm.alloc(0, 0x10_0000, false, PAGE_READWRITE, false, "t").unwrap();
        assert!(vm.commit(a + 0xc000, 0x4000, PAGE_READWRITE));
        assert!(vm.decommit(a + 0xc000, 0x4000));
        // over the decommitted piece and the reserved rest after it
        assert!(vm.commit(a + 0xc000, 0xc000, PAGE_READWRITE));
        assert_eq!(vm.query(a + 0x14000).2, MEM_COMMIT);
        // not past the reservation
        assert!(!vm.commit(a + 0xf_c000, 0x8000, PAGE_READWRITE));
        let b = vm.alloc(a + 0x10_0000, 0x1_0000, false, PAGE_READWRITE, false, "t").unwrap();
        assert!(!vm.commit(b - 0x1000, 0x2000, PAGE_READWRITE));
    }

    #[test]
    fn alloc_commit_query() {
        let mut vm = Vm::new();
        let a = vm.alloc(0, 0x3000, false, PAGE_READWRITE, false, "t").unwrap();
        assert_eq!(a % GRANULARITY, 0);
        assert!(vm.commit(a + 0x1000, 0x1000, PAGE_READWRITE));
        let (b, size, state, _, _) = vm.query(a + 0x1800);
        assert_eq!((b, size, state), (a + 0x1000, 0x1000, MEM_COMMIT));
        let (_, _, state, _, _) = vm.query(a);
        assert_eq!(state, MEM_RESERVE);
        let b2 = vm.alloc(0, 0x1000, true, PAGE_READWRITE, false, "t").unwrap();
        assert!(b2 >= a + 0x3000);
        assert!(vm.release(a));
        assert_eq!(vm.query(a).2, MEM_FREE);
    }
}
