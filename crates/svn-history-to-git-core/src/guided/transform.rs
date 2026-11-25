use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use sha2::{Digest, Sha256};

use super::{invalid, Manifest};
use crate::error::{Result, SvnHistoryToGitError};

fn exclusions(manifest: &Manifest) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in &manifest.exclude_globs {
        if pattern.trim().is_empty() {
            return Err(invalid("排除规则不能为空"));
        }
        builder.add(
            Glob::new(pattern)
                .map_err(|error| invalid(format!("排除规则 {pattern} 无效：{error}")))?,
        );
    }
    builder.build().map_err(|e| invalid(e.to_string()))
}

pub(super) fn validate(manifest: &Manifest) -> Result<()> {
    if let Some(mb) = manifest.lfs_threshold_mb {
        if mb == 0 || mb.checked_mul(1 << 20).is_none() {
            return Err(invalid("LFS 阈值须为正整数 MiB"));
        }
    }
    exclusions(manifest)?;
    Ok(())
}

fn digest(path: &Path, mut sink: impl Write) -> Result<(String, u64)> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 65536];
    let mut size = 0;
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        sink.write_all(&buffer[..n])?;
        size += n as u64;
    }
    Ok((hex::encode(hasher.finalize()), size))
}

fn pointer(repo: &Path, source: &Path, write: bool) -> Result<Vec<u8>> {
    let expected = digest(source, io::sink())?;
    let (oid, size) = &expected;
    let dir = repo.join("lfs/objects").join(&oid[..2]).join(&oid[2..4]);
    let object = dir.join(oid);
    if write && !object.exists() {
        fs::create_dir_all(&dir)?;
        let mut temp = tempfile::NamedTempFile::new_in(&dir)?;
        if digest(source, &mut temp)? != expected {
            return Err(SvnHistoryToGitError::Verification(format!(
                "写入 LFS 对象时 SVN 原始内容发生变化 {oid}"
            )));
        }
        temp.persist(&object).map_err(|e| e.error)?;
    } else if digest(&object, io::sink())
        .map_err(|e| SvnHistoryToGitError::Verification(format!("无法读取本地 LFS 对象 {oid}：{e}")))?
        != expected
    {
        return Err(SvnHistoryToGitError::Verification(format!(
            "本地 LFS 对象内容损坏 {oid}"
        )));
    }
    Ok(
        format!("version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {size}\n")
            .into_bytes(),
    )
}

// Anchor exact filenames in the nearest .gitattributes, escaping both Git's
// wildmatch pattern and its surrounding C-style quoted string.
fn attribute_pattern(name: &str) -> String {
    let mut text = String::from("\"/");
    for c in name.chars() {
        match c {
            '?' | '[' | ']' | '*' => {
                text.push_str("\\\\");
                text.push(c);
            }
            '\\' => text.push_str("\\\\\\\\"),
            '"' => text.push_str("\\\""),
            '\t' => text.push_str("\\t"),
            '\n' => text.push_str("\\n"),
            '\r' => text.push_str("\\r"),
            other => text.push(other),
        }
    }
    text.push('"');
    text
}

pub(super) enum Content<'a> {
    Pristine(&'a Path),
    Generated(&'a [u8]),
}

impl<'a> Content<'a> {
    pub(super) fn bytes(&self, name: &str, mode: &str) -> Result<Cow<'a, [u8]>> {
        match self {
            Self::Generated(bytes) => Ok(Cow::Borrowed(bytes)),
            Self::Pristine(path) => {
                let mut bytes = fs::read(path)?;
                if mode == "120000" {
                    if !bytes.starts_with(b"link ") {
                        return Err(SvnHistoryToGitError::Svn(format!(
                            "{name} 的 svn:special 内容无法表示为 Git 符号链接"
                        )));
                    }
                    bytes.drain(..5);
                }
                Ok(Cow::Owned(bytes))
            }
        }
    }
}

