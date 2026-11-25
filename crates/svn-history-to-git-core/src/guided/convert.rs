//! Rebuild selected SVN directory histories from pegged paths. Git objects carry
//! the repository UUID, historical path, revision and original timestamp.
use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;

use chrono::DateTime;
use roxmltree::{Document, Node};
use tokio::io::AsyncWriteExt;

use super::{source, transform, Manifest, ScopeRef, VerifiedRef};
use crate::credential::ResolvedSecret;
use crate::error::{Result, SvnHistoryToGitError};
use crate::orchestrator::dummy_config_for_url;
use crate::process::{configure_silent_command, decode_bytes, CommandSpec};
use crate::storage::AuthorEntry;
use crate::svn::build_svn_command;

#[path = "convert_authors.rs"]
mod authors;
pub use authors::scan_authors;
#[path = "convert_tree.rs"]
mod tree;
#[path = "convert_verify.rs"]
mod verify;

async fn output(spec: CommandSpec) -> Result<Vec<u8>> {
    let mut cmd = configure_silent_command(&spec.program);
    if let Some(dir) = &spec.cwd {
        cmd.current_dir(dir);
    }
    cmd.args(&spec.args)
        .envs(&spec.env)
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if spec.stdin_data.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    let mut child = cmd.spawn()?;
    let input = child.stdin.take();
    let (write, out) = tokio::join!(
        async {
            if let (Some(mut input), Some(data)) = (input, &spec.stdin_data) {
                input.write_all(data).await?;
            }
            std::io::Result::Ok(())
        },
        child.wait_with_output()
    );
    let out = out?;
    if !out.status.success() {
        return Err(SvnHistoryToGitError::CommandFailed {
            command: spec.display_command(),
            code: out.status.code(),
            stderr: decode_bytes(&out.stderr),
        });
    }
    write?;
    Ok(out.stdout)
}

fn xml(data: &[u8]) -> Result<Document<'_>> {
    let text = std::str::from_utf8(data).map_err(|e| SvnHistoryToGitError::Svn(e.to_string()))?;
    Document::parse(text).map_err(|e| SvnHistoryToGitError::Svn(format!("SVN XML: {e}")))
}

fn child_text<'a>(node: Node<'a, '_>, name: &str) -> Result<&'a str> {
    node.children()
        .find(|n| n.has_tag_name(name))
        .map(|n| n.text().unwrap_or(""))
        .ok_or_else(|| SvnHistoryToGitError::Svn(format!("SVN XML 缺少 {name}")))
}

fn utf8(data: Vec<u8>) -> Result<String> {
    String::from_utf8(data).map_err(|e| SvnHistoryToGitError::Git(e.to_string()))
}

async fn git(dir: &Path, args: &[&str], input: Option<Vec<u8>>) -> Result<Vec<u8>> {
    let mut spec = CommandSpec::new("git", args.iter().map(|s| s.to_string()).collect())
        .cwd(dir)
        .env("GIT_NO_REPLACE_OBJECTS", "1");
    spec.stdin_data = input;
    output(spec).await
}

async fn git_line(dir: &Path, args: &[&str], input: Option<Vec<u8>>) -> Result<String> {
    Ok(utf8(git(dir, args, input).await?)?
        .trim_end_matches(['\n', '\r'])
        .into())
}

struct Svn<'a> {
    secret: Option<&'a ResolvedSecret>,
}

