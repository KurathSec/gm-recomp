//! Host-agnostic Windows runtime for recompiled guests: address space, PE loader,
//! process state and the HLE API layer. The wasm host (native harness or browser)
//! supplies memory, translation and the dispatcher.

pub mod engine;
pub mod fasthash;
pub mod hle;
pub mod loader;
pub mod mem;
pub mod process;
pub mod state;
pub mod vfs;

pub use hle::{ApiError, ApiId, ApiResult, Ctx, Host, InputEvent, Regs, Registry, Ret};
pub use loader::{ImportResolver, LoadRequest, LoadedModule};
pub use mem::{Fault, Guest, Vm};
pub use process::{Handle, Process, RET_TO_HOST, STUB_BASE};

/// Import resolver that binds everything to HLE stubs, except exports of modules that
/// are already loaded as images (game DLLs).
pub struct StubResolver<'a> {
    pub registry: &'a mut Registry,
    pub modules: &'a [LoadedModule],
}

/// The DLL an API set name stands for (Windows maps them in its loader; Wine 11 has no
/// files for them): the Universal C Runtime's, `api-ms-win-crt-*`, are ucrtbase.dll.
pub fn api_set(dll: &str) -> Option<&'static str> {
    dll.to_lowercase().starts_with("api-ms-win-crt-").then_some("ucrtbase.dll")
}

impl ImportResolver for StubResolver<'_> {
    fn resolve(&mut self, dll: &str, name: &str) -> u32 {
        self.resolve_forwarded(dll, name, 0)
    }
}

impl StubResolver<'_> {
    /// `resolve`, following a loaded module's forwarded exports a few deep.
    fn resolve_forwarded(&mut self, dll: &str, name: &str, depth: u32) -> u32 {
        let dll = api_set(dll).unwrap_or(dll);
        let n = dll.to_lowercase();
        let stem = n.trim_end_matches(".exe").trim_end_matches(".dll");
        let found = self.modules.iter().find(|m| m.name.trim_end_matches(".exe").trim_end_matches(".dll") == stem);
        if let Some(m) = found {
            if let Some(&va) = m.exports.get(name) {
                return va;
            }
            if let Some((to_dll, to_name)) = m.forwards.get(name).cloned().filter(|_| depth < 4) {
                return self.resolve_forwarded(&to_dll, &to_name, depth + 1);
            }
        }
        if let Some(a) = self.registry.data_export(dll, name) {
            return a;
        }
        let id = self.registry.intern_import(dll, name);
        Process::stub_addr(id)
    }
}

/// Outcome of running the guest until it stops.
#[derive(Debug)]
pub enum Stop {
    Exit(u32),
    FrameLimit,
    Unimplemented(String),
    Fault(String),
    Trap(String),
}
