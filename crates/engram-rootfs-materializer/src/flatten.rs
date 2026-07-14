//! Whiteout-aware OCI layer flatten (ADR 0080 §C).
//!
//! Applies an ordered sequence of layer tars into one rootfs tree,
//! per the [OCI image-spec layer semantics](https://github.com/opencontainers/image-spec/blob/main/layer.md):
//!
//! - `.wh.<name>` removes `<name>` from lower layers.
//! - `.wh..wh..opq` makes its directory opaque: every lower-layer
//!   child is dropped; the marker layer's own children are kept.
//! - A later-layer entry replaces the earlier one (a dir replaced by
//!   a file removes the whole subtree, and vice versa).
//! - Hardlinks become real hardlinks; symlinks stay symlinks.
//! - Modes — including setuid/setgid/sticky — are applied to the tree.
//!
//! ## Ownership: the unprivileged seam
//!
//! `mke2fs -d` copies the tree's ownership **as-is** into the ext4
//! image, and an unprivileged process cannot `chown`. So the flatten
//! records every entry's tar-carried `(uid, gid, mode)` in a
//! [`TreeMetadata`] sidecar and applies what it can:
//!
//! - **modes** (incl. setuid) — always applied (chmod on own files is
//!   unprivileged-safe).
//! - **uid/gid** — applied by [`TreeMetadata::apply_ownership`], which
//!   the phase-3b host RPC (running as root) calls before the pack;
//!   under an unprivileged caller (tests, dev) it stops at the first
//!   `PermissionDenied` and the tree keeps the caller's uid — exactly
//!   the limitation the retiring `docker export` bake had (it also ran
//!   as the bake user).
//! - **xattrs** — applied where the filesystem allows; refusals (e.g.
//!   `security.*` on macOS or unprivileged Linux) are collected in
//!   [`TreeMetadata::skipped_xattrs`], never silently dropped.
//! - **device nodes / fifos** — mknod is root-only; recorded in
//!   [`TreeMetadata::skipped_specials`] (rare in session images; the
//!   guest's devtmpfs provides /dev at boot).
//!
//! ## Path safety
//!
//! Every entry (and hardlink target) is normalized and must stay
//! inside the tree: absolute paths and any `..` component are a hard
//! error (zip-slip), not a skip. Ancestor symlinks are resolved the
//! way the guest kernel would, SCOPED to the tree (docker's
//! `FollowSymlinkInScope`): `bin -> usr/bin` from a lower layer makes
//! an upper layer's `bin/ls` land at `usr/bin/ls` (the usrmerge
//! case), while a hostile `evil -> /host` re-anchors at the tree
//! root — nothing is ever written through a symlink to the host fs.

mod parallel;

use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

pub use parallel::{default_write_concurrency, ChannelReader};
use parallel::{WriteJob, WritePool, INLINE_WRITE_THRESHOLD};

/// Tar-carried identity of one flattened entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EntryMeta {
    pub uid: u64,
    pub gid: u64,
    /// Permission bits incl. setuid/setgid/sticky (`mode & 0o7777`).
    pub mode: u32,
}

/// An xattr the filesystem refused (collected, never silently lost).
#[derive(Clone, Debug)]
pub struct SkippedXattr {
    pub path: String,
    pub name: String,
    pub error: String,
}

/// Sidecar metadata for a flattened tree — the ownership/xattr record
/// that survives running unprivileged (see the module docs).
#[derive(Debug, Default)]
pub struct TreeMetadata {
    entries: BTreeMap<String, EntryMeta>,
    pub skipped_xattrs: Vec<SkippedXattr>,
    /// Device nodes / fifos the unprivileged flatten couldn't create
    /// (tar paths).
    pub skipped_specials: Vec<String>,
}

impl TreeMetadata {
    /// The recorded tar identity for a tree-relative path (`etc/passwd`).
    pub fn get(&self, rel: &str) -> Option<&EntryMeta> {
        self.entries.get(rel)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &EntryMeta)> {
        self.entries.iter()
    }

    fn insert(&mut self, rel: String, meta: EntryMeta) {
        self.entries.insert(rel, meta);
    }

    /// Drop the record for `rel` and everything under it.
    fn remove_subtree(&mut self, rel: &str) {
        let prefix = format!("{rel}/");
        self.entries
            .retain(|k, _| k != rel && !k.starts_with(&prefix));
    }

    /// Apply the recorded uid/gid (and re-assert the mode — `chown`
    /// strips setuid/setgid bits) onto the real tree. Root-only in
    /// practice: the phase-3b host RPC calls this before the ext4
    /// pack so `mke2fs -d` copies true ownership into the image.
    ///
    /// Returns `PermissionDenied` from the first refused chown —
    /// callers running unprivileged treat that as "recorded, not
    /// applied" (the documented dev/test limitation); any other error
    /// is real and propagates.
    pub fn apply_ownership(&self, root: &Path) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        for (rel, meta) in &self.entries {
            let p = root.join(rel);
            let Ok(fs_meta) = std::fs::symlink_metadata(&p) else {
                continue; // replaced/removed by a later layer's whiteout
            };
            std::os::unix::fs::lchown(&p, Some(meta.uid as u32), Some(meta.gid as u32))?;
            // chown clears setuid/setgid on regular files — re-apply the
            // recorded mode (symlinks carry no meaningful mode).
            if !fs_meta.file_type().is_symlink() {
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(meta.mode))?;
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum FlattenError {
    Io(std::io::Error),
    /// Zip-slip: an entry (or hardlink target) escapes the tree.
    PathEscape(String),
    /// A hardlink whose target doesn't exist in the tree built so far.
    HardlinkTarget {
        path: String,
        target: String,
    },
    /// Malformed tar (unreadable header fields, missing link name, …).
    Malformed(String),
}

impl std::fmt::Display for FlattenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::PathEscape(p) => {
                write!(f, "layer entry escapes the rootfs (zip-slip rejected): {p}")
            }
            Self::HardlinkTarget { path, target } => {
                write!(f, "hardlink {path} -> {target}: target not in tree")
            }
            Self::Malformed(m) => write!(f, "malformed layer tar: {m}"),
        }
    }
}

impl std::error::Error for FlattenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for FlattenError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Opaque-dir whiteout marker (`.wh..wh..opq`).
const OPAQUE_MARKER: &str = ".wh..wh..opq";
/// File whiteout prefix (`.wh.<name>`).
const WHITEOUT_PREFIX: &str = ".wh.";

