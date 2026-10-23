//! The in-process native x86-64 reference (tests/cpu/refrun64.c, built by build.rs): a
//! case's code runs directly on the host CPU, so no Wine is needed. One fixed region is
//! mapped once; each case is copied in, run through the trampoline and copied back.

use crate::proto::{CaseIn64, CaseOut64};
use anyhow::{Result, bail};
use std::sync::Once;

unsafe extern "C" {
    fn refrun64_init() -> i32;
    fn refrun64_run(inp: *const CaseIn64, out: *mut CaseOut64);
}

static INIT: Once = Once::new();
static mut INIT_OK: bool = false;

pub struct Native64;

impl Native64 {
    pub fn prepare() -> Result<Self> {
        INIT.call_once(|| unsafe {
            INIT_OK = refrun64_init() == 0;
        });
        // SAFETY: written once under `Once`, read after it.
        if !unsafe { INIT_OK } {
            bail!("refrun64_init failed (could not map the fixed region at 0x20000000)");
        }
        Ok(Native64)
    }

    pub fn run(&self, cases: &[Box<CaseIn64>]) -> Vec<Box<CaseOut64>> {
        cases
            .iter()
            .map(|c| {
                let mut out = CaseOut64::zeroed();
                // SAFETY: `prepare` mapped the region; the pointers are valid for the call.
                unsafe { refrun64_run(&**c, &mut *out) };
                out
            })
            .collect()
    }
}
