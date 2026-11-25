use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::config::{LayoutMode, TaskConfig, TreeVerificationReport};
use crate::credential::ResolvedSecret;
use crate::error::{Result, SvnHistoryToGitError};
use crate::process::{run_command, LineCallback, ProcessRegistry};
use crate::svn::build_svn_command;

/// Byte-level Tree Consistency Verifier:
/// Exports the target SVN revision into a temporary directory, hashes every file (SHA-256),
/// and compares it against Git's `HEAD` tree read directly via `gix` (zero extra git checkout).
pub async fn verify_svn_vs_git_tree(
    registry: &ProcessRegistry,
    task_id: i64,
    config: &TaskConfig,
    workdir: &Path,
    svn_revision: u64,
    secret: Option<&ResolvedSecret>,
    svn_config_dir: Option<&Path>,
    on_line: Option<LineCallback>,
) -> Result<TreeVerificationReport> {
    let temp_dir = tempfile::tempdir()?;
    let export_dir = temp_dir.path().join("svn_export");

    let mut target_url = config.effective_svn_url();
    if config.layout_mode == LayoutMode::Standard {
        target_url = format!("{}/trunk", target_url.trim_end_matches('/'));
    } else if config.layout_mode == LayoutMode::Custom && !config.custom_layout.trunk.is_empty() {
        target_url = format!(
            "{}/{}",
            target_url.trim_end_matches('/'),
            config.custom_layout.trunk.trim_matches('/')
        );
    }

    let mut args = vec!["export".into(), "--force".into(), "--ignore-externals".into()];
    if svn_revision > 0 {
        args.push("-r".into());
        args.push(svn_revision.to_string());
    }
    args.push(target_url);
    args.push(export_dir.to_string_lossy().to_string());

    let spec = build_svn_command("svn", args, config, secret, svn_config_dir)
        .timeout(300)
        .log_output(false);
    run_command(registry, task_id, spec, on_line).await?;

    let svn_hashes = hash_directory_recursive(&export_dir)?;
    let (git_ref, git_hashes) = hash_git_head_tree_gix(workdir)?;

    let mut missing_in_git = Vec::new();
    let mut extra_in_git = Vec::new();
    let mut hash_mismatches = Vec::new();

    for (path, svn_hash) in &svn_hashes {
        match git_hashes.get(path) {
            None => missing_in_git.push(path.clone()),
            Some(git_hash) if git_hash != svn_hash => {
                // If LFS pointer replaced the blob, skip false positive mismatch
                if !is_lfs_pointer_blob(workdir, path) {
                    hash_mismatches.push(path.clone());
                }
            }
            Some(_) => {}
        }
    }

    for path in git_hashes.keys() {
        // Ignore generated .gitignore / .gitattributes
        if path == ".gitignore" || path == ".gitattributes" {
            continue;
        }
        if !svn_hashes.contains_key(path) {
            extra_in_git.push(path.clone());
        }
    }

    let matched = missing_in_git.is_empty() && extra_in_git.is_empty() && hash_mismatches.is_empty();
    Ok(TreeVerificationReport {
        matched,
        checked_files: svn_hashes.len(),
        missing_in_git,
        extra_in_git,
        hash_mismatches,
        svn_revision,
        git_ref,
    })
}

fn hash_directory_recursive(root: &Path) -> Result<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    if !root.exists() {
        return Ok(map);
    }
    walk_dir(root, root, &mut map)?;
    Ok(map)
}

fn walk_dir(root: &Path, current: &Path, out: &mut BTreeMap<String, String>) -> Result<()> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let meta = entry.metadata()?;
        if meta.is_dir() {
            walk_dir(root, &path, out)?;
        } else if meta.is_file() {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let bytes = fs::read(&path)?;
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            out.insert(rel, hex::encode(hasher.finalize()));
        }
    }
    Ok(())
}

fn hash_git_head_tree_gix(workdir: &Path) -> Result<(String, BTreeMap<String, String>)> {
    let repo = gix::open(workdir).map_err(|e| SvnHistoryToGitError::Git(format!("gix open failed: {e}")))?;
    let head_name = repo
        .head_name()
        .ok()
        .flatten()
        .map(|n| n.shorten().to_string())
        .unwrap_or_else(|| "HEAD".to_string());
    let head_commit = repo
        .head_commit()
        .map_err(|e| SvnHistoryToGitError::Git(format!("no HEAD commit: {e}")))?;
    let tree = head_commit
        .tree()
        .map_err(|e| SvnHistoryToGitError::Git(format!("failed to read HEAD tree: {e}")))?;

    let mut map = BTreeMap::new();
    walk_gix_tree(&repo, &tree, "", &mut map)?;
    Ok((head_name, map))
}

fn walk_gix_tree(
    repo: &gix::Repository,
    tree: &gix::Tree<'_>,
    prefix: &str,
    out: &mut BTreeMap<String, String>,
) -> Result<()> {
    for entry_res in tree.iter() {
        let entry = entry_res.map_err(|e| SvnHistoryToGitError::Git(e.to_string()))?;
        let name = entry.filename().to_string();
        let full_path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let mode = entry.mode();
        if mode.is_tree() {
            let sub_obj = repo
                .find_object(entry.oid())
                .map_err(|e| SvnHistoryToGitError::Git(e.to_string()))?;
            let sub_tree = sub_obj.into_tree();
            walk_gix_tree(repo, &sub_tree, &full_path, out)?;
        } else if mode.is_blob() {
            let blob = repo
                .find_object(entry.oid())
                .map_err(|e| SvnHistoryToGitError::Git(e.to_string()))?;
            let mut hasher = Sha256::new();
            hasher.update(&blob.data);
            out.insert(full_path, hex::encode(hasher.finalize()));
        }
    }
    Ok(())
}

fn is_lfs_pointer_blob(workdir: &Path, rel_path: &str) -> bool {
    let Ok(repo) = gix::open(workdir) else {
        return false;
    };
    let Ok(head) = repo.head_commit() else {
        return false;
    };
    let Ok(tree) = head.tree() else {
        return false;
    };
    let Ok(Some(entry)) = tree.lookup_entry_by_path(rel_path) else {
        return false;
    };
    if let Ok(obj) = repo.find_object(entry.oid()) {
        return obj.data.starts_with(b"version https://git-lfs.github.com/spec/v1\n");
    }
    false
}
