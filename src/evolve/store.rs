//! Durable, append-only state for harness evolution (`{data_dir}/harness`).
//!
//! Layout:
//! `snapshots/<id>/{bundle.json,files/**}` content-addressed harness snapshots,
//! `manifests/<round>.json` one durable record per round,
//! `runs/<id>.json` evaluation summaries, `digest/` evidence corpora and rendered reports,
//! `patches/` staged patch proposals, `bundles.jsonl` append-only bundle index,
//! `evodag.jsonl` append-only EvoDAG nodes, `solved.json` task ids already learned from,
//! `jit.jsonl` JIT decision archive.
//!
//! Every write is atomic (exclusive temp file + rename) and every directory is created private.
#![allow(dead_code)]

use crate::evolve::types::*;
use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Root keeper for snapshots, manifests, runs, digests, patches and the EvoDAG.
#[derive(Clone, Debug)]
pub struct HarnessStore {
    root: PathBuf,
}

impl HarnessStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn ensure(&self) -> Result<()> {
        for dir in [self.root.clone(), self.snapshots_dir(), self.manifests_dir(), self.runs_dir(), self.digest_dir(), self.patches_dir()] {
            crate::config::mkdir_private(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
        }
        Ok(())
    }

    pub fn snapshots_dir(&self) -> PathBuf {
        self.root.join("snapshots")
    }
    pub fn manifests_dir(&self) -> PathBuf {
        self.root.join("manifests")
    }
    pub fn runs_dir(&self) -> PathBuf {
        self.root.join("runs")
    }
    pub fn digest_dir(&self) -> PathBuf {
        self.root.join("digest")
    }
    pub fn patches_dir(&self) -> PathBuf {
        self.root.join("patches")
    }
    pub fn bundles_index(&self) -> PathBuf {
        self.root.join("bundles.jsonl")
    }
    pub fn nodes_index(&self) -> PathBuf {
        self.root.join("evodag.jsonl")
    }
    pub fn solved_path(&self) -> PathBuf {
        self.root.join("solved.json")
    }
    pub fn jit_archive(&self) -> PathBuf {
        self.root.join("jit.jsonl")
    }

    /// Directory holding one snapshot's `bundle.json` plus its component files.
    pub fn snapshot_dir(&self, id: &str) -> PathBuf {
        self.snapshots_dir().join(safe_id(id))
    }

    pub fn save_bundle(&self, bundle: &HarnessBundle) -> Result<()> {
        self.ensure()?;
        self.write_json(&self.snapshot_dir(&bundle.id).join("bundle.json"), bundle)?;
        self.index_bundle(bundle)
    }

    pub fn load_bundle(&self, id: &str) -> Result<HarnessBundle> {
        let path = self.snapshot_dir(id).join("bundle.json");
        read_json(&path, "bundle snapshot")?.ok_or_else(|| anyhow!("Bundle snapshot {id} is missing ({}).", path.display()))
    }

    /// Index order, first appearance wins; falls back to the snapshots directory.
    pub fn list_bundles(&self) -> Result<Vec<String>> {
        let mut ids: Vec<String> = Vec::new();
        for value in self.read_jsonl(&self.bundles_index())? {
            let Some(id) = value.get("id").and_then(serde_json::Value::as_str) else { continue };
            if !id.is_empty() && !ids.iter().any(|known| known == id) {
                ids.push(id.to_string());
            }
        }
        if ids.is_empty() {
            ids = self.dir_stems(&self.snapshots_dir())?;
        }
        Ok(ids)
    }