#[derive(Default)]
struct Attributes {
    source: Option<(&'static str, PathBuf)>,
    rules: Vec<String>,
}

/// Entries contain paths to original SVN pristines. No contents are opened
/// before exclusion and each delivered file is consumed before the next one.
pub(super) fn apply(
    manifest: &Manifest,
    repo: &Path,
    files: impl IntoIterator<Item = (String, &'static str, PathBuf)>,
    store_lfs: bool,
    mut emit: impl FnMut(&str, &str, Content<'_>) -> Result<()>,
) -> Result<()> {
    let excludes = exclusions(manifest)?;
    let threshold = manifest
        .lfs_threshold_mb
        .map(|mb| mb.saturating_mul(1 << 20));
    let mut attributes = BTreeMap::<String, Attributes>::new();
    for (path, mode, source) in files {
        if excludes.is_match(&path) {
            continue;
        }
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", &path));
        if let Some(limit) = threshold {
            if name == ".gitattributes" {
                attributes.entry(path).or_default().source = Some((mode, source));
                continue;
            }
            if mode != "120000" && source.metadata()?.len() >= limit {
                let attr = if parent.is_empty() {
                    ".gitattributes".into()
                } else {
                    format!("{parent}/.gitattributes")
                };
                if excludes.is_match(&attr) {
                    return Err(invalid(format!("排除规则与 LFS 所需文件 {attr} 冲突")));
                }
                attributes.entry(attr).or_default().rules.push(format!(
                    "{} filter=lfs diff=lfs merge=lfs -text\n",
                    attribute_pattern(name)
                ));
                emit(
                    &path,
                    mode,
                    Content::Generated(&pointer(repo, &source, store_lfs)?),
                )?;
                continue;
            }
        }
        emit(&path, mode, Content::Pristine(&source))?;
    }
    for (path, mut attr) in attributes {
        if attr.rules.is_empty() {
            let (mode, source) = attr.source.expect("attributes were present in source");
            emit(&path, mode, Content::Pristine(&source))?;
            continue;
        }
        let (mode, mut data) = match attr.source {
            Some(("120000", _)) => return Err(invalid("LFS 规则不能写入符号链接 .gitattributes")),
            Some((mode, source)) => (mode, fs::read(source)?),
            None => ("100644", Vec::new()),
        };
        if !data.is_empty() && !data.ends_with(b"\n") {
            data.push(b'\n');
        }
        attr.rules.sort_unstable();
        for rule in attr.rules {
            data.extend_from_slice(rule.as_bytes());
        }
        emit(&path, mode, Content::Generated(&data))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_regular_files_before_opening_the_next_pristine() {
        let mut plan = super::super::tests::manifest();
        plan.lfs_threshold_mb = Some(1);
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        fs::write(&first, b"a").unwrap();
        let files = [
            ("a".into(), "100644", first),
            ("b".into(), "100644", second.clone()),
        ];
        let mut delivered = Vec::new();
        apply(&plan, dir.path(), files, true, |name, mode, content| {
            delivered.extend_from_slice(&content.bytes(name, mode)?);
            if name == "a" {
                fs::write(&second, b"b")?;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(delivered, b"ab");
    }

    #[test]
    fn checks_lfs_bytes_and_preserves_existing_attributes() {
        let mut plan = super::super::tests::manifest();
        plan.lfs_threshold_mb = Some(1);
        plan.exclude_globs.push("out/**".into());
        let dir = tempfile::tempdir().unwrap();
        let large = vec![23; 1 << 20];
        let oid = hex::encode(Sha256::digest(&large));
        fs::write(dir.path().join("data"), large).unwrap();
        fs::write(dir.path().join("attrs"), b"*.txt text").unwrap();
        let source = [
            (
                "a dir/asset[1].bin".into(),
                "100644",
                dir.path().join("data"),
            ),
            (
                "a dir/.gitattributes".into(),
                "100644",
                dir.path().join("attrs"),
            ),
            // Filtering must happen before even a metadata lookup on the pristine.
            (
                "out/omit".into(),
                "100644",
                dir.path().join("does-not-exist"),
            ),
        ];
        let collect = |write| {
            let mut files = BTreeMap::new();
            apply(
                &plan,
                dir.path(),
                source.clone(),
                write,
                |name, mode, content| {
                    files.insert(
                        name.to_owned(),
                        (mode.to_owned(), content.bytes(name, mode)?.into_owned()),
                    );
                    Ok(())
                },
            )?;
            Result::Ok(files)
        };
        let files = collect(true).unwrap();
        assert!(!files.contains_key("out/omit"));
        assert!(String::from_utf8_lossy(&files["a dir/asset[1].bin"].1).contains(&oid));
        assert!(files["a dir/.gitattributes"].1.starts_with(b"*.txt text\n"));
        let check = collect(false).unwrap();
        assert_eq!(check, files);
        let object = dir
            .path()
            .join("lfs/objects")
            .join(&oid[..2])
            .join(&oid[2..4])
            .join(oid);
        fs::write(object, b"damaged").unwrap();
        assert!(collect(false).is_err());
    }
}
