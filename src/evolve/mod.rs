//! Harness evolution subsystem: observability-driven, composable, durable.
//!
//! Pipeline (one round):
//! `bundle snapshot` -> `experience digest` -> `targeted patch proposal` -> `staged apply`
//! -> `validate` -> `evaluate before/after` -> `seesaw + dev gate` -> `manifest decision`
//! -> `durable accept/revert` -> `prediction verdict next round`.
//!
//! The subsystem is off by default (`EvolveConfig::enabled = false`) and never runs unless the
//! caller passes `--evolve` or invokes `goon evolve ...`.

pub mod bundle;
pub mod digest;
pub mod engine;
pub mod eval;
pub mod jit;
pub mod manifest;
pub mod patch;
pub mod pipeline;
pub mod profile;
pub mod store;
pub mod types;

pub use types::*;

use std::path::{Path, PathBuf};

/// Workspace-relative bundle directory (`.goon/harness`).
pub fn bundle_dir(workspace: &Path) -> PathBuf {
    workspace.join(type_bundle_dir())
}

fn type_bundle_dir() -> &'static str {
    types::BUNDLE_DIR
}

/// Whether a workspace already has an editable harness bundle.
pub fn has_bundle(workspace: &Path) -> bool {
    bundle_dir(workspace).is_dir()
}

/// Evolve state root: `{data_dir}/harness` unless `GOON_HARNESS_DIR` overrides it.
pub fn state_root(data_dir: &Path) -> PathBuf {
    if let Some(raw) = std::env::var_os("GOON_HARNESS_DIR") {
        let path = PathBuf::from(raw);
        if !path.as_os_str().is_empty() {
            return path;
        }
    }
    types::default_root(data_dir)
}