    /// Newest snapshot by `created`, tie-broken by round then id; unreadable snapshots are skipped.
    pub fn latest_bundle(&self) -> Result<Option<HarnessBundle>> {
        let mut entries: Vec<(String, u32, String)> = Vec::new();
        for value in self.read_jsonl(&self.bundles_index())? {
            let Some(id) = value.get("id").and_then(serde_json::Value::as_str) else { continue };
            if id.is_empty() {
                continue;
            }
            let created = value.get("created").and_then(serde_json::Value::as_str).unwrap_or_default().to_string();
            let round = value.get("round").and_then(serde_json::Value::as_u64).unwrap_or(0).min(u32::MAX as u64) as u32;
            entries.push((created, round, id.to_string()));
        }
        entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)).then_with(|| b.2.cmp(&a.2)));
        let mut seen = BTreeSet::new();
        for (_, _, id) in entries {
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Ok(bundle) = self.load_bundle(&id) {
                return Ok(Some(bundle));
            }
        }
        let mut best: Option<HarnessBundle> = None;
        for id in self.dir_stems(&self.snapshots_dir())? {
            let Ok(bundle) = self.load_bundle(&id) else { continue };
            let replace = match &best {
                None => true,
                Some(current) => (bundle.created.as_str(), bundle.round, bundle.id.as_str()) > (current.created.as_str(), current.round, current.id.as_str()),
            };
            if replace {
                best = Some(bundle);
            }
        }
        Ok(best)
    }

    pub fn save_component_files(&self, id: &str, files: &BTreeMap<String, String>) -> Result<()> {
        let base = self.snapshot_dir(id).join("files");
        crate::config::mkdir_private(&base).with_context(|| format!("cannot create {}", base.display()))?;
        for (path, body) in files {
            if component_kind_for_path(path).is_none() {
                bail!("Refusing to persist unknown component path {path}.");
            }
            let target = component_target(&base, path)?;
            self.write_atomic(&target, body.as_bytes())?;
        }
        Ok(())
    }

    pub fn component_files(&self, id: &str) -> Result<BTreeMap<String, String>> {
        let base = self.snapshot_dir(id).join("files");
        if !base.is_dir() {
            bail!("Bundle {id} has no component files ({}) yet.", base.display());
        }
        let mut files = BTreeMap::new();
        collect_component_files(&base, &base, &mut files)?;
        Ok(files)
    }

    pub fn save_run(&self, run: &RunSummary) -> Result<()> {
        self.ensure()?;
        self.write_json(&self.runs_dir().join(format!("{}.json", safe_id(&run.run_id))), run)
    }

    pub fn load_run(&self, id: &str) -> Result<RunSummary> {
        let path = self.runs_dir().join(format!("{}.json", safe_id(id)));
        read_json(&path, "run summary")?.ok_or_else(|| anyhow!("Run {id} is missing ({}).", path.display()))
    }

    pub fn list_runs(&self) -> Result<Vec<String>> {
        let mut ids = self.dir_stems(&self.runs_dir())?;
        ids.sort();
        Ok(ids)
    }

    pub fn save_manifest(&self, manifest: &ChangeManifest) -> Result<()> {
        self.ensure()?;
        self.write_json(&self.manifests_dir().join(format!("{}.json", manifest.round)), manifest)
    }

    pub fn load_manifest(&self, round: u32) -> Result<Option<ChangeManifest>> {
        read_json(&self.manifests_dir().join(format!("{round}.json")), "change manifest")
    }

    pub fn list_manifests(&self) -> Result<Vec<ChangeManifest>> {
        let mut manifests: Vec<ChangeManifest> = Vec::new();
        for id in self.dir_stems(&self.manifests_dir())? {
            let path = self.manifests_dir().join(format!("{id}.json"));
            let Some(manifest) = read_json::<ChangeManifest>(&path, "change manifest")? else { continue };
            manifests.push(manifest);
        }
        manifests.sort_by(|a, b| a.round.cmp(&b.round).then_with(|| a.created.cmp(&b.created)));
        Ok(manifests)
    }

    pub fn save_evidence(&self, name: &str, corpus: &EvidenceCorpus) -> Result<()> {
        self.ensure()?;
        self.write_json(&self.digest_dir().join(format!("{}.json", safe_name(name))), corpus)
    }

    pub fn load_evidence(&self, name: &str) -> Result<EvidenceCorpus> {
        let path = self.digest_dir().join(format!("{}.json", safe_name(name)));
        read_json(&path, "evidence corpus")?.ok_or_else(|| anyhow!("Evidence corpus {name} is missing ({}).", path.display()))
    }

    pub fn append_node(&self, node: &DagNode) -> Result<()> {
        self.ensure()?;
        let value = serde_json::to_value(node).context("cannot encode EvoDAG node")?;
        self.append_jsonl(&self.nodes_index(), &value)
    }

    pub fn nodes(&self) -> Result<Vec<DagNode>> {
        let mut nodes = Vec::new();
        for value in self.read_jsonl(&self.nodes_index())? {
            if let Ok(node) = serde_json::from_value::<DagNode>(value) {
                nodes.push(node);
            }
        }
        Ok(nodes)
    }

    pub fn solved(&self) -> Result<BTreeSet<String>> {
        let path = self.solved_path();
        let Some(ids) = read_json::<Vec<String>>(&path, "solved-task index")? else { return Ok(BTreeSet::new()) };
        Ok(ids.into_iter().filter(|id| !id.is_empty()).collect())
    }

    pub fn record_solved(&self, ids: impl IntoIterator<Item = String>) -> Result<()> {
        let mut solved = self.solved()?;
        for id in ids {
            if !id.is_empty() {
                solved.insert(id);
            }
        }
        self.ensure()?;
        let ordered: Vec<String> = solved.into_iter().collect();
        self.write_json(&self.solved_path(), &ordered)
    }

    pub fn write_atomic(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        if let Some(parent) = path.parent() {
            crate::config::mkdir_private(parent).with_context(|| format!("cannot create {}", parent.display()))?;
        }
        crate::config::atomic_write(path, bytes).with_context(|| format!("cannot write {}", path.display()))
    }

    pub fn append_jsonl(&self, path: &Path, value: &serde_json::Value) -> Result<()> {
        if let Some(parent) = path.parent() {
            crate::config::mkdir_private(parent).with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let line = serde_json::to_string(value).context("cannot encode JSONL record")?;
        let mut options = std::fs::OpenOptions::new();
        options.append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path).with_context(|| format!("cannot open {}", path.display()))?;
        file.write_all(line.as_bytes()).and_then(|_| file.write_all(b"\n")).and_then(|_| file.flush()).with_context(|| format!("cannot append to {}", path.display()))?;
        Ok(())
    }

    fn write_json<T: serde::Serialize>(&self, path: &Path, value: &T) -> Result<()> {
        let body = serde_json::to_string_pretty(value).context("cannot encode evolution state")?;
        self.write_atomic(path, format!("{body}\n").as_bytes())
    }

    fn index_bundle(&self, bundle: &HarnessBundle) -> Result<()> {
        let index = self.bundles_index();
        for value in self.read_jsonl(&index)? {
            if value.get("id").and_then(serde_json::Value::as_str) == Some(bundle.id.as_str()) {
                return Ok(());
            }
        }
        let record = serde_json::json!({
            "id": bundle.id,
            "parent": bundle.parent,
            "created": bundle.created,
            "round": bundle.round,
            "variant": bundle.variant,
            "components": bundle.components.len(),
        });
        self.append_jsonl(&index, &record)
    }

    /// Append-only JSONL: a torn or corrupt line (crash mid-write) is skipped, never fatal.
    fn read_jsonl(&self, path: &Path) -> Result<Vec<serde_json::Value>> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(anyhow::Error::new(error).context(format!("cannot read {}", path.display()))),
        };
        let mut values = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
                values.push(value);
            }
        }
        Ok(values)
    }

    fn dir_stems(&self, dir: &Path) -> Result<Vec<String>> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(anyhow::Error::new(error).context(format!("cannot list {}", dir.display()))),
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.with_context(|| format!("cannot read an entry of {}", dir.display()))?;
            let path = entry.path();
            let name = path.file_name().map(|name| name.to_string_lossy().to_string()).unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            if name.ends_with(".json") {
                let stem = name.trim_end_matches(".json");
                if !stem.is_empty() {
                    names.push(stem.to_string());
                }
            } else if path.is_dir() {
                names.push(name);
            }
        }
        names.sort();
        names.dedup();
        Ok(names)
    }
}

