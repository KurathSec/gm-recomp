//! Runs a batch through the freestanding 32-bit reference executor on the host CPU.

use crate::proto::{CaseIn, CaseOut, read_results, write_batch};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Native {
    pub exe: PathBuf,
    pub work: PathBuf,
}

impl Native {
    /// Compiles tests/cpu/refrun_linux.c if the binary is missing or older than the source.
    pub fn prepare(repo: &Path, work: &Path) -> Result<Self> {
        std::fs::create_dir_all(work)?;
        let src = repo.join("tests/cpu/refrun_linux.c");
        let exe = work.join("refrun");
        let stale = match (std::fs::metadata(&src), std::fs::metadata(&exe)) {
            (Ok(s), Ok(e)) => s.modified()? > e.modified()?,
            _ => true,
        };
        if stale {
            let st = Command::new("gcc")
                .args(["-m32", "-O1", "-nostdlib", "-static", "-ffreestanding", "-fno-builtin", "-fno-stack-protector", "-fno-pic", "-o"])
                .arg(&exe)
                .arg(&src)
                .status()
                .context("running gcc -m32")?;
            if !st.success() {
                bail!("build of refrun_linux.c failed");
            }
        }
        Ok(Native { exe, work: work.to_path_buf() })
    }

    pub fn run(&self, cases: &[CaseIn]) -> Result<Vec<CaseOut>> {
        let inp = self.work.join("batch.bin");
        let outp = self.work.join("result.bin");
        std::fs::write(&inp, write_batch(cases))?;
        let _ = std::fs::remove_file(&outp);
        let out = Command::new(&self.exe).arg(&inp).arg(&outp).output().context("running refrun")?;
        if !out.status.success() {
            bail!("refrun exited with {}: {}", out.status, String::from_utf8_lossy(&out.stderr));
        }
        let bytes = std::fs::read(&outp)?;
        read_results(&bytes, cases.len()).map_err(anyhow::Error::msg)
    }
}
