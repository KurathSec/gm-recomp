//! Process-wide emulated state: address space, modules, handles, heaps, threads'
//! TEBs, the virtual clock, files, windows and everything the HLE layer keeps.

use crate::hle::{ApiId, Registry};
use crate::loader::LoadedModule;
use crate::mem::{Guest, PAGE_READWRITE, Vm};
use std::collections::{BTreeMap, HashMap, VecDeque};

pub const STUB_BASE: u32 = 0x7F00_0000;
pub const STUB_STRIDE: u32 = 16;
/// Return address pushed by the host when it calls guest code; a `ret` to it ends the call.
pub const RET_TO_HOST: u32 = 0x7FFF_0000;
pub const TEB_BASE: u32 = 0x7FFD_E000;
pub const PEB_BASE: u32 = 0x7FFD_F000;
pub const STACK_BASE: u32 = 0x0003_0000;
pub const STACK_SIZE: u32 = 0x0010_0000;

#[derive(Debug, Clone)]
pub enum Handle {
    File(FileHandle),
    Thread { id: u32 },
    Event { manual: bool, signaled: bool },
    Mutex,
    Semaphore,
    Heap(u32),
    Module(u32),
    FindFile { entries: Vec<FindEntry>, pos: usize },
    Registry(String),
    /// CreateFileMapping: backing memory is allocated on the first MapViewOfFile.
    Mapping { size: u32, addr: u32, file: u32 },
    /// FindResource: the resource bytes inside the mapped image.
    Resource { addr: u32, size: u32 },
    Window(u32),
    Gdi(GdiObject),
    ImageList { cx: i32, cy: i32, count: i32, flags: u32, bk: u32 },
    Timer,
    Dummy(&'static str),
}

#[derive(Debug, Clone)]
pub struct FindEntry {
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
}

#[derive(Debug, Clone)]
pub struct FileHandle {
    pub path: String,
    /// Path in the VFS.
    pub key: String,
    pub pos: u64,
    pub writable: bool,
}

#[derive(Debug, Clone)]
pub enum GdiObject {
    Dc { hwnd: u32, font: u32, brush: u32, pen: u32, bitmap: u32, text_color: u32, bk_color: u32, bk_mode: u32 },
    Font { height: i32, weight: i32, face: String },
    Brush { color: u32 },
    Pen { color: u32 },
    Bitmap { w: i32, h: i32, bpp: u32, bits: u32, pitch: u32 },
    Region,
    Palette,
    Icon,
    Cursor,
}

#[derive(Debug, Clone)]
pub struct Window {
    pub class: String,
    pub wndproc: u32,
    pub parent: u32,
    pub style: u32,
    pub exstyle: u32,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub visible: bool,
    pub title: String,
    pub userdata: u32,
    pub id: u32,
    pub props: HashMap<String, u32>,
    pub longs: HashMap<i32, u32>,
    pub timers: HashMap<u32, (u32, u64)>,
    pub children: Vec<u32>,
    pub invalid: bool,
    pub enabled: bool,
    /// Edit-control selection (start, end) in characters of `title`.
    pub sel: (u32, u32),
}

#[derive(Debug, Clone, Default)]
pub struct WndClass {
    pub style: u32,
    pub wndproc: u32,
    pub cls_extra: i32,
    pub wnd_extra: i32,
    pub instance: u32,
    pub icon: u32,
    pub cursor: u32,
    pub background: u32,
    pub menu_name: String,
    pub atom: u16,
}

#[derive(Debug, Clone, Copy)]
pub struct Msg {
    pub hwnd: u32,
    pub msg: u32,
    pub wparam: u32,
    pub lparam: u32,
    pub time: u32,
    pub pt: (i32, i32),
}

/// A first-fit heap (HeapAlloc / GlobalAlloc / LocalAlloc) over one or more regions;
/// it grows by adding regions under the same handle.
#[derive(Debug, Clone)]
pub struct Heap {
    /// The handle: base of the first region.
    pub base: u32,
    /// (base, size) of each region.
    pub regions: Vec<(u32, u32)>,
    /// addr -> size of live blocks
    pub blocks: BTreeMap<u32, u32>,
}

impl Heap {
    pub fn alloc(&mut self, size: u32) -> Option<u32> {
        let size = size.max(1).div_ceil(16) * 16;
        for &(rb, rs) in &self.regions {
            let end = rb + rs;
            let mut cursor = rb + 16;
            for (&b, &s) in self.blocks.range(rb..end) {
                if b - cursor >= size {
                    break;
                }
                cursor = b + s;
            }
            if cursor.checked_add(size).is_some_and(|e| e <= end) {
                self.blocks.insert(cursor, size);
                return Some(cursor);
            }
        }
        None
    }