/// The parallel flatten engine (ADR 0088 addendum). Owns a persistent
/// [`WritePool`] (reused across layers) plus the memoized
/// ancestor-symlink facts; one instance per materialize.
///
/// Concurrency contract, load-bearing for correctness:
///
/// - The CALLER's thread (the "reader") performs every namespace
///   operation in exact tar order: sanitize, scoped resolution,
///   whiteouts/opaque, dir/symlink/hardlink creation, and the
///   unlink+create for every regular file.
/// - Workers receive an already-open fd and do only fd-scoped work
///   (write, fchmod, fsetxattr, futimens). They never touch paths, so
///   a later namespace op can at worst orphan an in-flight inode —
///   exactly the state sequential apply would leave. The final tree is
///   bit-identical to sequential apply (mke2fs determinism + rebake
///   chunk-dedup preserved).
/// - [`Flattener::finish`] joins the pool and MUST complete before
///   anything consumes the tree (ownership pass, mtime clamp, pack).
pub struct Flattener {
    root: PathBuf,
    pool: WritePool,
    /// Memoized "this tree-relative path is NOT a symlink" facts for
    /// the scoped ancestor resolution — amortizes the per-entry
    /// ancestor lstat walk to ~zero. Invalidated (prefix-wide) by
    /// `remove_entry`, which every type-replacing write funnels
    /// through.
    resolve_cache: HashSet<String>,
}

impl Flattener {
    pub fn new(root: &Path, write_concurrency: usize) -> Result<Self, FlattenError> {
        Ok(Self {
            root: root.to_path_buf(),
            pool: WritePool::new(write_concurrency)?,
            resolve_cache: HashSet::new(),
        })
    }

    /// Apply one layer tar onto the tree, updating `meta`. Layers must
    /// be applied in manifest order (base first). Takes any `Read` so
    /// tests feed in-memory tars and the pipeline feeds decompressors.
    /// Blocking (std IO); call from `spawn_blocking` in async contexts.
    ///
    /// On `Err`, call [`Flattener::finish`] and prefer ITS error — a
    /// mid-layer abort is usually the echo of a worker failure whose
    /// root cause the pool holds.
    pub fn apply_layer<R: Read>(
        &mut self,
        meta: &mut TreeMetadata,
        layer: R,
    ) -> Result<(), FlattenError> {
        let mut archive = tar::Archive::new(layer);
        // Whiteouts (incl. opaque) apply to LOWER layers only: entries this
        // layer created are immune, whatever order the archive lists them in.
        let mut created_this_layer: HashSet<String> = HashSet::new();

        for entry in archive.entries()? {
            if self.pool.failed() {
                // A worker already failed; stop queueing work. finish()
                // carries the root cause.
                return Err(FlattenError::Io(std::io::Error::other(
                    "flatten write pool failed (see finish)",
                )));
            }
            let mut entry = entry?;
            let raw_path = entry
                .path()
                .map_err(|e| FlattenError::Malformed(format!("entry path: {e}")))?
                .into_owned();
            let Some(rel) = sanitize(&raw_path)? else {
                continue; // the root dir itself ("./")
            };
            // Ancestor symlinks resolve the way the GUEST kernel would,
            // scoped to the tree (the usrmerge case: base layer has
            // `bin -> usr/bin`, upper writes `bin/ls` → usr/bin/ls). This
            // is also the symlink-traversal half of zip-slip: without it,
            // a layer symlink pointing at an absolute host path would have
            // later entries written THROUGH it, outside the tree.
            let rel = self.resolve_scoped(&rel)?;
            if rel.is_empty() {
                continue;
            }
            let (parent, name) = split_parent(&rel);

            // --- whiteouts ---
            if name == OPAQUE_MARKER {
                let parent = parent.to_string();
                self.opaque_dir(meta, &parent, &created_this_layer)?;
                continue;
            }
            if let Some(hidden) = name.strip_prefix(WHITEOUT_PREFIX) {
                let target = join_rel(parent, hidden);
                if !created_this_layer.contains(&target) {
                    self.remove_entry(meta, &target)?;
                }
                continue;
            }

            // --- real entries ---
            let header = entry.header();
            let mode = header
                .mode()
                .map_err(|e| FlattenError::Malformed(format!("{rel}: mode: {e}")))?
                & 0o7777;
            let uid = header
                .uid()
                .map_err(|e| FlattenError::Malformed(format!("{rel}: uid: {e}")))?;
            let gid = header
                .gid()
                .map_err(|e| FlattenError::Malformed(format!("{rel}: gid: {e}")))?;
            let mtime = header.mtime().unwrap_or(0);
            let size = header.size().unwrap_or(0);
            let entry_meta = EntryMeta { uid, gid, mode };
            let dst = self.root.join(&rel);

            use tar::EntryType;
            match header.entry_type() {
                EntryType::Directory => {
                    // Replacing a non-dir with a dir drops the old entry;
                    // an existing dir is merged (metadata refreshed).
                    if let Ok(m) = std::fs::symlink_metadata(&dst) {
                        if !m.is_dir() {
                            self.remove_entry(meta, &rel)?;
                        }
                    }
                    std::fs::create_dir_all(&dst)?;
                    set_mode(&dst, mode)?;
                    apply_xattrs(&mut entry, &dst, &rel, meta)?;
                    meta.insert(rel.clone(), entry_meta);
                    created_this_layer.insert(rel);
                }
                EntryType::Regular | EntryType::Continuous | EntryType::GNUSparse => {
                    self.remove_entry(meta, &rel)?;
                    ensure_parent(&dst)?;
                    // PAX xattrs are parsed reader-side (they borrow the
                    // entry); application is fd-scoped in the worker.
                    let xattrs = collect_xattrs(&mut entry)?;
                    if size > INLINE_WRITE_THRESHOLD {
                        // Large entry: stream inline on the reader —
                        // constant memory, bandwidth-bound anyway.
                        let f = std::fs::File::create(&dst)?;
                        {
                            let mut w = &f;
                            std::io::copy(&mut entry, &mut w)?;
                        }
                        for skipped in parallel::finish_file_fd(&f, &rel, mode, mtime, &xattrs)
                            .map_err(FlattenError::Io)?
                        {
                            meta.skipped_xattrs.push(skipped);
                        }
                    } else {
                        let mut bytes = Vec::with_capacity(size as usize);
                        entry.read_to_end(&mut bytes)?;
                        let file = std::fs::File::create(&dst)?;
                        self.pool.submit(WriteJob {
                            file,
                            rel: rel.clone(),
                            bytes,
                            mode,
                            mtime,
                            xattrs,
                        })?;
                    }
                    meta.insert(rel.clone(), entry_meta);
                    created_this_layer.insert(rel);
                }
                EntryType::Symlink => {
                    let target = entry
                        .link_name()
                        .map_err(|e| FlattenError::Malformed(format!("{rel}: link name: {e}")))?
                        .ok_or_else(|| {
                            FlattenError::Malformed(format!("{rel}: symlink without target"))
                        })?
                        .into_owned();
                    self.remove_entry(meta, &rel)?;
                    ensure_parent(&dst)?;
                    // Symlink TARGETS are stored verbatim — absolute or
                    // `..`-relative targets are legal inside the image (they
                    // resolve in the GUEST's namespace, e.g. /bin -> /usr/bin)
                    // and never dereferenced on the host by this crate.
                    std::os::unix::fs::symlink(&target, &dst)?;
                    meta.insert(rel.clone(), entry_meta);
                    created_this_layer.insert(rel);
                }
                EntryType::Link => {
                    let raw_target = entry
                        .link_name()
                        .map_err(|e| FlattenError::Malformed(format!("{rel}: link name: {e}")))?
                        .ok_or_else(|| {
                            FlattenError::Malformed(format!("{rel}: hardlink without target"))
                        })?
                        .into_owned();
                    // Hardlink targets are tree-relative paths and ARE
                    // resolved on the host — path-safety + scoped
                    // ancestor-symlink resolution apply. The target inode
                    // exists the moment the reader File::create'd it, even
                    // if a worker is still writing its content — both
                    // names share the inode either way.
                    let target_rel = sanitize(&raw_target)?.ok_or_else(|| {
                        FlattenError::PathEscape(raw_target.display().to_string())
                    })?;
                    let target_rel = self.resolve_scoped(&target_rel)?;
                    let target_abs = self.root.join(&target_rel);
                    if !target_abs.exists() {
                        return Err(FlattenError::HardlinkTarget {
                            path: rel,
                            target: target_rel,
                        });
                    }
                    self.remove_entry(meta, &rel)?;
                    ensure_parent(&dst)?;
                    std::fs::hard_link(&target_abs, &dst)?;
                    // A hardlink shares the target's inode — record the
                    // target's identity so the sidecar stays consistent.
                    let linked_meta = meta.get(&target_rel).copied().unwrap_or(entry_meta);
                    meta.insert(rel.clone(), linked_meta);
                    created_this_layer.insert(rel);
                }
                EntryType::Fifo | EntryType::Char | EntryType::Block => {
                    // mknod is root-only; record and continue (module docs).
                    tracing::warn!(path = %rel, kind = ?header.entry_type(), "skipping special file (mknod requires root)");
                    meta.skipped_specials.push(rel.clone());
                    meta.insert(rel, entry_meta);
                }
                // PAX/GNU metadata records are consumed by the tar crate
                // itself (surfaced via pax_extensions on the entry that
                // follows); anything else is noise, not rootfs content.
                _ => {}
            }
        }
        Ok(())
    }

