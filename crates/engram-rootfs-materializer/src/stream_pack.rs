//! ADR 0093: the streaming packer — OCI layers → ext4 bytes with no
//! intermediate tree and no image file.
//!
//! Two passes over the layer tars:
//!
//! 1. **Declare** ([`NamespaceBuilder::declare_layer`], headers only):
//!    OCI namespace semantics — sanitize/zip-slip, ancestor-symlink
//!    resolution scoped to the tree (docker `FollowSymlinkInScope`:
//!    the usrmerge case), whiteouts, opaque dirs, overwrite-wins,
//!    hardlinks — applied to an in-memory namespace. No mkext4 calls
//!    yet: image size derives from what survives, and `mkext4`
//!    requires size at construction.
//! 2. **Replay + seal** ([`NamespaceBuilder::seal`]): survivors are
//!    replayed into [`mkext4::FsBuilder`] in declaration order (the
//!    property that makes pass-2 fills ascend) and the layout is
//!    frozen — every metadata byte and zero run is emitted to the
//!    sink immediately.
//! 3. **Fill** ([`SealedImage::fill_layer`], full read): each layer is
//!    re-decoded and every surviving entry's bytes stream to their
//!    final image offsets.
//!
//! The semantics here are a 1:1 port of the retired tree-based
//! flatten engine (`flatten.rs`, ADR 0080/0088); its semantic test
//! matrix carries over against the [`mkext4::reader`] instead of the
//! host filesystem. One deliberate upgrade: device nodes and FIFOs
//! are *declared* (mknod needed root on a real tree; declaring bytes
//! doesn't), so `skipped_specials` retires.

use std::collections::{BTreeMap, HashSet};
use std::io::Read;

use mkext4::build::Timespec;
use mkext4::{FsBuilder, InodeCount, InodeHandle, Layout, Meta, Options, SpecialKind, ROOT};

use crate::flatten::{FlattenError, SkippedXattr};

/// Opaque-dir whiteout marker (`.wh..wh..opq`).
const OPAQUE_MARKER: &str = ".wh..wh..opq";
/// File whiteout prefix (`.wh.<name>`).
const WHITEOUT_PREFIX: &str = ".wh.";

/// Deterministic epoch, re-exported from the ext4 sizing module so the
/// clamp stays a single constant across the codebase.
const EPOCH: i64 = crate::ext4::DETERMINISTIC_EPOCH_SECS as i64;

fn epoch_ts() -> Timespec {
    (EPOCH, 0)
}

/// Clamp a tar mtime to the deterministic epoch (files keep their
/// history below the epoch; anything newer lands AT it — ADR 0036).
fn clamped_ts(tar_mtime: u64) -> Timespec {
    ((tar_mtime as i64).min(EPOCH), 0)
}

// ---------------------------------------------------------------------
// In-memory namespace
// ---------------------------------------------------------------------

type NodeId = usize;
type InoId = usize;

/// Where a regular file's bytes come from at fill time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FillSrc {
    /// `(layer index, entry index within that layer's tar)`.
    Layer(usize, u64),
    /// Index into [`NamespaceBuilder::synthetic`] (the init shim).
    Synthetic(usize),
}

enum InoKind {
    Dir,
    File,
    Symlink(String),
    Special(SpecialKind),
}

/// One inode: hardlinks share it. `nlink` is adapter-side aliveness —
/// a file whose every name is whited-out never reaches mkext4 at all.
struct NsInode {
    kind: InoKind,
    meta: Meta,
    xattrs: Vec<(String, Vec<u8>)>,
    nlink: u32,
    size: u64,
    src: Option<FillSrc>,
}

/// One name in the namespace. Directories carry children here (a
/// directory has exactly one name — hardlinks to dirs are rejected).
struct NsNode {
    ino: InoId,
    children: BTreeMap<String, NodeId>,
    /// Global declaration sequence — replay order. Parent dirs are
    /// always created (implied or explicit) before their children, so
    /// sorting by `decl` yields parents-first.
    decl: u64,
}

/// The declare-pass accumulator.
pub struct NamespaceBuilder {
    nodes: Vec<NsNode>,
    inos: Vec<NsInode>,
    root: NodeId,
    decl_counter: u64,
    layer_idx: usize,
    /// Synthetic file bodies (init shim), filled from memory in pass 2.
    synthetic: Vec<Vec<u8>>,
    /// Xattrs mkext4 can't carry (unsupported namespace) — collected,
    /// never silently lost (parity with the flatten sidecar).
    pub skipped_xattrs: Vec<SkippedXattr>,
}

impl Default for NamespaceBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl NamespaceBuilder {
    pub fn new() -> Self {
        let root_ino = NsInode {
            kind: InoKind::Dir,
            meta: Meta::new(0o755, 0, 0, epoch_ts()),
            xattrs: Vec::new(),
            nlink: 1,
            size: 0,
            src: None,
        };
        let root = NsNode {
            ino: 0,
            children: BTreeMap::new(),
            decl: 0,
        };
        Self {
            nodes: vec![root],
            inos: vec![root_ino],
            root: 0,
            decl_counter: 1,
            layer_idx: 0,
            synthetic: Vec::new(),
            skipped_xattrs: Vec::new(),
        }
    }

    fn next_decl(&mut self) -> u64 {
        let d = self.decl_counter;
        self.decl_counter += 1;
        d
    }

