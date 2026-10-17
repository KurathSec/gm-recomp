//! Process-wide emulated state: address space, modules, handles, heaps, threads'
//! TEBs, the virtual clock, files, windows and everything the HLE layer keeps.

use crate::hle::{ApiId, Registry};
use crate::loader::LoadedModule;
use crate::mem::{Guest, PAGE_READWRITE, Vm};
use std::collections::{BTreeMap, VecDeque};
use crate::fasthash::HashMap;

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
    /// Free space by address (-> size) and by (size, address): best fit, coalesced.
    free_at: BTreeMap<u32, u32>,
    free_by_size: std::collections::BTreeSet<(u32, u32)>,
}

impl Heap {
    pub fn new(base: u32, size: u32) -> Self {
        let mut h = Heap { base, regions: Vec::new(), blocks: BTreeMap::new(), free_at: BTreeMap::new(), free_by_size: Default::default() };
        h.add_region(base, size);
        h
    }

    /// Adds address space (the first 16 bytes stay unused, as a heap header).
    pub fn add_region(&mut self, base: u32, size: u32) {
        self.regions.push((base, size));
        self.insert_free(base + 16, size - 16);
    }

    fn insert_free(&mut self, at: u32, size: u32) {
        self.free_at.insert(at, size);
        self.free_by_size.insert((size, at));
    }

    fn remove_free(&mut self, at: u32) -> Option<u32> {
        let size = self.free_at.remove(&at)?;
        self.free_by_size.remove(&(size, at));
        Some(size)
    }

    /// The smallest free block that fits, lowest address first among equals.
    pub fn alloc(&mut self, size: u32) -> Option<u32> {
        let size = size.max(1).div_ceil(16) * 16;
        let &(fsize, at) = self.free_by_size.range((size, 0)..).next()?;
        self.remove_free(at);
        if fsize > size {
            self.insert_free(at + size, fsize - size);
        }
        self.blocks.insert(at, size);
        Some(at)
    }

