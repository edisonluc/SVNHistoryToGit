use super::*;
use gix::objs::tree::{Entry, EntryKind};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

pub(super) async fn checkout(svn: &Svn<'_>, wc: &Path, url: &str, rev: u64) -> Result<()> {
    let target = format!("{url}@{rev}");
    let revision = rev.to_string();
    if wc.exists() {
        svn.run(
            &[
                "switch",
                "--ignore-ancestry",
                "--ignore-externals",
                "-q",
                "-r",
                &revision,
                &target,
                ".",
            ],
            Some(wc),
        )
        .await?;
    } else {
        let path = wc
            .to_str()
            .ok_or_else(|| Svn2GitError::Svn("工作目录编码无效".into()))?;
        svn.run(
            &[
                "checkout",
                "--ignore-externals",
                "-q",
                "-r",
                &revision,
                &target,
                path,
            ],
            None,
        )
        .await?;
    }
    Ok(())
}

pub(super) fn svn_error(message: impl ToString) -> Svn2GitError {
    Svn2GitError::Svn(message.to_string())
}
fn git_error(message: impl ToString) -> Svn2GitError {
    Svn2GitError::Git(message.to_string())
}

fn local_path(path: &str) -> String {
    if cfg!(windows) {
        path.replace('\\', "/")
    } else {
        path.to_owned()
    }
}

pub(super) async fn pristines(
    svn: &Svn<'_>,
    wc: &Path,
    location: &Location,
) -> Result<BTreeMap<String, String>> {
    let data = svn.run(&["info", "--xml", "-R", "."], Some(wc)).await?;
    let doc = xml(&data)?;
    let revision = location.revision.to_string();
    let root = doc
        .descendants()
        .find(|n| n.has_tag_name("entry") && n.attribute("path") == Some("."))
        .ok_or_else(|| svn_error("工作副本缺少根目录"))?;
    if child_text(root, "relative-url")? != location.relative
        || doc.descendants().any(|n| {
            n.has_tag_name("tree-conflict")
                || (n.has_tag_name("entry") && n.attribute("revision") != Some(revision.as_str()))
        })
    {
        return Err(svn_error(format!(
            "工作副本未完整切换至 {}@{}",
            location.relative, revision
        )));
    }
    let mut files = BTreeMap::new();
    for entry in doc
        .descendants()
        .filter(|n| n.has_tag_name("entry") && n.attribute("kind") == Some("file"))
    {
        let path = local_path(
            entry
                .attribute("path")
                .ok_or_else(|| svn_error("文件缺少路径"))?,
        );
        let sha = entry
            .descendants()
            .find(|n| n.has_tag_name("checksum"))
            .and_then(|n| n.text())
            .filter(|s| s.len() == 40 && s.bytes().all(|c| c.is_ascii_hexdigit()))
            .ok_or_else(|| svn_error(format!("无法读取 {path} 的 SVN 原始内容校验和")))?;
        files.insert(path, sha.into());
    }
    Ok(files)
}

pub(super) fn properties(data: &[u8]) -> Result<BTreeMap<String, HashSet<String>>> {
    let doc = xml(data)?;
    let mut result = BTreeMap::new();
    for target in doc.descendants().filter(|n| n.has_tag_name("target")) {
        let name = target
            .attribute("path")
            .ok_or_else(|| svn_error("SVN 属性缺少路径"))?;
        let names = target
            .children()
            .filter(|n| n.has_tag_name("property"))
            .filter_map(|n| n.attribute("name"))
            .map(str::to_owned)
            .collect();
        result.insert(local_path(name), names);
    }
    Ok(result)
}

pub(super) fn mode(props: Option<&HashSet<String>>) -> &'static str {
    if props.is_some_and(|p| p.contains("svn:special")) {
        "120000"
    } else if props.is_some_and(|p| p.contains("svn:executable")) {
        "100755"
    } else {
        "100644"
    }
}

pub(super) fn pristine_path(wc: &Path, sha: &str) -> PathBuf {
    wc.join(".svn")
        .join("pristine")
        .join(&sha[..2])
        .join(format!("{sha}.svn-base"))
}

#[derive(Default)]
struct Dir {
    files: Vec<Entry>,
    dirs: BTreeMap<String, Dir>,
}

impl Dir {
    fn insert(&mut self, path: &str, mode: &str, oid: gix::ObjectId) -> Result<()> {
        let mut dir = self;
        let mut parts = path.split('/').peekable();
        while let Some(name) = parts.next() {
            if name.is_empty() || name == "." || name == ".." || name.eq_ignore_ascii_case(".git") {
                return Err(git_error(format!("Git 无法表示 SVN 路径 {path:?}")));
            }
            if parts.peek().is_some() {
                dir = dir.dirs.entry(name.into()).or_default();
            } else {
                let kind = match mode {
                    "100644" => EntryKind::Blob,
                    "100755" => EntryKind::BlobExecutable,
                    "120000" => EntryKind::Link,
                    _ => return Err(git_error(format!("{path}: 非法 Git 文件模式 {mode}"))),
                };
                dir.files.push(Entry {
                    filename: name.into(),
                    mode: kind.into(),
                    oid,
                });
            }
        }
        Ok(())
    }

    fn write(mut self, repo: &gix::Repository) -> Result<gix::ObjectId> {
        for (name, dir) in self.dirs {
            self.files.push(Entry {
                filename: name.into(),
                mode: EntryKind::Tree.into(),
                oid: dir.write(repo)?,
            });
        }
        self.files.sort();
        repo.write_object(gix::objs::Tree {
            entries: self.files,
        })
        .map(|id| id.detach())
        .map_err(git_error)
    }
}

pub(super) async fn write(
    svn: &Svn<'_>,
    wc: &Path,
    location: &Location,
    manifest: &Manifest,
    dest: &Path,
    blobs: &mut HashMap<(PathBuf, bool), gix::ObjectId>,
) -> Result<String> {
    let files = pristines(svn, wc, location).await?;
    let props = properties(&svn.run(&["proplist", "--xml", "-R", "."], Some(wc)).await?)?;
    let repo = gix::open(dest).map_err(git_error)?;
    let mut tree = Dir::default();
    let files = files.into_iter().map(|(path, sha)| {
        let mode = mode(props.get(&path));
        (path, mode, pristine_path(wc, &sha))
    });
    transform::apply(manifest, dest, files, true, |path, mode, content| {
        // SVN names pristine files by checksum. Reuse unchanged Git blobs across
        // revisions without reading working-copy EOL or keyword translations.
        if let transform::Content::Pristine(source) = content {
            let key = (source.to_owned(), mode == "120000");
            if let Some(&oid) = blobs.get(&key) {
                return tree.insert(path, mode, oid);
            }
            let oid = repo
                .write_blob(content.bytes(path, mode)?)
                .map_err(git_error)?
                .detach();
            blobs.insert(key, oid);
            return tree.insert(path, mode, oid);
        }
        let oid = repo
            .write_blob(content.bytes(path, mode)?)
            .map_err(git_error)?
            .detach();
        tree.insert(path, mode, oid)
    })?;
    Ok(tree.write(&repo)?.to_string())
}