    /// Walk `rel` (already resolved, `a/b/c`) to its parent node,
    /// creating implied directories (0755 root:root, epoch) as needed.
    /// Returns `(parent NodeId, basename)`.
    fn ensure_parent<'a>(&mut self, rel: &'a str) -> Result<(NodeId, &'a str), FlattenError> {
        let (parent_path, name) = match rel.rsplit_once('/') {
            Some((p, n)) => (p, n),
            None => ("", rel),
        };
        let mut cur = self.root;
        if !parent_path.is_empty() {
            for comp in parent_path.split('/') {
                cur = match self.nodes[cur].children.get(comp) {
                    Some(&child)
                        if matches!(self.inos[self.nodes[child].ino].kind, InoKind::Dir) =>
                    {
                        child
                    }
                    Some(&child) => {
                        // A file/symlink where a directory is needed:
                        // the tar told us to write THROUGH it — replace
                        // it with an implied dir (matches
                        // `std::fs::create_dir_all` failing… mirrored
                        // from ensure_parent semantics: create_dir_all
                        // errors on a file component, but the flatten
                        // never hit that because resolve_scoped already
                        // followed symlinks; a plain-file collision is
                        // malformed input).
                        let _ = child;
                        return Err(FlattenError::Malformed(format!(
                            "{rel}: ancestor {comp:?} is not a directory"
                        )));
                    }
                    None => {
                        let d = self.next_decl();
                        let ino = self.push_ino(NsInode {
                            kind: InoKind::Dir,
                            meta: Meta::new(0o755, 0, 0, epoch_ts()),
                            xattrs: Vec::new(),
                            nlink: 1,
                            size: 0,
                            src: None,
                        });
                        let id = self.push_node(NsNode {
                            ino,
                            children: BTreeMap::new(),
                            decl: d,
                        });
                        self.nodes[cur].children.insert(comp.to_string(), id);
                        id
                    }
                };
            }
        }
        Ok((cur, name))
    }

    fn push_ino(&mut self, ino: NsInode) -> InoId {
        self.inos.push(ino);
        self.inos.len() - 1
    }

    fn push_node(&mut self, node: NsNode) -> NodeId {
        self.nodes.push(node);
        self.nodes.len() - 1
    }

    fn lookup(&self, rel: &str) -> Option<NodeId> {
        let mut cur = self.root;
        if rel.is_empty() {
            return Some(cur);
        }
        for comp in rel.split('/') {
            cur = *self.nodes[cur].children.get(comp)?;
        }
        Some(cur)
    }

    /// Remove the name `rel` if present: detach the node and release
    /// every inode reference beneath it. Tolerant of absence (whiteout
    /// of something a lower layer never had).
    fn remove_name(&mut self, rel: &str) {
        let (parent_path, name) = match rel.rsplit_once('/') {
            Some((p, n)) => (p, n),
            None => ("", rel),
        };
        let Some(parent) = self.lookup(parent_path) else {
            return;
        };
        let Some(&node) = self.nodes[parent].children.get(name) else {
            return;
        };
        self.nodes[parent].children.remove(name);
        self.release(node);
    }

    /// Iteratively release a detached subtree's inode references
    /// (worklist, not recursion — hostile tars can nest deep).
    fn release(&mut self, node: NodeId) {
        let mut work = vec![node];
        while let Some(n) = work.pop() {
            let ino = self.nodes[n].ino;
            self.inos[ino].nlink = self.inos[ino].nlink.saturating_sub(1);
            let children: Vec<NodeId> = self.nodes[n].children.values().copied().collect();
            self.nodes[n].children.clear();
            work.extend(children);
        }
    }

    /// Port of the flatten engine's scoped ancestor-symlink resolution
    /// (docker `FollowSymlinkInScope`): absolute targets re-anchor at
    /// the tree root, `..` clamps at the root, the FINAL component is
    /// never followed, bounded hops. Operates on the in-memory tree —
    /// a cursor stack keeps each step O(1).
    fn resolve_scoped(&self, rel: &str) -> Result<String, FlattenError> {
        use std::collections::VecDeque;
        let mut queue: VecDeque<String> = rel.split('/').map(str::to_string).collect();
        let mut resolved: Vec<String> = Vec::new();
        let mut cursor: Vec<NodeId> = Vec::new(); // parallel to `resolved`
        let mut hops = 0u32;
        while let Some(comp) = queue.pop_front() {
            if comp.is_empty() || comp == "." {
                continue;
            }
            if comp == ".." {
                resolved.pop();
                cursor.pop();
                continue;
            }
            if queue.is_empty() {
                resolved.push(comp);
                break;
            }
            let at = cursor.last().copied().unwrap_or(self.root);
            let child = self.nodes[at].children.get(&comp).copied();
            let symlink_target = child.and_then(|c| {
                if let InoKind::Symlink(t) = &self.inos[self.nodes[c].ino].kind {
                    Some(t.clone())
                } else {
                    None
                }
            });
            match symlink_target {
                Some(target) => {
                    hops += 1;
                    if hops > 40 {
                        return Err(FlattenError::Malformed(format!(
                            "symlink loop while resolving {rel}"
                        )));
                    }
                    if target.starts_with('/') {
                        resolved.clear();
                        cursor.clear();
                    }
                    for c in target.split('/').rev() {
                        queue.push_front(c.to_string());
                    }
                }
                None => {
                    // Not a symlink (or nothing there yet — also not a
                    // symlink). Descend if it's a dir; otherwise keep a
                    // lexical cursor (writes through it will fail with
                    // a clear error at ensure_parent, same as flatten).
                    resolved.push(comp.clone());
                    match child {
                        Some(c) if matches!(self.inos[self.nodes[c].ino].kind, InoKind::Dir) => {
                            cursor.push(c)
                        }
                        _ => {
                            // Lexical from here on: remaining ancestors
                            // can't be symlinks (they don't exist).
                            while queue.len() > 1 {
                                let comp = queue.pop_front().unwrap();
                                if comp.is_empty() || comp == "." {
                                    continue;
                                }
                                if comp == ".." {
                                    resolved.pop();
                                } else {
                                    resolved.push(comp);
                                }
                            }
                            if let Some(last) = queue.pop_front() {
                                resolved.push(last);
                            }
                            break;
                        }
                    }
                }
            }
        }
        Ok(resolved.join("/"))
    }

    /// Declare one synthetic file (the init shim): parents implied,
    /// clamped at the epoch, filled from memory in pass 2.
    pub fn declare_synthetic(
        &mut self,
        rel: &str,
        mode: u16,
        body: Vec<u8>,
    ) -> Result<(), FlattenError> {
        let rel = self.resolve_scoped(rel)?;
        self.remove_name(&rel);
        let idx = self.synthetic.len();
        let size = body.len() as u64;
        self.synthetic.push(body);
        let (parent, name) = self.ensure_parent(&rel)?;
        let d = self.next_decl();
        let ino = self.push_ino(NsInode {
            kind: InoKind::File,
            meta: Meta::new(mode, 0, 0, epoch_ts()),
            xattrs: Vec::new(),
            nlink: 1,
            size,
            src: Some(FillSrc::Synthetic(idx)),
        });
        let id = self.push_node(NsNode {
            ino,
            children: BTreeMap::new(),
            decl: d,
        });
        self.nodes[parent].children.insert(name.to_string(), id);
        Ok(())
    }

    /// Apply one layer's tar HEADERS to the namespace. Layers in
    /// manifest order (base first). The reader is consumed but entry
    /// bodies are skipped, not stored.
    pub fn declare_layer<R: Read>(&mut self, layer: R) -> Result<(), FlattenError> {
        let layer_idx = self.layer_idx;
        self.layer_idx += 1;
        let mut archive = tar::Archive::new(layer);
        let mut created_this_layer: HashSet<String> = HashSet::new();
        for (seq, entry) in archive.entries()?.enumerate() {
            let mut entry = entry?;
            let raw_path = entry
                .path()
                .map_err(|e| FlattenError::Malformed(format!("entry path: {e}")))?
                .into_owned();
            let Some(rel) = sanitize(&raw_path)? else {
                continue; // the tree root ("./") — parity: not customized
            };
            let rel = self.resolve_scoped(&rel)?;
            if rel.is_empty() {
                continue;
            }
            let (parent_path, name) = match rel.rsplit_once('/') {
                Some((p, n)) => (p, n),
                None => ("", rel.as_str()),
            };

            // --- whiteouts ---
            if name == OPAQUE_MARKER {
                self.opaque_dir(parent_path, &created_this_layer);
                continue;
            }
            if let Some(hidden) = name.strip_prefix(WHITEOUT_PREFIX) {
                let target = if parent_path.is_empty() {
                    hidden.to_string()
                } else {
                    format!("{parent_path}/{hidden}")
                };
                if !created_this_layer.contains(&target) {
                    self.remove_name(&target);
                }
                continue;
            }

            // --- real entries ---
            let header = entry.header();
            let mode = (header
                .mode()
                .map_err(|e| FlattenError::Malformed(format!("{rel}: mode: {e}")))?
                & 0o7777) as u16;
            let uid = header
                .uid()
                .map_err(|e| FlattenError::Malformed(format!("{rel}: uid: {e}")))?
                as u32;
            let gid = header
                .gid()
                .map_err(|e| FlattenError::Malformed(format!("{rel}: gid: {e}")))?
                as u32;
            let mtime = header.mtime().unwrap_or(0);
            let size = header.size().unwrap_or(0);

            use tar::EntryType;
            match header.entry_type() {
                EntryType::Directory => {
                    let existing = self.lookup(&rel);
                    match existing {
                        Some(n) if matches!(self.inos[self.nodes[n].ino].kind, InoKind::Dir) => {
                            // Merge: refresh metadata, keep children +
                            // decl position. Dirs stamp AT the epoch
                            // (parity with the retired clamp — a dir's
                            // wall mtime always exceeded the epoch).
                            let ino = self.nodes[n].ino;
                            self.inos[ino].meta = Meta::new(mode, uid, gid, epoch_ts());
                            self.apply_xattrs(&mut entry, ino, &rel)?;
                        }
                        _ => {
                            self.remove_name(&rel);
                            let (parent, nm) = self.ensure_parent(&rel)?;
                            let d = self.next_decl();
                            let ino = self.push_ino(NsInode {
                                kind: InoKind::Dir,
                                meta: Meta::new(mode, uid, gid, epoch_ts()),
                                xattrs: Vec::new(),
                                nlink: 1,
                                size: 0,
                                src: None,
                            });
                            self.apply_xattrs(&mut entry, ino, &rel)?;
                            let id = self.push_node(NsNode {
                                ino,
                                children: BTreeMap::new(),
                                decl: d,
                            });
                            self.nodes[parent].children.insert(nm.to_string(), id);
                        }
                    }
                    created_this_layer.insert(rel);
                }
                EntryType::Regular | EntryType::Continuous | EntryType::GNUSparse => {
                    self.remove_name(&rel);
                    let (parent, nm) = self.ensure_parent(&rel)?;
                    let d = self.next_decl();
                    let ino = self.push_ino(NsInode {
                        kind: InoKind::File,
                        meta: Meta::new(mode, uid, gid, clamped_ts(mtime)),
                        xattrs: Vec::new(),
                        nlink: 1,
                        size,
                        src: Some(FillSrc::Layer(layer_idx, seq as u64)),
                    });
                    self.apply_xattrs(&mut entry, ino, &rel)?;
                    let id = self.push_node(NsNode {
                        ino,
                        children: BTreeMap::new(),
                        decl: d,
                    });
                    self.nodes[parent].children.insert(nm.to_string(), id);
                    created_this_layer.insert(rel);
                }
                EntryType::Symlink => {
                    let target = entry
                        .link_name()
                        .map_err(|e| FlattenError::Malformed(format!("{rel}: link name: {e}")))?
                        .ok_or_else(|| {
                            FlattenError::Malformed(format!("{rel}: symlink without target"))
                        })?
                        .to_string_lossy()
                        .into_owned();
                    self.remove_name(&rel);
                    let (parent, nm) = self.ensure_parent(&rel)?;
                    let d = self.next_decl();
                    // Target verbatim; stamped AT the epoch (parity).
                    let ino = self.push_ino(NsInode {
                        kind: InoKind::Symlink(target),
                        meta: Meta::new(mode, uid, gid, epoch_ts()),
                        xattrs: Vec::new(),
                        nlink: 1,
                        size: 0,
                        src: None,
                    });
                    self.apply_xattrs(&mut entry, ino, &rel)?;
                    let id = self.push_node(NsNode {
                        ino,
                        children: BTreeMap::new(),
                        decl: d,
                    });
                    self.nodes[parent].children.insert(nm.to_string(), id);
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
                    let target_rel = sanitize(&raw_target)?.ok_or_else(|| {
                        FlattenError::PathEscape(raw_target.display().to_string())
                    })?;
                    let target_rel = self.resolve_scoped(&target_rel)?;
                    let target_node =
                        self.lookup(&target_rel)
                            .ok_or_else(|| FlattenError::HardlinkTarget {
                                path: rel.clone(),
                                target: target_rel.clone(),
                            })?;
                    let target_ino = self.nodes[target_node].ino;
                    if matches!(self.inos[target_ino].kind, InoKind::Dir) {
                        return Err(FlattenError::Malformed(format!(
                            "{rel}: hardlink to a directory"
                        )));
                    }
                    self.remove_name(&rel);
                    let (parent, nm) = self.ensure_parent(&rel)?;
                    let d = self.next_decl();
                    self.inos[target_ino].nlink += 1;
                    let id = self.push_node(NsNode {
                        ino: target_ino,
                        children: BTreeMap::new(),
                        decl: d,
                    });
                    self.nodes[parent].children.insert(nm.to_string(), id);
                    created_this_layer.insert(rel);
                }
                EntryType::Fifo | EntryType::Char | EntryType::Block => {
                    // ADR 0093 fidelity upgrade: declared, not skipped
                    // (mknod needed root on a real tree).
                    let major = header.device_major().ok().flatten().unwrap_or(0);
                    let minor = header.device_minor().ok().flatten().unwrap_or(0);
                    let kind = match header.entry_type() {
                        EntryType::Char => SpecialKind::Char { major, minor },
                        EntryType::Block => SpecialKind::Block { major, minor },
                        _ => SpecialKind::Fifo,
                    };
                    self.remove_name(&rel);
                    let (parent, nm) = self.ensure_parent(&rel)?;
                    let d = self.next_decl();
                    let ino = self.push_ino(NsInode {
                        kind: InoKind::Special(kind),
                        meta: Meta::new(mode, uid, gid, clamped_ts(mtime)),
                        xattrs: Vec::new(),
                        nlink: 1,
                        size: 0,
                        src: None,
                    });
                    self.apply_xattrs(&mut entry, ino, &rel)?;
                    let id = self.push_node(NsNode {
                        ino,
                        children: BTreeMap::new(),
                        decl: d,
                    });
                    self.nodes[parent].children.insert(nm.to_string(), id);
                    created_this_layer.insert(rel);
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// `.wh..wh..opq`: drop every child this layer didn't create.
    fn opaque_dir(&mut self, dir_rel: &str, created_this_layer: &HashSet<String>) {
        let Some(dir) = self.lookup(dir_rel) else {
            return;
        };
        let names: Vec<String> = self.nodes[dir].children.keys().cloned().collect();
        for name in names {
            let child_rel = if dir_rel.is_empty() {
                name.clone()
            } else {
                format!("{dir_rel}/{name}")
            };
            if !created_this_layer.contains(&child_rel) {
                self.remove_name(&child_rel);
            }
        }
    }

    /// PAX xattrs onto the inode's pending list.
    fn apply_xattrs<R: Read>(
        &mut self,
        entry: &mut tar::Entry<'_, R>,
        ino: InoId,
        rel: &str,
    ) -> Result<(), FlattenError> {
        let Some(exts) = entry.pax_extensions()? else {
            return Ok(());
        };
        for ext in exts {
            let ext = ext?;
            let Ok(key) = ext.key() else { continue };
            let Some(name) = key.strip_prefix("SCHILY.xattr.") else {
                continue;
            };
            self.inos[ino]
                .xattrs
                .push((name.to_string(), ext.value_bytes().to_vec()));
            let _ = rel;
        }
        Ok(())
    }

    /// Survivor counts for the sizing inputs, mirroring the flatten's
    /// hints: file bytes rounded to 4 KiB, one page per name/dir.
    fn sizing(&self) -> (u64 /* entries */, u64 /* bytes */) {
        let mut entries = 0u64;
        let mut bytes = 4096u64; // the root
        let mut seen_ino: HashSet<InoId> = HashSet::new();
        let mut work = vec![self.root];
        while let Some(n) = work.pop() {
            let node = &self.nodes[n];
            if n != self.root {
                entries += 1;
            }
            let ino = &self.inos[node.ino];
            match ino.kind {
                InoKind::Dir => bytes = bytes.saturating_add(4096),
                InoKind::File => {
                    if seen_ino.insert(node.ino) {
                        bytes = bytes.saturating_add((ino.size + 4095) & !4095);
                    } else {
                        bytes = bytes.saturating_add(4096);
                    }
                }
                _ => bytes = bytes.saturating_add(4096),
            }
            work.extend(node.children.values().copied());
        }
        (entries, bytes)
    }

    /// Replay survivors into mkext4 in declaration order, seal, and
    /// return the frozen layout + the per-layer fill plan.
    pub fn seal(self) -> Result<SealedImage, FlattenError> {
        let (entry_count, tree_bytes) = self.sizing();
        let size_bytes = crate::ext4::recommended_size(tree_bytes);
        let inode_count = crate::ext4::inode_count_for(entry_count, size_bytes);

        let mut opts = Options::new(
            size_bytes,
            *uuid::Uuid::parse_str(crate::ext4::DETERMINISTIC_FS_UUID)
                .expect("const uuid parses")
                .as_bytes(),
            EPOCH,
        );
        opts.hash_seed = hash_seed_words();
        opts.inodes = InodeCount::Exact(inode_count.min(u32::MAX as u64) as u32);
        let mut b = FsBuilder::new(opts).map_err(mk_err)?;

        // Names in global declaration order (parents precede children
        // by construction).
        let mut names: Vec<(u64, NodeId, NodeId, String)> = Vec::new(); // (decl, node, parent, name)
        let mut walk: Vec<NodeId> = vec![self.root];
        while let Some(n) = walk.pop() {
            for (name, &child) in &self.nodes[n].children {
                names.push((self.nodes[child].decl, child, n, name.clone()));
                walk.push(child);
            }
        }
        names.sort_unstable_by_key(|(d, ..)| *d);

        let mut handles: Vec<Option<InodeHandle>> = vec![None; self.nodes.len()];
        handles[self.root] = Some(ROOT);
        let mut ino_handle: Vec<Option<InodeHandle>> = vec![None; self.inos.len()];
        ino_handle[0] = Some(ROOT);
        let mut skipped_xattrs = self.skipped_xattrs;
        // fill_plan[layer] = (entry seq → handle,size), ascending seq.
        let mut fill_plan: Vec<BTreeMap<u64, (InodeHandle, u64)>> =
            vec![BTreeMap::new(); self.layer_idx];
        let mut synthetic_plan: Vec<(usize, InodeHandle, u64)> = Vec::new();

        for (_, node, parent, name) in names {
            let parent_h = handles[parent].expect("parents declared first");
            let ino_id = self.nodes[node].ino;
            let ino = &self.inos[ino_id];
            if let Some(existing) = ino_handle[ino_id] {
                // Another name for an already-declared inode.
                b.hardlink(parent_h, &name, existing).map_err(mk_err)?;
                handles[node] = Some(existing);
                continue;
            }
            let h = match &ino.kind {
                InoKind::Dir => b.mkdir(parent_h, &name, ino.meta).map_err(mk_err)?,
                InoKind::File => {
                    let h = b
                        .file(parent_h, &name, ino.meta, ino.size)
                        .map_err(mk_err)?;
                    if ino.size > 0 {
                        match ino.src {
                            Some(FillSrc::Layer(l, s)) => {
                                fill_plan[l].insert(s, (h, ino.size));
                            }
                            Some(FillSrc::Synthetic(i)) => {
                                synthetic_plan.push((i, h, ino.size));
                            }
                            None => unreachable!("regular file without a fill source"),
                        }
                    }
                    h
                }
                InoKind::Symlink(target) => b
                    .symlink(parent_h, &name, target, ino.meta)
                    .map_err(mk_err)?,
                InoKind::Special(kind) => {
                    b.mknod(parent_h, &name, ino.meta, *kind).map_err(mk_err)?
                }
            };
            for (xname, value) in &ino.xattrs {
                if let Err(e) = b.set_xattr(h, xname, value) {
                    skipped_xattrs.push(SkippedXattr {
                        path: name.clone(),
                        name: xname.clone(),
                        error: e.to_string(),
                    });
                }
            }
            handles[node] = Some(h);
            ino_handle[ino_id] = Some(h);
        }

        let layout = b.seal().map_err(mk_err)?;
        Ok(SealedImage {
            layout,
            fill_plan,
            synthetic_plan,
            synthetic: self.synthetic,
            skipped_xattrs,
            entry_count,
        })
    }
}

/// The frozen image: layout + which tar entries feed which inodes.
pub struct SealedImage {
    pub layout: Layout,
    /// Per layer: tar entry seq → (handle, declared size), ascending.
    fill_plan: Vec<BTreeMap<u64, (InodeHandle, u64)>>,
    synthetic_plan: Vec<(usize, InodeHandle, u64)>,
    synthetic: Vec<Vec<u8>>,
    pub skipped_xattrs: Vec<SkippedXattr>,
    pub entry_count: u64,
}

impl SealedImage {
    pub fn image_len(&self) -> u64 {
        self.layout.image_len()
    }

    /// Open the image writer: every metadata byte and zero run is
    /// emitted to `sink` before this returns.
    pub fn begin<S: mkext4::sink::RegionSink>(
        &self,
        sink: S,
    ) -> Result<mkext4::build::ImageWriter<'_, S>, FlattenError> {
        self.layout.writer(sink).map_err(mk_err)
    }

    /// Finish the writer (asserts every declared file was filled).
    pub fn finish_writer<S: mkext4::sink::RegionSink>(
        w: mkext4::build::ImageWriter<'_, S>,
    ) -> Result<(), FlattenError> {
        w.finish().map(|_| ()).map_err(mk_err)
    }

    /// Number of surviving entries a given layer must fill (for
    /// progress accounting).
    pub fn layer_fill_count(&self, layer: usize) -> usize {
        self.fill_plan.get(layer).map(|m| m.len()).unwrap_or(0)
    }

    /// Fill every surviving entry of `layer` from its re-decoded tar.
    pub fn fill_layer<R: Read, S: mkext4::sink::RegionSink>(
        &self,
        w: &mut mkext4::build::ImageWriter<'_, S>,
        layer: usize,
        tar: R,
    ) -> Result<(), FlattenError> {
        let plan = &self.fill_plan[layer];
        if plan.is_empty() {
            return Ok(());
        }
        let mut archive = tar::Archive::new(tar);
        let mut remaining = plan.len();
        for (seq, entry) in archive.entries()?.enumerate() {
            if remaining == 0 {
                break; // nothing left in this layer — skip the tail
            }
            let entry = entry?;
            let Some(&(handle, size)) = plan.get(&(seq as u64)) else {
                continue;
            };
            let mut exact = ExactLen::new(entry, size);
            w.fill(handle, &mut exact).map_err(mk_err)?;
            remaining -= 1;
        }
        if remaining != 0 {
            return Err(FlattenError::Malformed(format!(
                "layer {layer}: {remaining} declared entries missing on refill \
                 (layer changed between passes?)"
            )));
        }
        Ok(())
    }

    /// Fill the synthetic files (init shim) from memory. Call after
    /// the last layer.
    pub fn fill_synthetic<S: mkext4::sink::RegionSink>(
        &self,
        w: &mut mkext4::build::ImageWriter<'_, S>,
    ) -> Result<(), FlattenError> {
        for &(idx, handle, size) in &self.synthetic_plan {
            let body = &self.synthetic[idx];
            debug_assert_eq!(body.len() as u64, size);
            w.fill(handle, &mut &body[..]).map_err(mk_err)?;
        }
        Ok(())
    }
}

/// Adapter: mkext4's RegionSink onto the chunk store's RegionChunker.
pub struct ChunkerSink<'a>(pub &'a mut engram_chunk_store::region::RegionChunker);

impl mkext4::sink::RegionSink for ChunkerSink<'_> {
    fn data(&mut self, offset: u64, bytes: &[u8]) -> std::io::Result<()> {
        self.0.data(offset, bytes)
    }
    fn zeros(&mut self, offset: u64, len: u64) -> std::io::Result<()> {
        self.0.zeros(offset, len)
    }
}