    /// Join every worker; surface the first worker failure; fold the
    /// fd-applied xattr refusals into `meta`. MUST complete (Ok) before
    /// the ownership pass / mtime clamp / pack consume the tree.
    pub fn finish(self, meta: &mut TreeMetadata) -> Result<(), FlattenError> {
        match self.pool.finish() {
            Ok(mut skipped) => {
                meta.skipped_xattrs.append(&mut skipped);
                Ok(())
            }
            Err((rel, e)) => Err(FlattenError::Io(std::io::Error::new(
                e.kind(),
                format!("{rel}: {e}"),
            ))),
        }
    }

    /// `remove_entry` + resolve-cache invalidation. EVERY namespace
    /// removal funnels through here — that single choke point is what
    /// keeps the memoized not-a-symlink facts sound (a dir replaced by
    /// a symlink invalidates itself and everything beneath it).
    fn remove_entry(&mut self, meta: &mut TreeMetadata, rel: &str) -> std::io::Result<()> {
        let prefix = format!("{rel}/");
        self.resolve_cache
            .retain(|k| k != rel && !k.starts_with(&prefix));
        remove_entry(&self.root, meta, rel)
    }

    /// `.wh..wh..opq`: drop every child of `dir_rel` that this layer
    /// didn't itself create. `read_dir` sees exactly the names the
    /// reader created — workers add none.
    fn opaque_dir(
        &mut self,
        meta: &mut TreeMetadata,
        dir_rel: &str,
        created_this_layer: &HashSet<String>,
    ) -> Result<(), FlattenError> {
        let dir = if dir_rel.is_empty() {
            self.root.clone()
        } else {
            self.root.join(dir_rel)
        };
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            let child_rel = join_rel(dir_rel, &entry.file_name().to_string_lossy());
            if !created_this_layer.contains(&child_rel) {
                self.remove_entry(meta, &child_rel)?;
            }
        }
        Ok(())
    }

    /// Resolve `rel`'s ANCESTOR symlinks the way the guest kernel would,
    /// scoped to the tree — docker's `FollowSymlinkInScope` semantics:
    /// an absolute symlink target re-anchors at the tree root (never the
    /// host's `/`), `..` clamps at the root, and the FINAL component is
    /// never followed (an entry replaces the node itself — lstat
    /// semantics; `remove_entry` runs before every write). Bounded hops
    /// so a symlink loop fails loud instead of spinning.
    ///
    /// Not-a-symlink facts (including "nothing there yet" — also not a
    /// symlink) are memoized in `resolve_cache`; only symlink CREATION
    /// can flip a fact, and every creation path unlinks first via
    /// [`Flattener::remove_entry`], which invalidates.
    fn resolve_scoped(&mut self, rel: &str) -> Result<String, FlattenError> {
        use std::collections::VecDeque;
        let mut queue: VecDeque<String> = rel.split('/').map(str::to_string).collect();
        let mut resolved: Vec<String> = Vec::new();
        let mut hops = 0u32;
        while let Some(comp) = queue.pop_front() {
            if comp.is_empty() || comp == "." {
                continue;
            }
            if comp == ".." {
                // Only symlink targets can inject `..` (sanitize rejected
                // it in entry paths); clamp at the tree root.
                resolved.pop();
                continue;
            }
            if queue.is_empty() {
                // Final component: never followed.
                resolved.push(comp);
                break;
            }
            let candidate_rel = if resolved.is_empty() {
                comp.clone()
            } else {
                format!("{}/{comp}", resolved.join("/"))
            };
            let is_symlink = if self.resolve_cache.contains(&candidate_rel) {
                false
            } else {
                let is = std::fs::symlink_metadata(self.root.join(&candidate_rel))
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false);
                if !is {
                    self.resolve_cache.insert(candidate_rel);
                }
                is
            };
            if is_symlink {
                hops += 1;
                if hops > 40 {
                    return Err(FlattenError::Malformed(format!(
                        "symlink loop while resolving {rel}"
                    )));
                }
                let target = std::fs::read_link(self.root.join(if resolved.is_empty() {
                    comp.clone()
                } else {
                    format!("{}/{comp}", resolved.join("/"))
                }))?;
                if target.is_absolute() {
                    resolved.clear();
                }
                let target = target.to_string_lossy().into_owned();
                for c in target.split('/').rev() {
                    queue.push_front(c.to_string());
                }
            } else {
                resolved.push(comp);
            }
        }
        Ok(resolved.join("/"))
    }
}

