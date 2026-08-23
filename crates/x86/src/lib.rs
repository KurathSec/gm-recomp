//! Static code discovery over a PE32 image: recursive descent from known roots,
//! jump-table resolution, and pointer scans. Produces a coverage picture the
//! AOT recompiler and the profile-guided fallback are planned from.

pub mod discover;

pub use discover::{Discovery, Function, JumpTable, RootKind, discover};