    pub fn free(&mut self, addr: u32) -> bool {
        self.blocks.remove(&addr).is_some()
    }

    pub fn size_of(&self, addr: u32) -> Option<u32> {
        self.blocks.get(&addr).copied()
    }

    pub fn total(&self) -> u32 {
        self.regions.iter().map(|r| r.1).sum()
    }
}

#[derive(Debug, Default)]
pub struct Thread {
    pub id: u32,
    pub teb: u32,
    pub last_error: u32,
    pub stack_base: u32,
    pub stack_limit: u32,
}

/// A DirectSound buffer: PCM data in guest memory plus playback state.
#[derive(Debug, Clone)]
pub struct SoundBuffer {
    pub primary: bool,
    pub flags: u32,
    pub data: u32,
    pub size: u32,
    pub format_tag: u16,
    pub channels: u16,
    pub rate: u32,
    /// Playback frequency (SetFrequency), in samples per second.
    pub freq: u32,
    pub block_align: u16,
    pub bits: u16,
    /// Hundredths of a decibel (0 = full, -10000 = silent).
    pub volume: i32,
    pub pan: i32,
    pub play: u32,
    pub write: u32,
    pub playing: bool,
    pub looping: bool,
    /// Virtual time the cursor was last advanced to.
    pub last_us: u64,
    /// Play position in frames (fractional).
    pub cursor: f64,
    /// (offset or DSBPN_OFFSETSTOP, event handle)
    pub notify: Vec<(u32, u32)>,
    pub fired: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct D3dObject {
    pub kind: &'static str,
    pub refcount: u32,
    pub data: HashMap<&'static str, u32>,
}

pub struct Process {
    pub vm: Vm,
    pub registry: Registry,
    pub modules: Vec<LoadedModule>,
    /// Fake bases for HLE DLLs that have no image.
    pub fake_modules: HashMap<String, u32>,
    pub handles: HashMap<u32, Handle>,
    pub next_handle: u32,
    pub heaps: HashMap<u32, Heap>,
    pub process_heap: u32,
    pub threads: Vec<Thread>,
    pub main_thread: u32,
    pub cmdline: String,
    pub exe_path: String,
    pub cwd: String,
    /// The guest's file system (`C:\\game` and everything below it).
    pub fs: Box<dyn crate::vfs::Vfs>,
    /// Host wall-clock time at process start (Unix seconds); dates follow it plus the
    /// virtual clock.
    pub epoch_unix: u64,
    pub env: Vec<(String, String)>,
    /// Virtual clock in microseconds since process start.
    pub clock_us: u64,
    /// Consecutive clock reads without other API work (see `Ctx::poll_clock`).
    pub idle_streak: u32,
    pub exit_code: Option<u32>,
    pub tls_used: Vec<bool>,
    pub atoms: HashMap<String, u16>,
    pub next_atom: u16,
    pub classes: HashMap<String, WndClass>,
    pub windows: HashMap<u32, Window>,
    pub next_hwnd: u32,
    pub messages: VecDeque<Msg>,
    pub focus: u32,
    pub active: u32,
    pub capture: u32,
    pub next_gdi: u32,
    pub stock_objects: HashMap<u32, u32>,
    pub registry_values: HashMap<String, Vec<u8>>,
    pub frames_presented: u64,
    pub frame_limit: u64,
    pub d3d_objects: HashMap<u32, D3dObject>,
    /// Direct3D device state and the host renderer it drives.
    pub d3d: crate::hle::gpu::DeviceState,
    pub gpu: Box<dyn crate::hle::gpu::Gpu>,
    pub sound_buffers: BTreeMap<u32, SoundBuffer>,
    /// The sound mix and where finished samples go.
    pub mixer: crate::hle::audio::Mixer,
    pub audio: Box<dyn crate::hle::audio::AudioSink>,
    /// MCI devices: id -> (mode, element), and devices opened by name/alias.
    pub mci: BTreeMap<u32, (u32, String)>,
    pub mci_named: BTreeMap<String, u32>,
    pub next_mci: u32,
    /// COM interface name -> vtable address in guest memory.
    pub com_vtables: HashMap<&'static str, u32>,
    /// Interface pointer -> (object base, interface index) for HLE COM objects.
    pub com_ifaces: HashMap<u32, (u32, u32)>,
    pub com_classes: HashMap<u32, &'static [&'static crate::hle::com::Interface]>,
    pub draw_calls: u64,
    /// msvcrt state: errno cell, FILE* -> file handle, rand() seed.
    pub crt_errno: u32,
    pub crt_files: HashMap<u32, u32>,
    pub crt_rand: u32,
    pub textures_created: u64,
    pub next_d3d_handle: u32,
    pub trace: bool,
    pub api_calls: u64,
    pub unimplemented: Vec<String>,
    pub key_state: [u8; 256],
    /// Last mouse position (client coordinates of the game window) and buttons.
    pub mouse: (i32, i32),
    pub mouse_buttons: u32,
    pub log: Vec<String>,
}

impl Process {
    pub fn new(mut fs: Box<dyn crate::vfs::Vfs>, exe_path: &str, cmdline: &str) -> Self {
        crate::vfs::add_standard_dirs(fs.as_mut());
        Self::with_registry(Registry::new(), fs, exe_path, cmdline)
    }