/// Apply one layer tar onto the tree at `root`, updating `meta` —
/// the single-layer convenience over [`Flattener`] (tests, the bake).
/// The materializer's hot path constructs one `Flattener` and reuses
/// its pool across all layers.
pub fn apply_layer<R: Read>(
    root: &Path,
    meta: &mut TreeMetadata,
    layer: R,
) -> Result<(), FlattenError> {
    let mut flattener = Flattener::new(root, default_write_concurrency())?;
    let applied = flattener.apply_layer(meta, layer);
    let finished = flattener.finish(meta);
    match (applied, finished) {
        // finish() holds the root cause when a worker failed mid-apply.
        (_, Err(e)) => Err(e),
        (Err(e), Ok(())) => Err(e),
        (Ok(()), Ok(())) => Ok(()),
    }
}

/// Normalize a tar path to a tree-relative `a/b/c` string.
/// `Ok(None)` = the tree root itself. Rejects absolute paths and any
/// `..` component (zip-slip).
fn sanitize(path: &Path) -> Result<Option<String>, FlattenError> {
    let mut parts: Vec<String> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::Normal(c) => parts.push(c.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) | Component::ParentDir => {
                return Err(FlattenError::PathEscape(path.display().to_string()));
            }
        }
    }
    if parts.is_empty() {
        return Ok(None);
    }
    Ok(Some(parts.join("/")))
}

/// (`parent`, `basename`) of a normalized rel path. Parent is `""` at
/// the tree root.
fn split_parent(rel: &str) -> (&str, &str) {
    match rel.rsplit_once('/') {
        Some((p, n)) => (p, n),
        None => ("", rel),
    }
}

