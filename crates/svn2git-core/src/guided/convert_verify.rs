use super::*;
use std::collections::BTreeMap;

fn mismatch(msg: impl Into<String>) -> Svn2GitError {
    Svn2GitError::Verification(msg.into())
}

/// Compare fresh SVN logs against the objects reached by walking the actual ref.
/// The commit cache used for replay is deliberately unavailable here.
pub(super) async fn selected(
    svn: &Svn<'_>,
    manifest: &Manifest,
    item: &ScopeRef,
    workdir: &Path,
    authors: &[AuthorEntry],
    progress: &(dyn Fn(&str, &str) + Sync),
) -> Result<VerifiedRef> {
    let target = pinned(manifest, item)?;
    let full_ref = item.full_ref();
    let oid = git_line(workdir, &["rev-parse", "--verify", &full_ref], None).await?;
    let commit_oid = git_line(
        workdir,
        &["rev-parse", "--verify", &format!("{oid}^{{commit}}")],
        None,
    )
    .await?;
    let expected = svn.history(&target, manifest.revision, true).await?;
    let annotation = annotation(svn, manifest, item, &target, &expected, authors).await?;
    let mut annotated = 0;
    if let Some(annotation) = annotation {
        if oid == commit_oid {
            return Err(mismatch(format!(
                "{full_ref} · 缺少记录 SVN 复制修订的标签对象"
            )));
        }
        let raw = utf8(git(workdir, &["cat-file", "tag", &oid], None).await?)?;
        let expected_tag = format!(
            "object {commit_oid}\ntype commit\ntag {}\ntagger {}\n\n{}",
            item.name, annotation.tagger, annotation.body
        );
        if raw != expected_tag {
            return Err(mismatch(format!(
                "{full_ref} · 复制标签 {oid} 的来源、修订或元数据不匹配"
            )));
        }
        annotated = annotation.revisions;
    } else if oid != commit_oid {
        return Err(mismatch(format!(
            "{full_ref} · SVN 路径日志与标签对象类型不匹配"
        )));
    }
    let mut next = Some(commit_oid.clone());
    for (index, rev) in expected.iter().enumerate().skip(annotated) {
        let context = format!("{full_ref} · SVN r{}", rev.rev);
        progress(
            "verify",
            &format!("{context} · 核对历史 {}/{}", index + 1, expected.len()),
        );
        let current = next
            .take()
            .ok_or_else(|| mismatch(format!("{context} 缺少可达的历史提交")))?;
        let location = svn.location(&target, rev.rev, &manifest.uuid).await?;
        let signature = rev.signature(authors)?;
        let raw = utf8(git(workdir, &["cat-file", "commit", &current], None).await?)?;
        let (headers, body) = raw
            .split_once("\n\n")
            .ok_or_else(|| mismatch(format!("{context} · 无效 Git 对象 {current}")))?;
        let header = |key: &str| headers.lines().find_map(|s| s.strip_prefix(key));
        if header("svn-revision ") != Some(rev.rev.to_string().as_str()) {
            return Err(mismatch(format!(
                "{context} · 预计 r{}，实际提交 {current} 对应 r{}",
                rev.rev,
                header("svn-revision ").unwrap_or("?")
            )));
        }
        for (field, expected_value) in [
            ("author ", signature.as_str()),
            ("committer ", signature.as_str()),
            (
                "svn-source ",
                &format!("{} {}", manifest.uuid, location.relative),
            ),
            ("svn-date ", &rev.date),
        ] {
            if header(field) != Some(expected_value) {
                return Err(mismatch(format!(
                    "{context} · {field}预计 {expected_value:?}，实际 {:?}",
                    header(field)
                )));
            }
        }
        if body != rev.message {
            return Err(mismatch(format!(
                "{context} · 预计提交说明 {:?}，实际 {body:?}",
                rev.message
            )));
        }
        let mut parents = headers.lines().filter_map(|s| s.strip_prefix("parent "));
        next = parents.next().map(str::to_owned);
        if parents.next().is_some() {
            return Err(mismatch(format!("{context} 包含预期之外的父提交")));
        }
    }
    if let Some(extra) = next {
        return Err(mismatch(format!("{full_ref} 包含多余历史 {extra}")));
    }
    progress(
        "verify",
        &format!("{full_ref} · 正在读取 r{} 的版本化文件", manifest.revision),
    );
    let files = verify_files(
        svn,
        manifest,
        &target,
        workdir,
        &commit_oid,
        &full_ref,
        progress,
    )
    .await?;
    Ok(VerifiedRef {
        full_ref,
        oid,
        commit_oid,
        files,
        revisions: expected.len() as u64,
    })
}