    /// A placeholder with no APIs: stands in for the real process while it is lent out.
    pub fn empty() -> Self {
        Self::with_registry(Registry::default(), Box::new(crate::vfs::MemFs::new()), "", "")
    }

    pub fn with_registry(registry: Registry, fs: Box<dyn crate::vfs::Vfs>, exe_path: &str, cmdline: &str) -> Self {
        let mut vm = Vm::new();
        vm.alloc(STUB_BASE, 0x0010_0000, true, PAGE_READWRITE, false, "hle stubs");
        vm.alloc(RET_TO_HOST, 0x1000, true, PAGE_READWRITE, false, "host return");
        // The page below each TEB holds the thread's x87 state (fpu::block).
        vm.alloc(TEB_BASE - 0x1000, 0x1000, true, PAGE_READWRITE, false, "thread state");
        vm.alloc(TEB_BASE, 0x1000, true, PAGE_READWRITE, false, "teb");
        vm.alloc(PEB_BASE, 0x1000, true, PAGE_READWRITE, false, "peb");
        vm.alloc(STACK_BASE, STACK_SIZE, true, PAGE_READWRITE, false, "stack");
        let cwd = exe_path.rsplit_once('\\').map(|(d, _)| d.to_string()).unwrap_or_else(|| "C:\\game".into());
        Process {
            vm,
            registry,
            modules: Vec::new(),
            fake_modules: HashMap::new(),
            handles: HashMap::new(),
            next_handle: 0x100,
            heaps: HashMap::new(),
            process_heap: 0,
            threads: Vec::new(),
            main_thread: 0,
            cmdline: cmdline.to_string(),
            exe_path: exe_path.to_string(),
            cwd,
            fs,
            epoch_unix: 1_767_225_600,
            env: vec![
                ("PATH".into(), "C:\\Windows\\system32;C:\\Windows".into()),
                ("SystemRoot".into(), "C:\\Windows".into()),
                ("TEMP".into(), "C:\\Temp".into()),
                ("TMP".into(), "C:\\Temp".into()),
                ("USERPROFILE".into(), "C:\\Users\\player".into()),
                ("APPDATA".into(), "C:\\Users\\player\\AppData\\Roaming".into()),
                ("LOCALAPPDATA".into(), "C:\\Users\\player\\AppData\\Local".into()),
                ("USERNAME".into(), "player".into()),
                ("COMPUTERNAME".into(), "GMRECOMP".into()),
            ],
            clock_us: 0,
            idle_streak: 0,
            exit_code: None,
            tls_used: vec![false; 1088],
            atoms: HashMap::new(),
            next_atom: 0xC000,
            classes: HashMap::new(),
            windows: HashMap::new(),
            next_hwnd: 0x0001_0010,
            messages: VecDeque::new(),
            focus: 0,
            active: 0,
            capture: 0,
            next_gdi: 0x0100_0000,
            stock_objects: HashMap::new(),
            registry_values: HashMap::new(),
            frames_presented: 0,
            frame_limit: u64::MAX,
            d3d_objects: HashMap::new(),
            d3d: Default::default(),
            gpu: Box::new(crate::hle::gpu::NullGpu),
            sound_buffers: BTreeMap::new(),
            mixer: Default::default(),
            audio: Box::new(crate::hle::audio::NullSink),
            mci: BTreeMap::new(),
            mci_named: BTreeMap::new(),
            next_mci: 0,
            com_vtables: HashMap::new(),
            com_ifaces: HashMap::new(),
            com_classes: HashMap::new(),
            draw_calls: 0,
            crt_errno: 0,
            crt_files: HashMap::new(),
            crt_rand: 1,
            textures_created: 0,
            next_d3d_handle: 0x1000,
            trace: false,
            api_calls: 0,
            unimplemented: Vec::new(),
            key_state: [0; 256],
            mouse: (0, 0),
            mouse_buttons: 0,
            log: Vec::new(),
        }
    }