/// Validates a bundle-relative component path and joins it under `base`.
fn component_target(base: &Path, path: &str) -> Result<PathBuf> {
    let relative = safe_relative(path).ok_or_else(|| anyhow!("Refusing unsafe component path {path}."))?;
    let target = base.join(relative);
    if !target.starts_with(base) {
        bail!("Refusing component path {path} outside {}.", base.display());
    }
    Ok(target)
}

/// Splits a forward-slash relative path, rejecting roots, `..`, drive letters and backslashes.
pub fn safe_relative(path: &str) -> Option<PathBuf> {
    if path.is_empty() || path.len() > 240 || path.contains('\\') || path.contains(':') || path.starts_with('/') {
        return None;
    }
    let mut relative = PathBuf::new();
    for part in path.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return None;
        }
        relative.push(part);
    }
    Some(relative)
}

/// Filesystem-safe identifier (bundle ids are short hashes, but never trust a stored id).
pub fn safe_id(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(64));
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        }
        if out.len() >= 64 {
            break;
        }
    }
    if out.is_empty() {
        short_hash(raw.as_bytes())
    } else {
        out
    }
}

/// Filesystem-safe artifact name; sanitised names keep a hash suffix so they stay unique.
pub fn safe_name(raw: &str) -> String {
    let trimmed = raw.trim();
    let mut out = String::with_capacity(trimmed.len().min(48));
    for ch in trimmed.chars() {
        let keep = ch.is_ascii_alphanumeric() || ch == '-' || ch == '_';
        out.push(if keep { ch } else { '-' });
        if out.len() >= 48 {
            break;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        return short_hash(trimmed.as_bytes());
    }
    if out == trimmed {
        out
    } else {
        format!("{out}-{}", &short_hash(trimmed.as_bytes())[..8])
    }
}

fn collect_component_files(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) -> Result<()> {
    let entries = std::fs::read_dir(dir).with_context(|| format!("cannot list {}", dir.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("cannot read an entry of {}", dir.display()))?;
        let path = entry.path();
        let meta = entry.metadata().with_context(|| format!("cannot stat {}", path.display()))?;
        if meta.is_dir() {
            collect_component_files(root, &path, out)?;
            continue;
        }
        if !meta.is_file() {
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else { continue };
        let normalized = relative.components().map(|part| part.as_os_str().to_string_lossy().to_string()).collect::<Vec<_>>().join("/");
        if component_kind_for_path(&normalized).is_none() {
            continue;
        }
        let body = std::fs::read_to_string(&path).with_context(|| format!("cannot read component {}", path.display()))?;
        out.insert(normalized, body);
    }
    Ok(())
}

fn read_json<T: DeserializeOwned>(path: &Path, what: &str) -> Result<Option<T>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(anyhow::Error::new(error).context(format!("cannot read {what} at {}", path.display()))),
    };
    serde_json::from_str::<T>(&text).map(Some).with_context(|| format!("cannot parse {what} at {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(path, body)| (path.to_string(), body.to_string())).collect()
    }

    fn manifest(round: u32) -> ChangeManifest {
        ChangeManifest {
            schema: EVOLVE_SCHEMA,
            round,
            base: "base".into(),
            candidate: "candidate".into(),
            created: now_rfc3339(),
            accepted: true,
            reject_reason: None,
            before: Score { passed: 1, total: 2 },
            after: None,
            dev_before: None,
            dev_after: None,
            entries: vec![],
        }
    }

    #[test]
    fn snapshot_roundtrip_index_and_latest() {
        let dir = tempfile::tempdir().unwrap();
        let store = HarnessStore::new(dir.path());
        store.ensure().unwrap();
        let base_files = files(&[("system.md", "v1"), ("skills/rust.md", "# rust")]);
        let base = HarnessBundle::build(&base_files, None, 0, Variant::Code);
        store.save_component_files(&base.id, &base_files).unwrap();
        store.save_bundle(&base).unwrap();
        assert_eq!(store.component_files(&base.id).unwrap(), base_files);
        assert_eq!(store.load_bundle(&base.id).unwrap().id, base.id);
        assert_eq!(store.list_bundles().unwrap(), vec![base.id.clone()]);

        // Content-addressed identical snapshot is not indexed twice.
        store.save_bundle(&base).unwrap();
        assert_eq!(store.list_bundles().unwrap().len(), 1);

        let mut newer = HarnessBundle::build(&base_files, Some(&base.id), 1, Variant::Code);
        newer.id = "feedfacefeedface".into();
        newer.created = "2999-01-01T00:00:00Z".into();
        store.save_bundle(&newer).unwrap();
        assert_eq!(store.list_bundles().unwrap().len(), 2);
        assert_eq!(store.latest_bundle().unwrap().unwrap().id, newer.id);

        // A snapshot whose files are missing still reports a clear error instead of panicking.
        let missing = store.component_files("does-not-exist");
        assert!(missing.is_err());
        assert!(store.load_bundle("does-not-exist").is_err());
    }

    #[test]
    fn empty_store_has_no_latest_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let store = HarnessStore::new(dir.path().join("harness"));
        assert!(store.latest_bundle().unwrap().is_none());
        assert!(store.list_bundles().unwrap().is_empty());
        assert!(store.solved().unwrap().is_empty());
        assert!(store.load_manifest(3).unwrap().is_none());
        assert!(store.nodes().unwrap().is_empty());
        assert!(store.list_manifests().unwrap().is_empty());
        assert!(store.list_runs().unwrap().is_empty());
    }

    #[test]
    fn rejects_path_traversal_and_unknown_slots() {
        let dir = tempfile::tempdir().unwrap();
        let store = HarnessStore::new(dir.path());
        store.ensure().unwrap();
        for path in ["../escape.md", "skills/../../escape.md", "/abs/system.md", "skills\\win.md", "skills/x.txt", "", "./system.md"] {
            let hostile = files(&[(path, "nope")]);
            assert!(store.save_component_files("abc", &hostile).is_err(), "{path} must be rejected");
        }
        assert!(!dir.path().join("escape.md").exists());
        assert!(!dir.path().join("snapshots").join("escape.md").exists());
    }

    #[test]
    fn runs_manifests_evidence_nodes_and_solved_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = HarnessStore::new(dir.path());
        store.ensure().unwrap();

        let run = RunSummary::new(Split::Dev, Some("abc".into()), Some(Variant::Ops), vec![]);
        store.save_run(&run).unwrap();
        assert_eq!(store.load_run(&run.run_id).unwrap().run_id, run.run_id);
        assert_eq!(store.list_runs().unwrap(), vec![run.run_id.clone()]);
        assert!(store.load_run("nope").is_err());

        store.save_manifest(&manifest(2)).unwrap();
        store.save_manifest(&manifest(1)).unwrap();
        assert_eq!(store.load_manifest(1).unwrap().unwrap().round, 1);
        let rounds: Vec<u32> = store.list_manifests().unwrap().iter().map(|m| m.round).collect();
        assert_eq!(rounds, vec![1, 2]);

        let corpus = EvidenceCorpus::empty();
        store.save_evidence("corpus", &corpus).unwrap();
        assert_eq!(store.load_evidence("corpus").unwrap().schema, EVOLVE_SCHEMA);
        assert!(store.load_evidence("missing").is_err());

        let node = DagNode::new("abc", None, 0, Score { passed: 1, total: 2 }, None, vec!["lesson".into()]);
        store.append_node(&node).unwrap();
        store.append_node(&node).unwrap();
        assert_eq!(store.nodes().unwrap().len(), 2);

        assert!(store.solved().unwrap().is_empty());
        store.record_solved(vec!["a".to_string(), "b".to_string()]).unwrap();
        store.record_solved(vec!["b".to_string(), "c".to_string(), String::new()]).unwrap();
        assert_eq!(store.solved().unwrap(), ["a", "b", "c"].iter().map(|id| id.to_string()).collect::<BTreeSet<_>>());

        // Atomic writes create parents and land the exact bytes.
        let nested = dir.path().join("deep").join("deeper").join("note.json");
        store.write_atomic(&nested, b"{}\n").unwrap();
        assert_eq!(std::fs::read(&nested).unwrap(), b"{}\n");

        // A torn JSONL line is skipped, the intact records still parse.
        let index = store.nodes_index();
        let mut text = std::fs::read_to_string(&index).unwrap();
        text.push_str("{\"id\":\"torn\"");
        std::fs::write(&index, text).unwrap();
        assert_eq!(store.nodes().unwrap().len(), 2);
    }
}