impl Svn<'_> {
    async fn run(&self, args: &[&str], cwd: Option<&Path>) -> Result<Vec<u8>> {
        let mut spec = build_svn_command(
            "svn",
            args.iter().map(|s| s.to_string()).collect(),
            &dummy_config_for_url(""),
            self.secret,
            None,
        );
        spec.cwd = cwd.map(Path::to_path_buf);
        output(spec).await
    }

    async fn location(&self, target: &str, rev: u64, uuid: &str) -> Result<Location> {
        let data = self
            .run(&["info", "--xml", "-r", &rev.to_string(), target], None)
            .await?;
        let doc = xml(&data)?;
        let entry = doc
            .descendants()
            .find(|n| n.has_tag_name("entry") && n.attribute("kind") == Some("dir"))
            .ok_or_else(|| SvnHistoryToGitError::Svn(format!("r{rev} 未找到目录 {target}")))?;
        if doc
            .descendants()
            .find(|n| n.has_tag_name("uuid"))
            .and_then(|n| n.text())
            != Some(uuid)
        {
            return Err(SvnHistoryToGitError::Svn(format!(
                "r{rev} 的仓库标识与确认清单不同"
            )));
        }
        Ok(Location {
            url: child_text(entry, "url")?.into(),
            relative: child_text(entry, "relative-url")?.into(),
            revision: rev,
        })
    }

    async fn history(&self, target: &str, revision: u64, verbose: bool) -> Result<Vec<Revision>> {
        let range = format!("{revision}:1");
        let mut args = vec!["log", "--xml", "-r", &range];
        if verbose {
            args.push("-v");
        }
        args.push(target);
        let data = self.run(&args, None).await?;
        let doc = xml(&data)?;
        let mut history = Vec::new();
        for entry in doc.descendants().filter(|n| n.has_tag_name("logentry")) {
            let rev = entry
                .attribute("revision")
                .and_then(|s| s.parse::<u64>().ok())
                .filter(|r| *r > 0 && *r <= revision)
                .ok_or_else(|| SvnHistoryToGitError::Svn("无效的日志修订".into()))?;
            if history.last().is_some_and(|p: &Revision| p.rev <= rev) {
                return Err(SvnHistoryToGitError::Svn(format!("r{rev} 的日志顺序有误")));
            }
            let mut changed = Vec::new();
            for path in entry.descendants().filter(|n| n.has_tag_name("path")) {
                changed.push(Change {
                    path: path.text().unwrap_or("").to_owned(),
                    copied: path.attribute("copyfrom-path").is_some()
                        && matches!(path.attribute("action"), Some("A" | "R")),
                    structural: matches!(path.attribute("action"), Some("A" | "R" | "D")),
                    unmodified: path.attribute("prop-mods") == Some("false")
                        && path.attribute("text-mods") == Some("false"),
                });
            }
            let date = child_text(entry, "date")?.to_owned();
            let timestamp = DateTime::parse_from_rfc3339(&date)
                .map_err(|e| SvnHistoryToGitError::Svn(format!("r{rev} 日期: {e}")))?
                .timestamp();
            let author = entry
                .children()
                .find(|n| n.has_tag_name("author"))
                .and_then(|n| n.text())
                .unwrap_or("unknown");
            history.push(Revision {
                rev,
                author: author.into(),
                date,
                timestamp,
                message: child_text(entry, "msg")?.into(),
                changed,
            });
        }
        if history.is_empty() {
            return Err(SvnHistoryToGitError::Svn(format!("没有可读取的路径历史 {target}")));
        }
        Ok(history)
    }
}

struct Location {
    url: String,
    relative: String,
    revision: u64,
}
struct Revision {
    rev: u64,
    author: String,
    date: String,
    timestamp: i64,
    message: String,
    changed: Vec<Change>,
}

struct Change {
    path: String,
    copied: bool,
    structural: bool,
    unmodified: bool,
}

impl Revision {
    fn signature(&self, authors: &[AuthorEntry]) -> Result<String> {
        let (name, email) = authors
            .iter()
            .find(|e| e.svn_user == self.author)
            .map(|e| (e.git_name.clone(), e.git_email.clone()))
            .unwrap_or_else(|| (self.author.clone(), format!("{}@svn.local", self.author)));
        if [&name, &email]
            .iter()
            .any(|s| s.trim().is_empty() || s.contains(['\r', '\n', '<', '>', '\0']))
        {
            return Err(SvnHistoryToGitError::Config(format!(
                "请为 SVN 用户 {:?} 指定 Git 姓名和邮箱",
                self.author
            )));
        }
        Ok(format!("{name} <{email}> {} +0000", self.timestamp))
    }
}

fn tag_body(rev: &Revision, uuid: &str, relative: &str) -> String {
    format!(
        "{}\n\nsvn-revision: {}\nsvn-source: {uuid} {relative}\nsvn-date: {}\n",
        rev.message, rev.rev, rev.date
    )
}

/// A copied tag can subsequently appear in SVN logs because its enclosing
/// project moved. Keep those events in the annotation on the original commit.
struct Annotation {
    revisions: usize,
    tagger: String,
    body: String,
}

async fn annotation(
    svn: &Svn<'_>,
    manifest: &Manifest,
    item: &ScopeRef,
    target: &str,
    history: &[Revision],
    authors: &[AuthorEntry],
) -> Result<Option<Annotation>> {
    if item.kind != "tag" {
        return Ok(None);
    }
    let mut relocations = Vec::new();
    for (index, rev) in history
        .iter()
        .enumerate()
        .take(history.len().saturating_sub(1))
    {
        let loc = svn.location(target, rev.rev, &manifest.uuid).await?;
        let Some(encoded) = loc.relative.strip_prefix('^') else {
            return Ok(None);
        };
        let path = percent_encoding::percent_decode_str(encoded)
            .decode_utf8()
            .map_err(tree::svn_error)?;
        let descendants = format!("{path}/");
        if rev
            .changed
            .iter()
            .any(|change| change.path.starts_with(&descendants))
        {
            return Ok(None);
        }
        if let Some(change) = rev
            .changed
            .iter()
            .find(|change| change.path == path.as_ref())
        {
            if !change.copied || !change.unmodified {
                return Ok(None);
            }
            let mut body = tag_body(rev, &manifest.uuid, &loc.relative);
            if !relocations.is_empty() {
                relocations.reverse();
                body.push_str(&format!(
                    "svn-relocations: {}\n",
                    serde_json::to_string(&relocations)?
                ));
            }
            return Ok(Some(Annotation {
                revisions: index + 1,
                tagger: rev.signature(authors)?,
                body,
            }));
        }
        // A property-only edit to an enclosing folder does not replace the
        // enclosing copy that carried this tag to its new location.
        let ancestor = rev
            .changed
            .iter()
            .filter(|change| {
                change.structural
                    && path
                        .strip_prefix(&change.path)
                        .is_some_and(|s| s.starts_with('/'))
            })
            .max_by_key(|change| change.path.len());
        if !ancestor.is_some_and(|change| change.copied) {
            return Ok(None);
        }
        relocations.push(
            serde_json::json!({"revision":rev.rev,"path":loc.relative,"author":rev.author,
            "git_author":rev.signature(authors)?,"date":rev.date,"message":rev.message}),
        );
    }
    Ok(None)
}