/// Reads exactly `declared` bytes from the inner reader: short input
/// is zero-padded (GNU-sparse tail parity with the tree flatten, which
/// wrote whatever the entry yielded), long input is an error.
struct ExactLen<R> {
    inner: R,
    remaining: u64,
    padding: bool,
}

impl<R: Read> ExactLen<R> {
    fn new(inner: R, declared: u64) -> Self {
        Self {
            inner,
            remaining: declared,
            padding: false,
        }
    }
}

impl<R: Read> Read for ExactLen<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            return Ok(0);
        }
        let want = buf.len().min(self.remaining as usize);
        if self.padding {
            buf[..want].fill(0);
            self.remaining -= want as u64;
            return Ok(want);
        }
        let n = self.inner.read(&mut buf[..want])?;
        if n == 0 {
            self.padding = true;
            tracing::warn!(
                short_by = self.remaining,
                "tar entry shorter than its declared size; zero-padding (GNU sparse?)"
            );
            return self.read(buf);
        }
        self.remaining -= n as u64;
        Ok(n)
    }
}

fn mk_err(e: impl std::fmt::Display) -> FlattenError {
    FlattenError::Io(std::io::Error::other(e.to_string()))
}

/// `mke2fs -E hash_seed=<uuid>` semantics: the UUID's 16 bytes as four
/// LE u32 words — byte-compatible with what the retired pack pinned.
fn hash_seed_words() -> [u32; 4] {
    let u = uuid::Uuid::parse_str(crate::ext4::DETERMINISTIC_HASH_SEED).expect("const uuid parses");
    let b = u.as_bytes();
    [
        u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
        u32::from_le_bytes([b[8], b[9], b[10], b[11]]),
        u32::from_le_bytes([b[12], b[13], b[14], b[15]]),
    ]
}