async fn verify_files(
    svn: &Svn<'_>,
    manifest: &Manifest,
    target: &str,
    workdir: &Path,
    commit: &str,
    full_ref: &str,
    progress: &(dyn Fn(&str, &str) + Sync),
) -> Result<u64> {
    let revision = manifest.revision.to_string();
    let base = svn
        .location(target, manifest.revision, &manifest.uuid)
        .await?;
    // Get paths and modes straight from the server. A fresh working copy supplies
    // original file bytes, including symlinks, unexpanded keywords and EOLs.
    let listing = svn
        .run(&["list", "-R", "--xml", "-r", &revision, target], None)
        .await?;
    let doc = xml(&listing)?;
    let mut paths = Vec::new();
    for item in doc
        .descendants()
        .filter(|n| n.has_tag_name("entry") && n.attribute("kind") == Some("file"))
    {
        paths.push(child_text(item, "name")?.to_owned());
    }
    let props = tree::properties(
        &svn.run(&["proplist", "-R", "--xml", "-r", &revision, target], None)
            .await?,
    )?;
    let prefix = format!("{}/", base.url.trim_end_matches('/'));
    let mut modes = BTreeMap::new();
    for (url, names) in props {
        if url == base.url {
            continue;
        }
        let relative = url
            .strip_prefix(&prefix)
            .ok_or_else(|| mismatch(format!("SVN 属性路径 {url} 超出 {prefix}")))?;
        let path = percent_encoding::percent_decode_str(relative)
            .decode_utf8()
            .map_err(tree::svn_error)?;
        modes.insert(path.into_owned(), tree::mode(Some(&names)));
    }
    let temp = tempfile::tempdir()?;
    let wc = temp.path().join("verify");
    tree::checkout(svn, &wc, &base.url, manifest.revision).await?;
    let checksums = tree::pristines(svn, &wc, &base).await?;
    let listing = git(
        workdir,
        &["ls-tree", "-r", "-z", "--full-tree", commit],
        None,
    )
    .await?;
    let mut actual = BTreeMap::new();
    for row in listing.split(|c| *c == 0).filter(|s| !s.is_empty()) {
        let row = std::str::from_utf8(row).map_err(|e| mismatch(e.to_string()))?;
        let (header, path) = row
            .split_once('\t')
            .ok_or_else(|| mismatch("无效的 Git 文件树"))?;
        let fields: Vec<_> = header.split(' ').collect();
        if fields.len() != 3 || fields[1] != "blob" {
            return Err(mismatch(format!("{full_ref} · {path} 的对象类型有误")));
        }
        actual.insert(
            path.to_owned(),
            (fields[0].to_owned(), fields[2].to_owned()),
        );
    }
    let repo = gix::open(workdir).map_err(|e| mismatch(e.to_string()))?;
    let count = actual.len() as u64;
    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        let mode = modes.get(&path).copied().unwrap_or("100644");
        let sha = checksums
            .get(&path)
            .ok_or_else(|| mismatch(format!("{full_ref} · SVN 原始文件缺失 {path}")))?;
        files.push((path, mode, tree::pristine_path(&wc, sha)));
    }
    let mut index = 0;
    transform::apply(manifest, workdir, files, false, |path, mode, content| {
        index += 1;
        if index % 128 == 1 || index == count {
            progress(
                "verify",
                &format!("{full_ref} · 核对文件 {index}/{count} · {path}"),
            );
        }
        compare_file(
            &repo,
            &mut actual,
            full_ref,
            path,
            mode,
            &content.bytes(path, mode)?,
        )
    })?;
    if let Some(extra) = actual.keys().next() {
        return Err(mismatch(format!("{full_ref} · Git 多出文件 {extra}")));
    }
    Ok(count)
}

fn compare_file(
    repo: &gix::Repository,
    actual: &mut BTreeMap<String, (String, String)>,
    full_ref: &str,
    path: &str,
    expected_mode: &str,
    bytes: &[u8],
) -> Result<()> {
    let (mode, oid) = actual
        .remove(path)
        .ok_or_else(|| mismatch(format!("{full_ref} · Git 缺少文件 {path}")))?;
    if mode != expected_mode {
        return Err(mismatch(format!(
            "{full_ref} · {path} 预计模式 {expected_mode}，实际 {mode}"
        )));
    }
    let id = gix::ObjectId::from_hex(oid.as_bytes()).map_err(|e| mismatch(e.to_string()))?;
    let object = repo.find_object(id).map_err(|e| mismatch(e.to_string()))?;
    if object.kind != gix::objs::Kind::Blob || object.data != bytes {
        use sha2::{Digest, Sha256};
        return Err(mismatch(format!(
            "{full_ref} · {path} 的内容与 SVN 不符，预计 SHA-256 {}（{} 字节），Git 对象 {oid} 的 SHA-256 {}（{} 字节）",
            hex::encode(Sha256::digest(bytes)),
            bytes.len(),
            hex::encode(Sha256::digest(&object.data)),
            object.data.len()
        )));
    }
    Ok(())
}