pub async fn convert(
    manifest: &Manifest,
    workdir: &Path,
    authors: &[AuthorEntry],
    secret: Option<&ResolvedSecret>,
    progress: &(dyn Fn(&str, &str) + Sync),
) -> Result<Vec<VerifiedRef>> {
    std::fs::create_dir_all(workdir)?;
    if workdir.read_dir()?.next().is_some() {
        return Err(SvnHistoryToGitError::Config(format!(
            "转换目录须为空 {}",
            workdir.display()
        )));
    }
    let trunk = manifest
        .refs
        .iter()
        .find(|r| r.kind == "trunk")
        .ok_or_else(|| SvnHistoryToGitError::Config("缺少主线".into()))?;
    git(
        workdir,
        &["init", "--bare", "--initial-branch", &trunk.name, "."],
        None,
    )
    .await?;
    let scratch = tempfile::tempdir()?;
    let wc = scratch.path().join("replay");
    let svn = Svn { secret };
    let mut commits = HashMap::<(String, u64), String>::new();
    let mut blobs = HashMap::new();
    for item in &manifest.refs {
        let target = pinned(manifest, item)?;
        let history = svn.history(&target, manifest.revision, true).await?;
        let annotation = annotation(&svn, manifest, item, &target, &history, authors).await?;
        let commit_count = history.len() - annotation.as_ref().map_or(0, |a| a.revisions);
        let mut parent = None::<String>;
        for (index, rev) in history.iter().rev().take(commit_count).enumerate() {
            progress(
                "convert",
                &format!(
                    "{} · r{} · {}/{}",
                    item.full_ref(),
                    rev.rev,
                    index + 1,
                    history.len()
                ),
            );
            let loc = svn.location(&target, rev.rev, &manifest.uuid).await?;
            let key = (loc.relative.clone(), rev.rev);
            if let Some(commit) = commits.get(&key) {
                parent = Some(commit.clone());
                continue;
            }
            let signature = rev.signature(authors)?;
            tree::checkout(&svn, &wc, &loc.url, rev.rev).await?;
            let tree = tree::write(&svn, &wc, &loc, manifest, workdir, &mut blobs).await?;
            let ancestry = parent
                .as_ref()
                .map(|p| format!("parent {p}\n"))
                .unwrap_or_default();
            let commit = format!("tree {tree}\n{ancestry}author {signature}\ncommitter {signature}\nsvn-revision {}\nsvn-source {} {}\nsvn-date {}\n\n{}",
                    rev.rev,manifest.uuid,loc.relative,rev.date,rev.message);
            let oid = git_line(
                workdir,
                &["hash-object", "-t", "commit", "-w", "--stdin"],
                Some(commit.into_bytes()),
            )
            .await?;
            commits.insert(key, oid.clone());
            parent = Some(oid);
        }
        if let Some(annotation) = annotation {
            let source = parent.as_deref().expect("copy has a source revision");
            let tag = format!(
                "object {source}\ntype commit\ntag {}\ntagger {}\n\n{}",
                item.name, annotation.tagger, annotation.body
            );
            parent = Some(
                git_line(
                    workdir,
                    &["hash-object", "-t", "tag", "-w", "--stdin"],
                    Some(tag.into_bytes()),
                )
                .await?,
            );
            progress(
                "convert",
                &format!("{} · {} 项 SVN 修订已记录", item.full_ref(), history.len()),
            );
        }
        git(
            workdir,
            &[
                "update-ref",
                &item.full_ref(),
                parent.as_deref().expect("nonempty history"),
            ],
            None,
        )
        .await?;
    }
    let mut verified = Vec::new();
    for item in &manifest.refs {
        progress(
            "verify",
            &format!("核对 {} 的 SVN 路径历史及文件", item.full_ref()),
        );
        verified.push(verify::selected(&svn, manifest, item, workdir, authors, progress).await?);
    }
    Ok(verified)
}

fn pinned(manifest: &Manifest, item: &ScopeRef) -> Result<String> {
    Ok(format!(
        "{}@{}",
        source::at_path(&manifest.svn_url, &item.path)?,
        manifest.revision
    ))
}

#[cfg(test)]
#[path = "convert_tests.rs"]
mod tests;