    pub fn free(&mut self, addr: u32) -> bool {
        let Some(size) = self.blocks.remove(&addr) else { return false };
        let (mut at, mut size) = (addr, size);
        // merge with the free space right after and right before (within a region)
        let region_end = self.regions.iter().find(|&&(b, s)| addr >= b && addr < b + s).map(|&(b, s)| b + s).unwrap_or(u32::MAX);
        if at + size < region_end {
            if let Some(next) = self.remove_free(at + size) {
                size += next;
            }
        }
        if let Some((&prev, &psize)) = self.free_at.range(..at).next_back() {
            if prev + psize == at && self.regions.iter().any(|&(b, s)| prev >= b && at < b + s) {
                self.remove_free(prev);
                at = prev;
                size += psize;
            }
        }
        self.insert_free(at, size);
        true
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
    /// The thread's handle (0 for the main thread).
    pub handle: u32,
    pub state: ThreadState,
    /// Registers while another thread runs (secondary threads only).
    pub ctx: Option<Box<ThreadCtx>>,
    /// EAX when the thread resumes (the result of the wait it blocked in).
    pub resume_eax: Option<u32>,
}

/// What a guest thread is doing. Threads run one at a time: secondary threads are
/// scheduled from the main thread's blocking points (see `hle::threads`).
#[derive(Debug, Clone, PartialEq, Default)]
pub enum ThreadState {
    #[default]
    Running,
    Ready,
    /// Until this virtual time.
    Sleeping(u64),
    Waiting { handles: Vec<u32>, all: bool, until: Option<u64> },
    /// For a critical section another thread owns.
    CritSec(u32),
    Suspended(u32),
    Exited(u32),
}

/// A thread's registers while it is not running.
#[derive(Debug, Clone)]
pub struct ThreadCtx {
    pub regs: crate::hle::Regs,
    /// cf, pf, af, zf, sf, of, df
    pub flags: [u32; 7],
    pub xmm: [u128; 8],
    pub mxcsr: u32,
}

/// Where a thread's start routine returns to.
pub const THREAD_EXIT: u32 = RET_TO_HOST + 0xFFC;

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
    /// Index in `threads` of the running thread (0: main).
    pub current: usize,
    /// Nested guest calls in progress (host frames), and the depth a secondary
    /// thread's slice started at: it can only block (yield) at that depth.
    pub nest: u32,
    pub slice_nest: Option<u32>,
    /// The scheduler is running secondary threads.
    pub in_scheduler: bool,
    /// Critical sections held (address -> owning thread id), and the ones handed to
    /// threads that woke up waiting for them.
    pub cs_owners: HashMap<u32, u32>,
    pub cs_pending: Vec<(u32, u32)>,
    pub cmdline: String,
    pub exe_path: String,
    pub cwd: String,
    /// The guest's file system (`C:\\game` and everything below it).
    pub fs: Box<dyn crate::vfs::Vfs>,
    /// Receives the game's file changes (not the temp directory) for persistence.
    pub saves: Box<dyn crate::vfs::SaveSink>,
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
    /// Atom -> its name as first added (lookups ignore case; GetAtomName keeps it, and
    /// MinGW's runtime encodes a pointer in the letter case of an atom name).
    pub atom_names: HashMap<u16, String>,
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
    /// Direct3D 9 device state, and the state blocks' snapshots.
    pub d3d9: Box<crate::hle::d3d9::State9>,
    pub d3d9_blocks: HashMap<u32, Box<crate::hle::d3d9::State9>>,
    pub gpu: Box<dyn crate::hle::gpu::Gpu>,
    pub sound_buffers: BTreeMap<u32, SoundBuffer>,
    /// The sound mix and where finished samples go.
    pub mixer: crate::hle::audio::Mixer,
    /// DirectMusic: loaded sounds, playing segments, master volume (mB), default path,
    /// loader search directories.
    pub dm_sounds: Vec<crate::hle::dmusic::DmSound>,
    pub dm_voices: Vec<crate::hle::dmusic::DmVoice>,
    pub dm_master_volume: i32,
    pub dm_default_path: u32,
    pub dm_search_dirs: HashMap<u32, String>,
    pub audio: Box<dyn crate::hle::audio::AudioSink>,
    /// MCI devices: id -> (mode, element), and devices opened by name/alias.
    pub mci: crate::hle::mci::Devices,
    /// Decoded MCI media by file, kept for reopening.
    pub mci_media: HashMap<String, std::rc::Rc<crate::hle::mci::Media>>,
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
    /// setlocale's "C" string.
    pub crt_locale: u32,
    /// getenv's copies of the variables it returned (they stay valid).
    pub crt_getenv: HashMap<String, u32>,
    /// AllocateLocallyUniqueId's counter.
    pub next_luid: u64,
    /// Characters written to stdout one at a time (fputc), until the line ends.
    pub crt_line: Vec<u8>,
    /// Each module's resource directory, parsed once (by module base).
    pub resources: HashMap<u32, std::rc::Rc<Vec<pe::Resource>>>,
    /// Keypad keys held (by VK_NUMPAD0.., VK_DECIMAL last) and the key each was sent as.
    pub numpad_sent: [u8; 11],
    /// Shift keys (bit 0 left, bit 1 right) held but released for the keypad: Windows
    /// lets go of Shift while a keypad key is down with NumLock on (so it arrives as
    /// an unshifted navigation key) and presses it again afterwards.
    pub numpad_shift: u8,
    /// The millisecond of the clock the host was last asked to pace to.
    pub paced_ms: u64,
    /// Textures the guest finished writing that no draw has uploaded yet, with the
    /// Direct3D version (8, 9) that owns them: uploaded while the game waits.
    pub pending_uploads: Vec<(u32, u8)>,
    /// The MIDI SoundFont, once the host supplied it (or failed to).
    pub soundfont: Option<std::sync::Arc<rustysynth::SoundFont>>,
    pub soundfont_failed: bool,
    /// Game controllers 0..3.
    pub pads: [crate::hle::PadState; 4],
    /// Waitable timers: (handle, virtual time they become signaled).
    pub timers_due: Vec<(u32, u64)>,
    pub textures_created: u64,
    pub next_d3d_handle: u32,
    pub trace: bool,
    pub api_calls: u64,
    pub unimplemented: Vec<String>,
    pub key_state: [u8; 256],
    /// DirectInput keyboard state, by DIK code (scan code, 0x80 for extended keys).
    pub dik: [u8; 256],
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
        vm.alloc(crate::hle::DATA_EXPORTS, crate::hle::DATA_EXPORTS_SIZE, true, PAGE_READWRITE, false, "exported variables");
        vm.alloc(RET_TO_HOST, 0x1000, true, PAGE_READWRITE, false, "host return");
        // (The debugging counters after it, abi::COUNTERS, are left out: reserving them
        // moves later top-down allocations and so every state hash. With GMR_COUNT an
        // allocation landing there gets them as its initial contents.)
        vm.alloc(recomp::abi::RESOLVE_CACHE, recomp::abi::RESOLVE_CACHE_SIZE, true, PAGE_READWRITE, false, "resolve cache");
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
            fake_modules: HashMap::default(),
            handles: HashMap::default(),
            next_handle: 0x100,
            heaps: HashMap::default(),
            process_heap: 0,
            threads: Vec::new(),
            main_thread: 0,
            current: 0,
            nest: 0,
            slice_nest: None,
            in_scheduler: false,
            cs_owners: HashMap::default(),
            cs_pending: Vec::new(),
            cmdline: cmdline.to_string(),
            exe_path: exe_path.to_string(),
            cwd,
            fs,
            saves: Box::new(crate::vfs::NullSaves),
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
            atoms: HashMap::default(),
            atom_names: HashMap::default(),
            next_atom: 0xC000,
            classes: HashMap::default(),
            windows: HashMap::default(),
            next_hwnd: 0x0001_0010,
            messages: VecDeque::new(),
            focus: 0,
            active: 0,
            capture: 0,
            next_gdi: 0x0100_0000,
            stock_objects: HashMap::default(),
            registry_values: HashMap::default(),
            frames_presented: 0,
            frame_limit: u64::MAX,
            d3d_objects: HashMap::default(),
            d3d: Default::default(),
            d3d9: Default::default(),
            d3d9_blocks: HashMap::default(),
            gpu: Box::new(crate::hle::gpu::NullGpu),
            sound_buffers: BTreeMap::new(),
            mixer: Default::default(),
            dm_sounds: Vec::new(),
            dm_voices: Vec::new(),
            dm_master_volume: 0,
            dm_default_path: 0,
            dm_search_dirs: HashMap::default(),
            audio: Box::new(crate::hle::audio::NullSink),
            mci: BTreeMap::new(),
            mci_media: HashMap::default(),
            next_mci: 0,
            com_vtables: HashMap::default(),
            com_ifaces: HashMap::default(),
            com_classes: HashMap::default(),
            draw_calls: 0,
            crt_errno: 0,
            crt_files: HashMap::default(),
            crt_rand: 1,
            crt_locale: 0,
            crt_getenv: HashMap::default(),
            next_luid: 0,
            crt_line: Vec::new(),
            resources: HashMap::default(),
            numpad_sent: [0; 11],
            numpad_shift: 0,
            paced_ms: 0,
            pending_uploads: Vec::new(),
            timers_due: Vec::new(),
            pads: Default::default(),
            soundfont: None,
            soundfont_failed: false,
            textures_created: 0,
            next_d3d_handle: 0x1000,
            trace: false,
            api_calls: 0,
            unimplemented: Vec::new(),
            key_state: [0; 256],
            dik: [0; 256],
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

    /// Hands the files changed since the last call to the save sink (temp files stay).
    pub fn flush_saves(&mut self) {
        let changes: Vec<_> = self.fs.take_changes().into_iter().filter(|(p, _)| !p.to_ascii_lowercase().starts_with("c\\temp\\")).collect();
        if !changes.is_empty() {
            self.saves.save(&changes);
        }
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
        self.threads.push(Thread { id: 0x1238, teb: TEB_BASE, last_error: 0, stack_base: stack_top, stack_limit: STACK_BASE, handle: 0, state: ThreadState::Running, ctx: None, resume_eax: None });
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
        let heap = Heap::new(base, size);
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
                h.add_region(base, grow);
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
        let i = self.current;
        &mut self.threads[i]
    }

    pub fn current_thread_id(&self) -> u32 {
        self.threads.get(self.current).map(|t| t.id).unwrap_or(self.main_thread)
    }
}