    pub fn new_handle(&mut self, h: Handle) -> u32 {
        let id = self.next_handle;
        self.next_handle += 4;
        self.handles.insert(id, h);
        id
    }

    pub fn new_gdi(&mut self, o: GdiObject) -> u32 {
        let id = self.next_gdi;
        self.next_gdi += 4;
        self.handles.insert(id, Handle::Gdi(o));
        id
    }

    pub fn module_by_name(&self, name: &str) -> Option<&LoadedModule> {
        let n = name.to_lowercase();
        let stem = n.trim_end_matches(".exe").trim_end_matches(".dll");
        self.modules.iter().find(|m| {
            let mn = m.name.trim_end_matches(".exe").trim_end_matches(".dll");
            mn == stem
        })
    }

    pub fn module_by_base(&self, base: u32) -> Option<&LoadedModule> {
        self.modules.iter().find(|m| m.base == base)
    }

    pub fn module_containing(&self, va: u32) -> Option<&LoadedModule> {
        self.modules.iter().find(|m| va >= m.base && va < m.base + m.size)
    }

    pub fn stub_addr(id: ApiId) -> u32 {
        STUB_BASE + id.0 * STUB_STRIDE
    }

    pub fn stub_id(addr: u32) -> Option<ApiId> {
        if (STUB_BASE..STUB_BASE + 0x0010_0000).contains(&addr) && (addr - STUB_BASE) % STUB_STRIDE == 0 {
            Some(ApiId((addr - STUB_BASE) / STUB_STRIDE))
        } else {
            None
        }
    }

    /// Fake module handle for DLLs implemented by HLE.
    pub fn fake_module(&mut self, dll: &str) -> u32 {
        let key = dll.to_lowercase();
        let key = if key.ends_with(".dll") || key.ends_with(".drv") { key } else { format!("{key}.dll") };
        if let Some(&b) = self.fake_modules.get(&key) {
            return b;
        }
        let base = 0x7C00_0000 + self.fake_modules.len() as u32 * 0x0010_0000;
        self.vm.alloc(base, 0x1000, true, PAGE_READWRITE, false, "fake module");
        self.fake_modules.insert(key, base);
        base
    }

    pub fn fake_module_name(&self, base: u32) -> Option<&str> {
        self.fake_modules.iter().find(|(_, b)| **b == base).map(|(n, _)| n.as_str())
    }

