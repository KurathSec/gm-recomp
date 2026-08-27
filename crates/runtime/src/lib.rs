//! Host-agnostic Windows runtime for recompiled guests: address space, PE loader,
//! process state and the HLE API layer. The wasm host (native harness or browser)
//! supplies memory, translation and the dispatcher.

pub mod engine;
pub mod hle;
pub mod loader;
pub mod mem;
pub mod process;
pub mod vfs;

pub use hle::{ApiError, ApiId, ApiResult, Ctx, Host, Regs, Registry, Ret};
pub use loader::{ImportResolver, LoadRequest, LoadedModule};
pub use mem::{Fault, Guest, Vm};
pub use process::{Handle, Process, RET_TO_HOST, STUB_BASE};

/// Import resolver that binds everything to HLE stubs, except exports of modules that
/// are already loaded as images (game DLLs).
pub struct StubResolver<'a> {
    pub registry: &'a mut Registry,
    pub modules: &'a [LoadedModule],
}

impl ImportResolver for StubResolver<'_> {
    fn resolve(&mut self, dll: &str, name: &str) -> u32 {
        let n = dll.to_lowercase();
        let stem = n.trim_end_matches(".exe").trim_end_matches(".dll");
        let found = self.modules.iter().find(|m| m.name.trim_end_matches(".exe").trim_end_matches(".dll") == stem);
        if let Some(m) = found {
            if let Some(&va) = m.exports.get(name) {
                return va;
            }
        }
        let id = self.registry.intern(dll, name);
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