/// Normalize a tar path to `a/b/c`. `Ok(None)` = the tree root.
/// Rejects absolute paths and `..` (zip-slip). (Moved from the retired
/// flatten engine, semantics identical.)
fn sanitize(path: &std::path::Path) -> Result<Option<String>, FlattenError> {
    use std::path::Component;
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

#[cfg(test)]
mod tests {
    use super::*;
    use mkext4::reader::Fs;

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
            h.set_mtime(946_684_800); // 2000-01-01 — BELOW the epoch
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

        fn chardev(mut self, path: &str, major: u32, minor: u32) -> Self {
            let mut h = Self::header(0o600, 0, 0, 0, tar::EntryType::Char);
            h.set_device_major(major).unwrap();
            h.set_device_minor(minor).unwrap();
            self.tar.append_data(&mut h, path, &[][..]).unwrap();
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

    /// Declare + seal + fill + finish; every image is checksum-verified
    /// by the mkext4 reader before any assertion runs.
    fn build_image(layers: &[Vec<u8>]) -> Vec<u8> {
        build_image_with(layers, |_| {})
    }

    fn build_image_with(layers: &[Vec<u8>], tweak: impl FnOnce(&mut NamespaceBuilder)) -> Vec<u8> {
        let mut ns = NamespaceBuilder::new();
        for l in layers {
            ns.declare_layer(&l[..]).unwrap();
        }
        tweak(&mut ns);
        let sealed = ns.seal().unwrap();
        let mut sink = mkext4::sink::VecSink::default();
        let mut w = sealed.layout.writer(&mut sink).unwrap();
        for (i, l) in layers.iter().enumerate() {
            sealed.fill_layer(&mut w, i, &l[..]).unwrap();
        }
        sealed.fill_synthetic(&mut w).unwrap();
        w.finish().unwrap();
        sink.buf
    }

    fn open(image: &[u8]) -> Fs<&[u8]> {
        let fs = Fs::open(image).unwrap();
        let issues = fs.verify().unwrap();
        assert!(issues.is_empty(), "image must verify clean: {issues:?}");
        fs
    }

    #[test]
    fn file_whiteout_removes_lower_entry() {
        let lower = LayerBuilder::new()
            .dir("etc", 0o755)
            .file("etc/removed.conf", 0o644, b"bye")
            .file("etc/kept.conf", 0o644, b"hi")
            .build();
        let upper = LayerBuilder::new().whiteout("etc", "removed.conf").build();
        let img = build_image(&[lower, upper]);
        let fs = open(&img);
        assert!(fs.resolve("/etc/removed.conf").is_err());
        let kept = fs.resolve("/etc/kept.conf").unwrap();
        assert_eq!(fs.read_file(kept).unwrap(), b"hi");
    }

    #[test]
    fn file_whiteout_removes_lower_directory_subtree() {
        let lower = LayerBuilder::new()
            .dir("opt", 0o755)
            .dir("opt/tool", 0o755)
            .file("opt/tool/bin", 0o755, b"x")
            .build();
        let upper = LayerBuilder::new().whiteout("opt", "tool").build();
        let img = build_image(&[lower, upper]);
        let fs = open(&img);
        assert!(fs.resolve("/opt/tool").is_err());
        assert!(fs.resolve("/opt").is_ok());
    }

    #[test]
    fn opaque_dir_drops_lower_children_keeps_uppers() {
        let lower = LayerBuilder::new()
            .dir("cfg", 0o755)
            .file("cfg/lower-a", 0o644, b"a")
            .file("cfg/lower-b", 0o644, b"b")
            .build();
        let upper = LayerBuilder::new()
            .dir("cfg", 0o755)
            .file("cfg/upper", 0o644, b"u") // BEFORE the marker on purpose
            .opaque("cfg")
            .build();
        let img = build_image(&[lower, upper]);
        let fs = open(&img);
        assert!(fs.resolve("/cfg/lower-a").is_err());
        assert!(fs.resolve("/cfg/lower-b").is_err());
        let upper = fs.resolve("/cfg/upper").unwrap();
        assert_eq!(fs.read_file(upper).unwrap(), b"u");
    }

    #[test]
    fn hardlink_pair_shares_inode() {
        let layer = LayerBuilder::new()
            .file("bin/busybox", 0o755, b"#!x")
            .hardlink("bin/sh", "bin/busybox")
            .build();
        let img = build_image(&[layer]);
        let fs = open(&img);
        let a = fs.resolve("/bin/busybox").unwrap();
        let b = fs.resolve("/bin/sh").unwrap();
        assert_eq!(a, b, "hardlink must share the inode");
        assert_eq!(fs.inode(a).unwrap().links_count, 2);
    }

    /// The bytes survive even when the ORIGINAL name is whited out and
    /// only the hardlink alias remains (fill must key on the inode, not
    /// the name).
    #[test]
    fn hardlink_survives_original_name_removal() {
        let lower = LayerBuilder::new()
            .file("data/orig", 0o644, b"payload")
            .hardlink("data/alias", "data/orig")
            .build();
        let upper = LayerBuilder::new().whiteout("data", "orig").build();
        let img = build_image(&[lower, upper]);
        let fs = open(&img);
        assert!(fs.resolve("/data/orig").is_err());
        let alias = fs.resolve("/data/alias").unwrap();
        assert_eq!(fs.read_file(alias).unwrap(), b"payload");
        assert_eq!(fs.inode(alias).unwrap().links_count, 1);
    }

    #[test]
    fn setuid_bit_preserved() {
        let layer = LayerBuilder::new()
            .file("usr/bin/sudo", 0o4755, b"elf")
            .build();
        let img = build_image(&[layer]);
        let fs = open(&img);
        let ino = fs.resolve("/usr/bin/sudo").unwrap();
        assert_eq!(fs.inode(ino).unwrap().mode & 0o7777, 0o4755);
    }

    /// usrmerge: a lower-layer `bin -> usr/bin` symlink routes upper
    /// writes into usr/bin (FollowSymlinkInScope).
    #[test]
    fn ancestor_symlink_resolves_in_scope() {
        let lower = LayerBuilder::new()
            .dir("usr", 0o755)
            .dir("usr/bin", 0o755)
            .symlink("bin", "usr/bin")
            .build();
        let upper = LayerBuilder::new().file("bin/ls", 0o755, b"ls!").build();
        let img = build_image(&[lower, upper]);
        let fs = open(&img);
        let ino = fs.resolve("/usr/bin/ls").unwrap();
        assert_eq!(fs.read_file(ino).unwrap(), b"ls!");
        // And /bin stays a symlink.
        let bin = fs.resolve("/bin").unwrap();
        assert_eq!(fs.symlink_target(bin).unwrap(), b"usr/bin");
    }

    /// An ABSOLUTE symlink target re-anchors at the image root, never
    /// the host's / (the symlink half of zip-slip).
    #[test]
    fn absolute_symlink_target_reanchors_at_root() {
        let lower = LayerBuilder::new()
            .dir("usr", 0o755)
            .dir("usr/lib", 0o755)
            .symlink("lib", "/usr/lib")
            .build();
        let upper = LayerBuilder::new()
            .file("lib/libc.so", 0o644, b"so")
            .build();
        let img = build_image(&[lower, upper]);
        let fs = open(&img);
        assert!(fs.resolve("/usr/lib/libc.so").is_ok());
    }

    /// Raw-header layer builder for hostile paths the tar crate's
    /// Builder refuses to write (same trick as the flatten tests).
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

    #[test]
    fn zip_slip_paths_rejected() {
        for evil in ["../evil", "a/../../evil", "/etc/passwd"] {
            let layer = raw_path_layer(evil, None);
            let mut ns = NamespaceBuilder::new();
            let err = ns
                .declare_layer(&layer[..])
                .expect_err(&format!("{evil} must be rejected"));
            assert!(matches!(err, FlattenError::PathEscape(_)), "{evil}: {err}");
        }
        // Hardlink TARGETS are resolved namespace-side — same rule.
        let layer = raw_path_layer("link", Some("../outside"));
        let mut ns = NamespaceBuilder::new();
        assert!(matches!(
            ns.declare_layer(&layer[..]),
            Err(FlattenError::PathEscape(_))
        ));
    }

    #[test]
    fn hardlink_to_missing_target_rejected() {
        let layer = LayerBuilder::new().hardlink("a", "nonexistent").build();
        let mut ns = NamespaceBuilder::new();
        assert!(matches!(
            ns.declare_layer(&layer[..]),
            Err(FlattenError::HardlinkTarget { .. })
        ));
    }

    /// Later layer (and later same-layer entry) wins; the loser's bytes
    /// never reach the image.
    #[test]
    fn overwrite_wins_across_and_within_layers() {
        let lower = LayerBuilder::new()
            .file("app/config", 0o644, b"old-old-old")
            .build();
        let upper = LayerBuilder::new()
            .file("app/config", 0o600, b"mid")
            .file("app/config", 0o640, b"new")
            .build();
        let img = build_image(&[lower, upper]);
        let fs = open(&img);
        let ino = fs.resolve("/app/config").unwrap();
        assert_eq!(fs.read_file(ino).unwrap(), b"new");
        assert_eq!(fs.inode(ino).unwrap().mode & 0o7777, 0o640);
    }

    /// Whiteout in the SAME layer as a recreation: the layer's own
    /// entry is immune (created_this_layer).
    #[test]
    fn whiteout_spares_same_layer_creation() {
        let lower = LayerBuilder::new().file("x", 0o644, b"lower").build();
        let upper = LayerBuilder::new()
            .file("x", 0o644, b"upper")
            .whiteout("", "x")
            .build();
        let img = build_image(&[lower, upper]);
        let fs = open(&img);
        let ino = fs.resolve("/x").unwrap();
        assert_eq!(fs.read_file(ino).unwrap(), b"upper");
    }

    /// File mtimes below the epoch survive; dirs and symlinks stamp AT
    /// the epoch (parity with the retired clamp walk).
    #[test]
    fn mtime_clamp_parity() {
        let layer = LayerBuilder::new()
            .dir("d", 0o755)
            .file("d/f", 0o644, b"data")
            .symlink("d/s", "f")
            .build();
        let img = build_image(&[layer]);
        let fs = open(&img);
        let f = fs.inode(fs.resolve("/d/f").unwrap()).unwrap();
        assert_eq!(f.mtime as i64, 946_684_800, "file keeps its history");
        let d = fs.inode(fs.resolve("/d").unwrap()).unwrap();
        assert_eq!(d.mtime as i64, EPOCH, "dir stamps AT the epoch");
        let s = fs.inode(fs.resolve("/d/s").unwrap()).unwrap();
        assert_eq!(s.mtime as i64, EPOCH, "symlink stamps AT the epoch");
    }

    /// ADR 0093 fidelity upgrade: device nodes are declared.
    #[test]
    fn char_device_declared() {
        let layer = LayerBuilder::new().chardev("dev/null", 1, 3).build();
        let img = build_image(&[layer]);
        let fs = open(&img);
        let ino = fs.resolve("/dev/null").unwrap();
        let inode = fs.inode(ino).unwrap();
        assert_eq!(inode.mode & 0xF000, 0x2000, "S_IFCHR");
    }

    /// The synthetic init shim declares + fills from memory.
    #[test]
    fn synthetic_file_declares_and_fills() {
        let layer = LayerBuilder::new().dir("sbin", 0o755).build();
        let img = build_image_with(&[layer], |ns| {
            ns.declare_synthetic("sbin/engram-init", 0o755, b"#!/bin/sh\n".to_vec())
                .unwrap();
        });
        let fs = open(&img);
        let ino = fs.resolve("/sbin/engram-init").unwrap();
        assert_eq!(fs.read_file(ino).unwrap(), b"#!/bin/sh\n");
        assert_eq!(fs.inode(ino).unwrap().mode & 0o7777, 0o755);
    }

    /// Same layers twice ⇒ byte-identical images (the property every
    /// chunk-dedup and cold-base-reuse key hangs off).
    #[test]
    fn double_build_is_byte_identical() {
        let layers = vec![
            LayerBuilder::new()
                .dir("usr", 0o755)
                .file("usr/app", 0o755, b"binary")
                .symlink("app", "usr/app")
                .build(),
            LayerBuilder::new()
                .file("usr/app", 0o755, b"binary-v2")
                .file("etc/conf", 0o644, b"k=v")
                .build(),
        ];
        let a = build_image(&layers);
        let b = build_image(&layers);
        assert_eq!(a, b, "double build must be byte-identical");
    }

    /// Sensitivity: one byte of content changes the image.
    #[test]
    fn content_change_changes_image() {
        let mk = |body: &[u8]| vec![LayerBuilder::new().file("f", 0o644, body).build()];
        assert_ne!(build_image(&mk(b"aaaa")), build_image(&mk(b"aaab")));
    }
}