    /// Path components of a guest path relative to the game directory (C:\game);
    /// other drives and directories map below it too.
    pub fn guest_rel(&self, guest: &str) -> Vec<String> {
        let mut p = guest.replace('/', "\\");
        if !p.contains(':') && !p.starts_with("\\\\") {
            p = format!("{}\\{}", self.cwd.trim_end_matches('\\'), p.trim_start_matches('\\'));
        }
        let lower = p.to_lowercase();
        let root = "c:\\game";
        let rel = if lower.starts_with(root) { &p[root.len()..] } else { &p[..] };
        let mut out: Vec<String> = Vec::new();
        for comp in rel.split('\\').filter(|c| !c.is_empty() && *c != ".") {
            if comp == ".." {
                out.pop();
            } else if comp.ends_with(':') {
                out.push(comp.trim_end_matches(':').to_string());
            } else {
                out.push(comp.to_string());
            }
        }
        out
    }

    /// The VFS path of a guest path.
    pub fn vpath(&self, guest: &str) -> String {
        self.guest_rel(guest).join("\\")
    }

    pub fn now_ms(&self) -> u32 {
        (self.clock_us / 1000) as u32
    }

    pub fn log(&mut self, s: String) {
        if self.trace {
            eprintln!("{s}");
        }
        if self.log.len() < 100_000 {
            self.log.push(s);
        }
    }

    /// Lays out the main thread: TEB, PEB, stack, process heap, command line in guest memory.
    pub fn setup_main_thread(&mut self, mem: &mut dyn Guest, image_base: u32) -> Result<(u32, u32), String> {
        for (base, size) in [(TEB_BASE - 0x1000, 0x1000u32), (TEB_BASE, 0x1000), (PEB_BASE, 0x1000), (STACK_BASE, STACK_SIZE), (RET_TO_HOST, 0x1000), (STUB_BASE, 0x0010_0000)] {
            mem.ensure_mapped(base, size).map_err(|e| e.to_string())?;
        }
        let stack_top = STACK_BASE + STACK_SIZE;
        let mut fpu_state = [0u8; fpu::block::SIZE];
        fpu::Fpu::new().to_block(&mut fpu_state);
        mem.write(TEB_BASE - fpu::block::OFFSET, &fpu_state).map_err(|e| e.to_string())?;
        // TEB
        let w = |mem: &mut dyn Guest, off: u32, v: u32| mem.write_u32(TEB_BASE + off, v).map_err(|e| e.to_string());
        w(mem, 0x00, 0xFFFF_FFFF)?; // NtTib.ExceptionList
        w(mem, 0x04, stack_top)?; // StackBase
        w(mem, 0x08, STACK_BASE)?; // StackLimit
        w(mem, 0x18, TEB_BASE)?; // Self
        w(mem, 0x20, 0x1234)?; // ProcessId
        w(mem, 0x24, 0x1238)?; // ThreadId
        w(mem, 0x30, PEB_BASE)?; // PEB
        w(mem, 0x34, 0)?; // LastErrorValue
        // PEB
        let p = |mem: &mut dyn Guest, off: u32, v: u32| mem.write_u32(PEB_BASE + off, v).map_err(|e| e.to_string());
        p(mem, 0x08, image_base)?; // ImageBaseAddress
        p(mem, 0x64, 1)?; // NumberOfProcessors
        p(mem, 0xA4, 5)?; // OSMajorVersion
        p(mem, 0xA8, 1)?; // OSMinorVersion
        p(mem, 0xAC, 2600)?; // OSBuildNumber
        p(mem, 0xB0, 2)?; // OSPlatformId
        // Process parameters with the command line (UNICODE_STRING at 0x40) and image path (0x38)
        let params = PEB_BASE + 0x400;
        p(mem, 0x10, params)?;
        let cmd_w = params + 0x200;
        mem.write_wstr(cmd_w, &self.cmdline).map_err(|e| e.to_string())?;
        let len = (self.cmdline.encode_utf16().count() * 2) as u32;
        mem.write_u16(params + 0x40, len as u16).map_err(|e| e.to_string())?;
        mem.write_u16(params + 0x42, (len + 2) as u16).map_err(|e| e.to_string())?;
        mem.write_u32(params + 0x44, cmd_w).map_err(|e| e.to_string())?;
        let img_w = params + 0x600;
        mem.write_wstr(img_w, &self.exe_path).map_err(|e| e.to_string())?;
        let ilen = (self.exe_path.encode_utf16().count() * 2) as u32;
        mem.write_u16(params + 0x38, ilen as u16).map_err(|e| e.to_string())?;
        mem.write_u16(params + 0x3A, (ilen + 2) as u16).map_err(|e| e.to_string())?;
        mem.write_u32(params + 0x3C, img_w).map_err(|e| e.to_string())?;
        // Process heap
        let heap = self.create_heap(mem, 0x0100_0000)?;
        self.process_heap = heap;
        p(mem, 0x18, heap)?;
        self.threads.push(Thread { id: 0x1238, teb: TEB_BASE, last_error: 0, stack_base: stack_top, stack_limit: STACK_BASE });
        self.main_thread = 0x1238;
        Ok((TEB_BASE, stack_top))
    }