fn join_rel(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

fn ensure_parent(dst: &Path) -> std::io::Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// Remove `rel` (file, symlink, or whole dir subtree) from the tree
/// and the metadata record. Missing entries are fine (a whiteout may
/// name something an earlier whiteout already dropped).
fn remove_entry(root: &Path, meta: &mut TreeMetadata, rel: &str) -> std::io::Result<()> {
    let p = root.join(rel);
    match std::fs::symlink_metadata(&p) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(&p)?,
        Ok(_) => std::fs::remove_file(&p)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    meta.remove_subtree(rel);
    Ok(())
}

/// Parse the entry's PAX-carried xattrs (`SCHILY.xattr.*`) without
/// applying them — the reader parses (the extensions borrow the
/// entry), the worker applies fd-scoped.
fn collect_xattrs<R: Read>(
    entry: &mut tar::Entry<'_, R>,
) -> Result<Vec<(String, Vec<u8>)>, FlattenError> {
    let Some(exts) = entry.pax_extensions()? else {
        return Ok(Vec::new());
    };
    let mut xattrs: Vec<(String, Vec<u8>)> = Vec::new();
    for ext in exts {
        let ext = ext?;
        let Ok(key) = ext.key() else { continue };
        if let Some(name) = key.strip_prefix("SCHILY.xattr.") {
            xattrs.push((name.to_string(), ext.value_bytes().to_vec()));
        }
    }
    Ok(xattrs)
}

/// Apply the entry's PAX-carried xattrs (`SCHILY.xattr.*`) to the
/// extracted file/dir; refusals are collected on `meta` (module docs).
/// Symlink entries never reach here — `xattr::set` follows links.
fn apply_xattrs<R: Read>(
    entry: &mut tar::Entry<'_, R>,
    dst: &Path,
    rel: &str,
    meta: &mut TreeMetadata,
) -> Result<(), FlattenError> {
    let Some(exts) = entry.pax_extensions()? else {
        return Ok(());
    };
    // Collect first: applying while iterating would borrow `entry` twice.
    let mut xattrs: Vec<(String, Vec<u8>)> = Vec::new();
    for ext in exts {
        let ext = ext?;
        let Ok(key) = ext.key() else { continue };
        if let Some(name) = key.strip_prefix("SCHILY.xattr.") {
            xattrs.push((name.to_string(), ext.value_bytes().to_vec()));
        }
    }
    for (name, value) in xattrs {
        if let Err(e) = xattr::set(dst, &name, &value) {
            meta.skipped_xattrs.push(SkippedXattr {
                path: rel.to_string(),
                name,
                error: e.to_string(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::PathBuf;

    // ---- tiny in-memory tar-layer builders (KB-sized fixtures) ----

    struct LayerBuilder {
        tar: tar::Builder<Vec<u8>>,
    }

    impl LayerBuilder {
        fn new() -> Self {
            Self {
                tar: tar::Builder::new(Vec::new()),
            }
        }

        fn header(mode: u32, uid: u64, gid: u64, size: u64, kind: tar::EntryType) -> tar::Header {
            let mut h = tar::Header::new_gnu();
            h.set_mode(mode);
            h.set_uid(uid);
            h.set_gid(gid);
            h.set_size(size);
            h.set_mtime(946_684_800); // 2000-01-01, deterministic
            h.set_entry_type(kind);
            h
        }

        fn file_owned(mut self, path: &str, mode: u32, uid: u64, gid: u64, body: &[u8]) -> Self {
            let mut h = Self::header(mode, uid, gid, body.len() as u64, tar::EntryType::Regular);
            self.tar.append_data(&mut h, path, body).unwrap();
            self
        }

        fn file(self, path: &str, mode: u32, body: &[u8]) -> Self {
            self.file_owned(path, mode, 0, 0, body)
        }

        fn dir(mut self, path: &str, mode: u32) -> Self {
            let mut h = Self::header(mode, 0, 0, 0, tar::EntryType::Directory);
            self.tar.append_data(&mut h, path, &[][..]).unwrap();
            self
        }

        fn symlink(mut self, path: &str, target: &str) -> Self {
            let mut h = Self::header(0o777, 0, 0, 0, tar::EntryType::Symlink);
            self.tar.append_link(&mut h, path, target).unwrap();
            self
        }

        fn hardlink(mut self, path: &str, target: &str) -> Self {
            let mut h = Self::header(0o644, 0, 0, 0, tar::EntryType::Link);
            self.tar.append_link(&mut h, path, target).unwrap();
            self
        }

        fn whiteout(self, dir: &str, name: &str) -> Self {
            let path = if dir.is_empty() {
                format!(".wh.{name}")
            } else {
                format!("{dir}/.wh.{name}")
            };
            self.file(&path, 0o644, b"")
        }

        fn opaque(self, dir: &str) -> Self {
            self.file(&format!("{dir}/.wh..wh..opq"), 0o644, b"")
        }

        fn build(mut self) -> Vec<u8> {
            self.tar.finish().unwrap();
            self.tar.into_inner().unwrap()
        }
    }

    fn flatten(layers: &[Vec<u8>]) -> (tempfile::TempDir, TreeMetadata) {
        let root = tempfile::tempdir().unwrap();
        let mut meta = TreeMetadata::default();
        for layer in layers {
            apply_layer(root.path(), &mut meta, layer.as_slice()).unwrap();
        }
        (root, meta)
    }

    /// `.wh.<name>` in an upper layer removes the lower layer's entry
    /// (from the tree AND the sidecar); siblings survive.
    #[test]
    fn file_whiteout_removes_lower_entry() {
        let lower = LayerBuilder::new()
            .dir("etc", 0o755)
            .file("etc/removed.conf", 0o644, b"bye")
            .file("etc/kept.conf", 0o644, b"hi")
            .build();
        let upper = LayerBuilder::new().whiteout("etc", "removed.conf").build();
        let (root, meta) = flatten(&[lower, upper]);
        assert!(!root.path().join("etc/removed.conf").exists());
        assert!(root.path().join("etc/kept.conf").exists());
        assert!(meta.get("etc/removed.conf").is_none());
        assert!(meta.get("etc/kept.conf").is_some());
    }

    /// A whiteout can also remove a whole lower DIRECTORY subtree.
    #[test]
    fn file_whiteout_removes_lower_directory_subtree() {
        let lower = LayerBuilder::new()
            .dir("opt", 0o755)
            .dir("opt/tool", 0o755)
            .file("opt/tool/bin", 0o755, b"x")
            .build();
        let upper = LayerBuilder::new().whiteout("opt", "tool").build();
        let (root, meta) = flatten(&[lower, upper]);
        assert!(!root.path().join("opt/tool").exists());
        assert!(root.path().join("opt").is_dir());
        assert!(
            meta.get("opt/tool/bin").is_none(),
            "subtree records dropped"
        );
    }

    /// `.wh..wh..opq` drops ALL lower-layer children but keeps the
    /// marker layer's own children — regardless of entry order within
    /// the layer (the upper's child here precedes the marker).
    #[test]
    fn opaque_dir_drops_lower_children_keeps_uppers() {
        let lower = LayerBuilder::new()
            .dir("cfg", 0o755)
            .file("cfg/lower-a", 0o644, b"a")
            .file("cfg/lower-b", 0o644, b"b")
            .build();
        let upper = LayerBuilder::new()
            .dir("cfg", 0o755)
            .file("cfg/upper", 0o644, b"u") // created BEFORE the marker on purpose
            .opaque("cfg")
            .build();
        let (root, meta) = flatten(&[lower, upper]);
        assert!(!root.path().join("cfg/lower-a").exists());
        assert!(!root.path().join("cfg/lower-b").exists());
        assert_eq!(std::fs::read(root.path().join("cfg/upper")).unwrap(), b"u");
        assert!(meta.get("cfg/lower-a").is_none());
        assert!(meta.get("cfg/upper").is_some());
    }

    /// A hardlink pair shares one inode after flatten.
    #[test]
    fn hardlink_pair_stays_linked() {
        let layer = LayerBuilder::new()
            .file("bin/busybox", 0o755, b"#!x")
            .hardlink("bin/sh", "bin/busybox")
            .build();
        let (root, meta) = flatten(&[layer]);
        let a = std::fs::metadata(root.path().join("bin/busybox")).unwrap();
        let b = std::fs::metadata(root.path().join("bin/sh")).unwrap();
        assert_eq!(a.ino(), b.ino(), "hardlink must share the inode");
        assert_eq!(a.nlink(), 2);
        // The link inherits the target's recorded identity.
        assert_eq!(meta.get("bin/sh"), meta.get("bin/busybox"));
    }

    /// setuid (and the full 4-digit mode) survives extraction.
    #[test]
    fn setuid_bit_preserved() {
        let layer = LayerBuilder::new()
            .file("usr/bin/sudo", 0o4755, b"elf")
            .build();
        let (root, meta) = flatten(&[layer]);
        let mode = std::fs::metadata(root.path().join("usr/bin/sudo"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o7777, 0o4755, "setuid must survive on the tree");
        assert_eq!(meta.get("usr/bin/sudo").unwrap().mode, 0o4755);
    }

    /// uid/gid land in the sidecar even though the unprivileged
    /// flatten can't chown the real tree (the ownership seam 3b
    /// applies as root).
    #[test]
    fn uid_gid_recorded_in_sidecar() {
        let layer = LayerBuilder::new()
            .file_owned("home/dev/.bashrc", 0o644, 1000, 1000, b"x")
            .file_owned("etc/shadow", 0o600, 0, 42, b"y")
            .build();
        let (_root, meta) = flatten(&[layer]);
        assert_eq!(
            meta.get("home/dev/.bashrc"),
            Some(&EntryMeta {
                uid: 1000,
                gid: 1000,
                mode: 0o644
            })
        );
        assert_eq!(
            meta.get("etc/shadow"),
            Some(&EntryMeta {
                uid: 0,
                gid: 42,
                mode: 0o600
            })
        );
    }

    /// Symlinks are preserved as symlinks (targets verbatim, even
    /// absolute — they resolve in the guest, not on the host).
    #[test]
    fn symlink_preserved() {
        let layer = LayerBuilder::new()
            .file("usr/bin/python3.11", 0o755, b"elf")
            .symlink("usr/bin/python3", "python3.11")
            .symlink("bin", "/usr/bin")
            .build();
        let (root, _meta) = flatten(&[layer]);
        let l = root.path().join("usr/bin/python3");
        assert!(std::fs::symlink_metadata(&l)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read_link(&l).unwrap(), PathBuf::from("python3.11"));
        assert_eq!(
            std::fs::read_link(root.path().join("bin")).unwrap(),
            PathBuf::from("/usr/bin"),
            "absolute symlink target stored verbatim"
        );
    }

    /// A later layer's file replaces the earlier one — content AND mode.
    #[test]
    fn later_layer_replaces_file_and_mode() {
        let lower = LayerBuilder::new().file("app/run.sh", 0o644, b"v1").build();
        let upper = LayerBuilder::new()
            .file("app/run.sh", 0o755, b"v2-longer")
            .build();
        let (root, meta) = flatten(&[lower, upper]);
        let p = root.path().join("app/run.sh");
        assert_eq!(std::fs::read(&p).unwrap(), b"v2-longer");
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o7777,
            0o755
        );
        assert_eq!(meta.get("app/run.sh").unwrap().mode, 0o755);
    }

    /// A later layer may replace a DIRECTORY with a file (the whole
    /// lower subtree goes).
    #[test]
    fn later_layer_replaces_dir_with_file() {
        let lower = LayerBuilder::new()
            .dir("cache", 0o755)
            .file("cache/blob", 0o644, b"old")
            .build();
        let upper = LayerBuilder::new()
            .file("cache", 0o644, b"now-a-file")
            .build();
        let (root, meta) = flatten(&[lower, upper]);
        assert!(root.path().join("cache").is_file());
        assert!(meta.get("cache/blob").is_none());
    }

    /// Craft a tar whose header carries a RAW (unvalidated) path —
    /// the tar crate's own builders refuse `..`/absolute paths, but a
    /// malicious registry doesn't use the tar crate.
    fn raw_path_layer(evil_path: &str, evil_link: Option<&str>) -> Vec<u8> {
        let body: &[u8] = if evil_link.is_some() { b"" } else { b"x" };
        let mut h = tar::Header::new_gnu();
        h.set_mode(0o644);
        h.set_uid(0);
        h.set_gid(0);
        h.set_size(body.len() as u64);
        h.set_entry_type(if evil_link.is_some() {
            tar::EntryType::Link
        } else {
            tar::EntryType::Regular
        });
        {
            let gnu = h.as_gnu_mut().unwrap();
            gnu.name[..evil_path.len()].copy_from_slice(evil_path.as_bytes());
            if let Some(link) = evil_link {
                gnu.linkname[..link.len()].copy_from_slice(link.as_bytes());
            }
        }
        h.set_cksum();
        let mut b = tar::Builder::new(Vec::new());
        b.append(&h, body).unwrap();
        b.finish().unwrap();
        b.into_inner().unwrap()
    }

    /// Zip-slip: absolute and `..`-escaping entries are hard errors.
    #[test]
    fn zip_slip_entries_rejected() {
        for evil in ["../evil", "a/../../evil", "/etc/passwd"] {
            let layer = raw_path_layer(evil, None);
            let root = tempfile::tempdir().unwrap();
            let mut meta = TreeMetadata::default();
            let err = apply_layer(root.path(), &mut meta, layer.as_slice())
                .expect_err(&format!("{evil} must be rejected"));
            assert!(
                matches!(err, FlattenError::PathEscape(_)),
                "{evil}: got {err}"
            );
            assert!(
                !root.path().parent().unwrap().join("evil").exists(),
                "nothing may land outside the tree"
            );
        }
        // Hardlink TARGETS are resolved host-side — same rule.
        let layer = raw_path_layer("link", Some("../outside"));
        let root = tempfile::tempdir().unwrap();
        let mut meta = TreeMetadata::default();
        assert!(matches!(
            apply_layer(root.path(), &mut meta, layer.as_slice()),
            Err(FlattenError::PathEscape(_))
        ));
    }

    /// Ancestor symlinks resolve in-tree, guest-style (usrmerge): a
    /// lower layer's `bin -> usr/bin` makes an upper layer's `bin/ls`
    /// land at `usr/bin/ls` — through the link, inside the tree.
    #[test]
    fn ancestor_symlink_resolves_in_tree_usrmerge() {
        let lower = LayerBuilder::new()
            .dir("usr", 0o755)
            .dir("usr/bin", 0o755)
            .symlink("bin", "usr/bin")
            .build();
        let upper = LayerBuilder::new().file("bin/ls", 0o755, b"elf").build();
        let (root, meta) = flatten(&[lower, upper]);
        assert!(
            root.path().join("usr/bin/ls").is_file(),
            "written through the link"
        );
        assert!(
            std::fs::symlink_metadata(root.path().join("bin"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "the usrmerge symlink itself survives"
        );
        assert!(
            meta.get("usr/bin/ls").is_some(),
            "sidecar keys the REAL path"
        );
    }

    /// Symlink-traversal zip-slip: a hostile ancestor symlink
    /// (absolute, or `..`-escaping) re-anchors at the tree root —
    /// nothing lands outside the tree.
    #[test]
    fn hostile_ancestor_symlink_cannot_escape_tree() {
        let outside = tempfile::tempdir().unwrap();
        let abs_target = outside.path().join("landing");

        // Absolute target: re-anchored at the tree root.
        let evil = LayerBuilder::new()
            .symlink("evil", abs_target.to_str().unwrap())
            .file("evil/pwn", 0o644, b"x")
            .build();
        let (root, _meta) = flatten(&[evil]);
        assert!(!abs_target.exists(), "host path must be untouched");
        let rebased = root
            .path()
            .join(abs_target.strip_prefix("/").unwrap())
            .join("pwn");
        assert!(rebased.is_file(), "absolute link target re-anchors in-tree");

        // `..`-escaping relative target: clamped at the tree root.
        let evil = LayerBuilder::new()
            .symlink("up", "../../..")
            .file("up/pwn", 0o644, b"y")
            .build();
        let (root, _meta) = flatten(&[evil]);
        assert!(root.path().join("pwn").is_file(), "`..` clamps at the root");
        assert!(!root.path().parent().unwrap().join("pwn").exists());
    }

    /// A symlink loop fails loud instead of spinning.
    #[test]
    fn symlink_loop_fails_loud() {
        let layer = LayerBuilder::new()
            .symlink("a", "b")
            .symlink("b", "a")
            .file("a/inside", 0o644, b"x")
            .build();
        let root = tempfile::tempdir().unwrap();
        let mut meta = TreeMetadata::default();
        let err = apply_layer(root.path(), &mut meta, layer.as_slice())
            .expect_err("loop must be rejected");
        assert!(err.to_string().contains("symlink loop"), "{err}");
    }

    /// A same-layer whiteout must not delete the entry the layer
    /// itself just created (whiteouts target lower layers only).
    #[test]
    fn whiteout_spares_same_layer_entry() {
        let lower = LayerBuilder::new().file("data", 0o644, b"old").build();
        let upper = LayerBuilder::new()
            .file("data", 0o644, b"new")
            .whiteout("", "data")
            .build();
        let (root, _meta) = flatten(&[lower, upper]);
        assert_eq!(std::fs::read(root.path().join("data")).unwrap(), b"new");
    }

    /// Special files (fifo/dev nodes) can't be created unprivileged:
    /// they're recorded, not silently lost, and don't fail the flatten.
    #[test]
    fn special_files_are_collected_not_fatal() {
        let mut h = LayerBuilder::header(0o644, 0, 0, 0, tar::EntryType::Fifo);
        let mut b = tar::Builder::new(Vec::new());
        b.append_data(&mut h, "run/queue.pipe", &[][..]).unwrap();
        b.finish().unwrap();
        let layer = b.into_inner().unwrap();

        let (root, meta) = flatten(&[layer]);
        assert!(!root.path().join("run/queue.pipe").exists());
        assert_eq!(meta.skipped_specials, vec!["run/queue.pipe".to_string()]);
    }

    // ---- parallel-engine tests (ADR 0088 addendum) ----

    /// Flatten with an explicit Flattener at fixed concurrency —
    /// the parallel-path twin of the `flatten()` helper.
    fn flatten_parallel(
        layers: &[Vec<u8>],
        concurrency: usize,
    ) -> (tempfile::TempDir, TreeMetadata) {
        let root = tempfile::tempdir().unwrap();
        let mut meta = TreeMetadata::default();
        let mut f = Flattener::new(root.path(), concurrency).unwrap();
        for layer in layers {
            f.apply_layer(&mut meta, layer.as_slice()).unwrap();
        }
        f.finish(&mut meta).unwrap();
        (root, meta)
    }

    /// Recursive tree fingerprint: (rel, type, mode, mtime, content or
    /// link target, inode group). Inode numbers differ across runs, so
    /// hardlink structure is captured as "which paths share an inode".
    fn tree_fingerprint(root: &Path) -> Vec<String> {
        use std::collections::HashMap;
        fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, u64, String)>) {
            let mut names: Vec<_> = std::fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().path())
                .collect();
            names.sort();
            for p in names {
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
                let m = std::fs::symlink_metadata(&p).unwrap();
                let (kind, body) = if m.file_type().is_symlink() {
                    (
                        "symlink",
                        std::fs::read_link(&p).unwrap().display().to_string(),
                    )
                } else if m.is_dir() {
                    ("dir", String::new())
                } else {
                    ("file", format!("{:x}", md5ish(&std::fs::read(&p).unwrap())))
                };
                let mode = m.permissions().mode() & 0o7777;
                // Dir mtimes are wall-clock at flatten time (child
                // creation bumps them; the pack-stage clamp normalizes
                // later) — only FILE mtimes are flatten's contract.
                let mtime = if m.is_dir() {
                    0
                } else {
                    filetime::FileTime::from_last_modification_time(&m).unix_seconds()
                };
                out.push((
                    format!("{rel}|{kind}|{mode:o}|{mtime}|{body}"),
                    m.ino(),
                    rel,
                ));
                if m.is_dir() {
                    walk(root, &p, out);
                }
            }
        }
        // Cheap stable content hash (no external dep): FNV-1a.
        fn md5ish(bytes: &[u8]) -> u64 {
            let mut h: u64 = 0xcbf29ce484222325;
            for b in bytes {
                h ^= *b as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
            h
        }
        let mut raw = Vec::new();
        walk(root, root, &mut raw);
        // Map inodes to first-seen path so hardlink groups are stable.
        let mut groups: HashMap<u64, String> = HashMap::new();
        raw.iter().for_each(|(_, ino, rel)| {
            groups.entry(*ino).or_insert_with(|| rel.clone());
        });
        raw.into_iter()
            .map(|(line, ino, _)| format!("{line}|group={}", groups[&ino]))
            .collect()
    }

    /// Two entries for the SAME path in one layer: last wins (the
    /// first write lands on an orphaned inode — harmless).
    #[test]
    fn duplicate_path_in_same_layer_last_wins() {
        let layer = LayerBuilder::new()
            .file("app/cfg", 0o600, b"first")
            .file("app/cfg", 0o644, b"second")
            .build();
        let (root, meta) = flatten_parallel(&[layer], 8);
        assert_eq!(
            std::fs::read(root.path().join("app/cfg")).unwrap(),
            b"second"
        );
        assert_eq!(
            std::fs::metadata(root.path().join("app/cfg"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o644
        );
        assert_eq!(meta.get("app/cfg").unwrap().mode, 0o644);
    }

    /// The parallelism determinism gate: many small files + symlinks +
    /// hardlinks + a whiteout + an overwrite, applied twice at
    /// concurrency 8 — identical tree fingerprints (type, mode, mtime,
    /// content, link targets, hardlink grouping).
    #[test]
    fn parallel_apply_tree_is_deterministic() {
        let mut lower = LayerBuilder::new().dir("d", 0o755);
        for i in 0..300 {
            lower = lower.file(&format!("d/f{i:03}"), 0o640, format!("body-{i}").as_bytes());
        }
        let lower = lower
            .file("shared", 0o755, b"tool")
            .hardlink("shared-link", "shared")
            .symlink("alias", "d")
            .build();
        let upper = LayerBuilder::new()
            .whiteout("d", "f000")
            .file("d/f001", 0o600, b"replaced")
            .file("alias/via-link", 0o644, b"through-symlink")
            .build();

        let (root_a, _) = flatten_parallel(&[lower.clone(), upper.clone()], 8);
        let (root_b, _) = flatten_parallel(&[lower, upper], 8);
        let (fa, fb) = (
            tree_fingerprint(root_a.path()),
            tree_fingerprint(root_b.path()),
        );
        assert_eq!(fa, fb, "parallel flatten must be run-to-run identical");
        assert!(
            fa.iter().any(|l| l.starts_with("d/via-link|file")),
            "symlink-dir write must land through the link: {fa:?}"
        );
        assert!(!root_a.path().join("d/f000").exists(), "whiteout applied");
    }

    /// A hardlink created immediately after its target is queued for a
    /// pool write: both names share the inode and show the final bytes.
    #[test]
    fn hardlink_to_in_flight_write_shares_content() {
        let body = vec![0xabu8; 1 << 20]; // 1 MiB: guaranteed pool path
        let layer = LayerBuilder::new()
            .file("blob", 0o644, &body)
            .hardlink("blob-link", "blob")
            .build();
        let (root, _) = flatten_parallel(&[layer], 8);
        let a = std::fs::metadata(root.path().join("blob")).unwrap();
        let b = std::fs::metadata(root.path().join("blob-link")).unwrap();
        assert_eq!(a.ino(), b.ino());
        assert_eq!(std::fs::read(root.path().join("blob-link")).unwrap(), body);
    }

    /// finish() must join the pool: immediately after it returns,
    /// every queued write is on disk with its attrs.
    #[test]
    fn finish_joins_workers_before_return() {
        let mut b = LayerBuilder::new();
        for i in 0..1000 {
            b = b.file(&format!("many/f{i:04}"), 0o600, format!("{i}").as_bytes());
        }
        let (root, _) = flatten_parallel(&[b.build()], 16);
        for i in 0..1000 {
            let p = root.path().join(format!("many/f{i:04}"));
            assert_eq!(std::fs::read(&p).unwrap(), format!("{i}").as_bytes());
            let m = std::fs::metadata(&p).unwrap();
            assert_eq!(m.permissions().mode() & 0o7777, 0o600);
            assert_eq!(
                filetime::FileTime::from_last_modification_time(&m).unix_seconds(),
                946_684_800,
                "futimens must have landed before finish() returned"
            );
        }
    }

    /// A worker failure fails the flatten loudly at finish() with the
    /// failing path in the error.
    #[test]
    fn worker_error_fails_the_flatten() {
        use super::parallel::{WriteJob, WritePool};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("victim");
        std::fs::write(&path, b"x").unwrap();
        // A read-only handle: write_all fails with EBADF-class error.
        let ro = std::fs::File::open(&path).unwrap();
        let pool = WritePool::new(2).unwrap();
        pool.submit(WriteJob {
            file: ro,
            rel: "victim".into(),
            bytes: b"data".to_vec(),
            mode: 0o644,
            mtime: 0,
            xattrs: Vec::new(),
        })
        .unwrap();
        let err = pool.finish().expect_err("read-only fd must fail the pool");
        assert_eq!(err.0, "victim", "failure carries the entry path");
    }

    /// user.* xattrs land via the worker's fd path; refused names are
    /// collected on the sidecar, never fatal (linux-only refusal leg:
    /// `trusted.*` needs CAP_SYS_ADMIN there, while macOS accepts
    /// arbitrary names).
    #[test]
    fn xattrs_apply_via_fd_and_refusals_collected() {
        let mut b = tar::Builder::new(Vec::new());
        b.append_pax_extensions([("SCHILY.xattr.user.engram.test", &b"hello"[..])])
            .unwrap();
        let mut h = LayerBuilder::header(0o644, 0, 0, 4, tar::EntryType::Regular);
        b.append_data(&mut h, "xf", &b"data"[..]).unwrap();
        b.finish().unwrap();
        let layer = b.into_inner().unwrap();

        let (root, meta) = flatten_parallel(&[layer], 4);
        let got = xattr::get(root.path().join("xf"), "user.engram.test").unwrap();
        assert_eq!(got.as_deref(), Some(&b"hello"[..]), "fd xattr must land");
        assert!(
            meta.skipped_xattrs.is_empty(),
            "user.* must not be refused: {:?}",
            meta.skipped_xattrs
        );

        // Refusal leg (linux: trusted.* needs CAP_SYS_ADMIN; macOS
        // accepts arbitrary names, so only assert there's no crash).
        #[cfg(target_os = "linux")]
        if !nix_is_root() {
            let mut b = tar::Builder::new(Vec::new());
            b.append_pax_extensions([("SCHILY.xattr.trusted.engram", &b"x"[..])])
                .unwrap();
            let mut h = LayerBuilder::header(0o644, 0, 0, 1, tar::EntryType::Regular);
            b.append_data(&mut h, "tf", &b"y"[..]).unwrap();
            b.finish().unwrap();
            let (_root, meta) = flatten_parallel(&[b.into_inner().unwrap()], 4);
            assert_eq!(meta.skipped_xattrs.len(), 1, "trusted.* refusal collected");
            assert_eq!(meta.skipped_xattrs[0].name, "trusted.engram");
        }
    }

    #[cfg(target_os = "linux")]
    fn nix_is_root() -> bool {
        std::fs::metadata("/proc/self")
            .map(|m| m.uid() == 0)
            .unwrap_or(false)
    }

    /// The resolve memo must not serve a stale "not a symlink" fact
    /// after a dir is replaced by a symlink: `a/f` written after the
    /// replacement lands under the link target.
    #[test]
    fn resolve_cache_invalidated_by_symlink_replace() {
        let lower = LayerBuilder::new()
            .dir("a", 0o755)
            .file("a/seed", 0o644, b"warm the cache")
            .dir("b", 0o755)
            .build();
        let upper = LayerBuilder::new()
            .whiteout("", "a")
            .symlink("a", "b")
            .file("a/f", 0o644, b"through")
            .build();
        let (root, _) = flatten_parallel(&[lower, upper], 4);
        assert!(
            root.path().join("b/f").is_file(),
            "write after symlink-replace must follow the fresh link"
        );
        assert!(std::fs::symlink_metadata(root.path().join("a"))
            .unwrap()
            .file_type()
            .is_symlink());
    }

    /// apply_ownership under an unprivileged caller: refused chowns
    /// surface as PermissionDenied (the caller downgrades to
    /// "recorded, not applied"); a root caller would get real
    /// ownership. Self-chown (our own uid/gid) is the allowed case and
    /// must succeed.
    #[test]
    fn apply_ownership_self_is_ok_foreign_is_permission_denied() {
        let layer = LayerBuilder::new().file("f", 0o600, b"x").build();
        let (root, mut meta) = flatten(&[layer]);

        // Rewrite the record to OUR uid/gid — chown to self is allowed
        // unprivileged, so this must succeed and re-assert the mode.
        let m = std::fs::metadata(root.path().join("f")).unwrap();
        let (my_uid, my_gid) = (m.uid() as u64, m.gid() as u64);
        meta.entries.insert(
            "f".into(),
            EntryMeta {
                uid: my_uid,
                gid: my_gid,
                mode: 0o600,
            },
        );
        meta.apply_ownership(root.path())
            .expect("self-chown is fine");

        if my_uid != 0 {
            // A foreign uid must refuse with PermissionDenied (the
            // documented unprivileged limitation, downgraded by callers).
            meta.entries.insert(
                "f".into(),
                EntryMeta {
                    uid: 0,
                    gid: 0,
                    mode: 0o600,
                },
            );
            let err = meta
                .apply_ownership(root.path())
                .expect_err("foreign chown must fail unprivileged");
            assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        }
    }
}