    /// Implicit TLS of statically loaded images: each module with a TLS directory
    /// gets an index and a copy of its template in the TEB's ThreadLocalStoragePointer array.
    pub fn setup_static_tls(&mut self, mem: &mut dyn Guest) -> Result<(), String> {
        let templates: Vec<(u32, (u32, u32, u32))> = self.modules.iter().filter_map(|m| m.tls_template.map(|t| (m.tls_index_va, t))).collect();
        if templates.is_empty() {
            return Ok(());
        }
        let heap = self.process_heap;
        let array = self.heap_alloc(mem, heap, 4 * 64, true);
        for (i, (index_va, (start, end, zero))) in templates.into_iter().enumerate() {
            let len = end.saturating_sub(start);
            let block = self.heap_alloc(mem, heap, len + zero + 16, true);
            let mut buf = vec![0u8; len as usize];
            mem.read(start, &mut buf).map_err(|e| e.to_string())?;
            mem.write(block, &buf).map_err(|e| e.to_string())?;
            mem.write_u32(array + 4 * i as u32, block).map_err(|e| e.to_string())?;
            if index_va != 0 {
                mem.write_u32(index_va, i as u32).map_err(|e| e.to_string())?;
            }
            self.tls_used[i] = true;
        }
        mem.write_u32(TEB_BASE + 0x2C, array).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn create_heap(&mut self, mem: &mut dyn Guest, size: u32) -> Result<u32, String> {
        let base = self.vm.alloc(0, size, true, PAGE_READWRITE, false, "heap").ok_or("no space for heap")?;
        mem.ensure_mapped(base, size).map_err(|e| e.to_string())?;
        let heap = Heap { base, regions: vec![(base, size)], blocks: BTreeMap::new() };
        self.heaps.insert(base, heap);
        Ok(base)
    }

    pub fn heap_alloc(&mut self, mem: &mut dyn Guest, heap: u32, size: u32, zero: bool) -> u32 {
        let heap = if self.heaps.contains_key(&heap) { heap } else { self.process_heap };
        let Some(h) = self.heaps.get_mut(&heap) else { return 0 };
        let a = match h.alloc(size) {
            Some(a) => a,
            None => {
                // Grow: a new region at least as large as the heap so far.
                let grow = (h.total().max(size.saturating_add(0x10000))).div_ceil(0x10_0000) * 0x10_0000;
                let Some(base) = self.vm.alloc(0, grow, true, PAGE_READWRITE, false, "heap") else { return 0 };
                if mem.ensure_mapped(base, grow).is_err() {
                    return 0;
                }
                let h = self.heaps.get_mut(&heap).unwrap();
                h.regions.push((base, grow));
                match h.alloc(size) {
                    Some(a) => a,
                    None => return 0,
                }
            }
        };
        if zero {
            let _ = mem.fill(a, size, 0);
        }
        a
    }

    pub fn heap_free(&mut self, addr: u32) -> bool {
        for h in self.heaps.values_mut() {
            if h.free(addr) {
                return true;
            }
        }
        false
    }

    pub fn heap_size(&self, addr: u32) -> Option<u32> {
        self.heaps.values().find_map(|h| h.size_of(addr))
    }

    pub fn thread_mut(&mut self, id: u32) -> Option<&mut Thread> {
        self.threads.iter_mut().find(|t| t.id == id)
    }

    pub fn current_thread_mut(&mut self) -> &mut Thread {
        let id = self.main_thread;
        self.threads.iter_mut().find(|t| t.id == id).expect("main thread")
    }
}
