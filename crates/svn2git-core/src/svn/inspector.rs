use std::collections::BTreeMap;
use std::path::Path;

use quick_xml::events::Event;
use quick_xml::Reader;
use serde::{Deserialize, Serialize};

use crate::config::{LayoutMode, SvnExternalEntry, TaskConfig};
use crate::credential::ResolvedSecret;
use crate::error::{Result, Svn2GitError};
use crate::process::{run_command, LineCallback, ProcessRegistry};
use crate::storage::AuthorEntry;
use crate::svn::build_svn_command;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SvnRepoInfo {
    pub url: String,
    pub root_url: String,
    pub uuid: String,
    pub head_revision: u64,
    pub last_changed_rev: u64,
    pub last_changed_author: String,
    pub last_changed_date: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SvnTreeEntry {
    pub name: String,
    pub path: String,
    pub kind: String, // "dir" | "file"
    pub size: Option<u64>,
    pub commit_rev: u64,
    pub commit_author: String,
    pub commit_date: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayoutDetection {
    pub suggested_mode: LayoutMode,
    pub has_trunk: bool,
    pub has_branches: bool,
    pub has_tags: bool,
    pub top_level_dirs: Vec<String>,
    pub summary: String,
}

pub async fn fetch_svn_info(
    registry: &ProcessRegistry,
    task_id: i64,
    config: &TaskConfig,
    secret: Option<&ResolvedSecret>,
    svn_config_dir: Option<&Path>,
    on_line: Option<LineCallback>,
) -> Result<SvnRepoInfo> {
    let url = config.effective_svn_url();
    let spec = build_svn_command(
        "svn",
        vec!["info".into(), "--xml".into(), url],
        config,
        secret,
        svn_config_dir,
    )
    .timeout(30)
    .log_output(false);

    let out = run_command(registry, task_id, spec, on_line).await?;
    parse_svn_info_xml(&out.stdout)
}

pub fn parse_svn_info_xml(xml: &str) -> Result<SvnRepoInfo> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut info = SvnRepoInfo {
        url: String::new(),
        root_url: String::new(),
        uuid: String::new(),
        head_revision: 0,
        last_changed_rev: 0,
        last_changed_author: String::new(),
        last_changed_date: String::new(),
    };

    let mut current_tag = String::new();
    let mut in_commit = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if tag == "entry" {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"revision" {
                            if let Ok(v) = String::from_utf8_lossy(&attr.value).parse::<u64>() {
                                info.head_revision = v;
                            }
                        }
                    }
                } else if tag == "commit" {
                    in_commit = true;
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"revision" {
                            if let Ok(v) = String::from_utf8_lossy(&attr.value).parse::<u64>() {
                                info.last_changed_rev = v;
                            }
                        }
                    }
                }
                current_tag = tag;
            }
            Ok(Event::End(e)) => {
                if e.name().as_ref() == b"commit" {
                    in_commit = false;
                }
                current_tag.clear();
            }
            Ok(Event::Text(e)) => {
                let text = e.unescape().unwrap_or_default().to_string();
                match current_tag.as_str() {
                    "url" => info.url = text,
                    "root" => info.root_url = text,
                    "uuid" => info.uuid = text,
                    "author" if in_commit => info.last_changed_author = text,
                    "date" if in_commit => info.last_changed_date = text,
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(Svn2GitError::Svn(format!("failed to parse svn info XML: {e}"))),
            _ => {}
        }
    }

    if info.url.is_empty() && info.head_revision == 0 {
        return Err(Svn2GitError::Svn("empty svn info response".into()));
    }
    Ok(info)
}

pub async fn list_svn_directory(
    registry: &ProcessRegistry,
    task_id: i64,
    config: &TaskConfig,
    relative_path: Option<&str>,
    secret: Option<&ResolvedSecret>,
    svn_config_dir: Option<&Path>,
) -> Result<Vec<SvnTreeEntry>> {
    let base = config.svn_url.trim().trim_end_matches('/');
    let rel = relative_path
        .unwrap_or("")
        .trim()
        .trim_matches('/')
        .replace('\\', "/");
    let target = if rel.is_empty() {
        base.to_string()
    } else {
        format!("{base}/{rel}")
    };

    let spec = build_svn_command(
        "svn",
        vec!["list".into(), "--xml".into(), target],
        config,
        secret,
        svn_config_dir,
    )
    .timeout(30)
    .log_output(false);

    let out = run_command(registry, task_id, spec, None).await?;
    parse_svn_list_xml(&out.stdout, &rel)
}

pub fn parse_svn_list_xml(xml: &str, parent_rel: &str) -> Result<Vec<SvnTreeEntry>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut entries = Vec::new();
    let mut cur: Option<SvnTreeEntry> = None;
    let mut current_tag = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if tag == "entry" {
                    let mut kind = "file".to_string();
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"kind" {
                            kind = String::from_utf8_lossy(&attr.value).to_string();
                        }
                    }
                    cur = Some(SvnTreeEntry {
                        name: String::new(),
                        path: String::new(),
                        kind,
                        size: None,
                        commit_rev: 0,
                        commit_author: String::new(),
                        commit_date: String::new(),
                    });
                } else if tag == "commit" {
                    if let Some(entry) = cur.as_mut() {
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == b"revision" {
                                if let Ok(v) = String::from_utf8_lossy(&attr.value).parse::<u64>() {
                                    entry.commit_rev = v;
                                }
                            }
                        }
                    }
                }
                current_tag = tag;
            }
            Ok(Event::End(e)) => {
                if e.name().as_ref() == b"entry" {
                    if let Some(mut entry) = cur.take() {
                        entry.path = if parent_rel.is_empty() {
                            entry.name.clone()
                        } else {
                            format!("{parent_rel}/{}", entry.name)
                        };
                        entries.push(entry);
                    }
                }
                current_tag.clear();
            }
            Ok(Event::Text(e)) => {
                if let Some(entry) = cur.as_mut() {
                    let text = e.unescape().unwrap_or_default().to_string();
                    match current_tag.as_str() {
                        "name" => entry.name = text,
                        "size" => entry.size = text.parse::<u64>().ok(),
                        "author" => entry.commit_author = text,
                        "date" => entry.commit_date = text,
                        _ => {}
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(Svn2GitError::Svn(format!("failed to parse svn list XML: {e}"))),
            _ => {}
        }
    }

    entries.sort_by(|a, b| {
        let ord_kind = (a.kind != "dir").cmp(&(b.kind != "dir"));
        ord_kind.then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(entries)
}

pub async fn detect_layout(
    registry: &ProcessRegistry,
    task_id: i64,
    config: &TaskConfig,
    secret: Option<&ResolvedSecret>,
    svn_config_dir: Option<&Path>,
) -> Result<LayoutDetection> {
    let entries = list_svn_directory(
        registry,
        task_id,
        config,
        config.svn_subpath.as_deref(),
        secret,
        svn_config_dir,
    )
    .await?;

    let dirs: Vec<String> = entries
        .into_iter()
        .filter(|e| e.kind == "dir")
        .map(|e| e.name)
        .collect();

    let has_trunk = dirs.iter().any(|d| d == "trunk");
    let has_branches = dirs.iter().any(|d| d == "branches");
    let has_tags = dirs.iter().any(|d| d == "tags");

    let (suggested_mode, summary) = if has_trunk && (has_branches || has_tags) {
        (
            LayoutMode::Standard,
            format!(
                "检测到标准 SVN 布局 (trunk={}, branches={}, tags={})",
                has_trunk, has_branches, has_tags
            ),
        )
    } else if dirs.iter().any(|d| d == "releases" || d == "branch") {
        (
            LayoutMode::Custom,
            format!("检测到自定义多分支布局，顶层目录: {}", dirs.join(", ")),
        )
    } else {
        (
            LayoutMode::Root,
            "未检测到 trunk/branches/tags，建议采用单目录直迁 (Root) 模式".to_string(),
        )
    };

    Ok(LayoutDetection {
        suggested_mode,
        has_trunk,
        has_branches,
        has_tags,
        top_level_dirs: dirs,
        summary,
    })
}

/// Streaming `svn log --xml --quiet` author scanner with commit counts.
pub async fn discover_authors(
    registry: &ProcessRegistry,
    task_id: i64,
    config: &TaskConfig,
    secret: Option<&ResolvedSecret>,
    svn_config_dir: Option<&Path>,
    on_line: Option<LineCallback>,
) -> Result<Vec<AuthorEntry>> {
    let mut args = vec!["log".into(), "--xml".into(), "--quiet".into()];
    match (config.start_revision, config.end_revision) {
        (Some(s), Some(e)) => {
            args.push("-r".into());
            args.push(format!("{s}:{e}"));
        }
        (Some(s), None) => {
            args.push("-r".into());
            args.push(format!("{s}:HEAD"));
        }
        (None, Some(e)) => {
            args.push("-r".into());
            args.push(format!("1:{e}"));
        }
        (None, None) => {}
    }
    args.push(config.effective_svn_url());

    let spec = build_svn_command("svn", args, config, secret, svn_config_dir)
        .timeout(300)
        .log_output(false);
    let out = run_command(registry, task_id, spec, on_line).await?;
    parse_authors_from_log_xml(&out.stdout, config.default_email_domain.as_deref())
}

pub fn parse_authors_from_log_xml(
    xml: &str,
    default_domain: Option<&str>,
) -> Result<Vec<AuthorEntry>> {
    let domain = default_domain
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .unwrap_or("svn.local");

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    let mut in_author = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if e.name().as_ref() == b"author" => in_author = true,
            Ok(Event::End(e)) if e.name().as_ref() == b"author" => in_author = false,
            Ok(Event::Text(e)) if in_author => {
                let author = e.unescape().unwrap_or_default().trim().to_string();
                if !author.is_empty() {
                    *counts.entry(author).or_insert(0) += 1;
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(Svn2GitError::Svn(format!("failed to parse svn log XML: {e}"))),
            _ => {}
        }
    }

    let now = chrono::Utc::now().to_rfc3339();
    let mut result: Vec<AuthorEntry> = counts
        .into_iter()
        .map(|(user, count)| AuthorEntry {
            git_name: user.clone(),
            git_email: format!("{user}@{domain}"),
            svn_user: user,
            commit_count: count,
            updated_at: now.clone(),
        })
        .collect();
    result.sort_by(|a, b| b.commit_count.cmp(&a.commit_count).then(a.svn_user.cmp(&b.svn_user)));
    Ok(result)
}

/// Scan `svn:externals` recursively using `svn propget -R svn:externals --xml`.
pub async fn scan_svn_externals(
    registry: &ProcessRegistry,
    task_id: i64,
    config: &TaskConfig,
    secret: Option<&ResolvedSecret>,
    svn_config_dir: Option<&Path>,
) -> Result<Vec<SvnExternalEntry>> {
    let spec = build_svn_command(
        "svn",
        vec![
            "propget".into(),
            "-R".into(),
            "svn:externals".into(),
            "--xml".into(),
            config.effective_svn_url(),
        ],
        config,
        secret,
        svn_config_dir,
    )
    .timeout(60)
    .log_output(false);

    let out = match run_command(registry, task_id, spec, None).await {
        Ok(o) => o,
        Err(_) => return Ok(Vec::new()),
    };

    let mut reader = Reader::from_str(&out.stdout);
    reader.config_mut().trim_text(true);
    let mut externals = Vec::new();
    let mut current_target = String::new();
    let mut in_prop = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match e.name().as_ref() {
                b"target" => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"path" {
                            current_target = String::from_utf8_lossy(&attr.value).to_string();
                        }
                    }
                }
                b"property" => in_prop = true,
                _ => {}
            },
            Ok(Event::End(e)) if e.name().as_ref() == b"property" => in_prop = false,
            Ok(Event::Text(e)) if in_prop => {
                let text = e.unescape().unwrap_or_default().trim().to_string();
                if !text.is_empty() {
                    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
                        externals.push(SvnExternalEntry {
                            path: current_target.clone(),
                            definition: line.to_string(),
                        });
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }

    Ok(externals)
}

/// Extract `svn:ignore` rules recursively to generate `.gitignore`.
pub async fn fetch_svn_ignore_rules(
    registry: &ProcessRegistry,
    task_id: i64,
    config: &TaskConfig,
    secret: Option<&ResolvedSecret>,
    svn_config_dir: Option<&Path>,
) -> Result<String> {
    let base_url = config.effective_svn_url();
    let spec = build_svn_command(
        "svn",
        vec![
            "propget".into(),
            "-R".into(),
            "svn:ignore".into(),
            "--xml".into(),
            base_url.clone(),
        ],
        config,
        secret,
        svn_config_dir,
    )
    .timeout(60)
    .log_output(false);

    let out = match run_command(registry, task_id, spec, None).await {
        Ok(o) => o,
        Err(_) => return Ok(String::new()),
    };

    let mut reader = Reader::from_str(&out.stdout);
    reader.config_mut().trim_text(true);
    let mut lines = Vec::new();
    let mut current_target = String::new();
    let mut in_prop = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match e.name().as_ref() {
                b"target" => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"path" {
                            current_target = String::from_utf8_lossy(&attr.value).to_string();
                        }
                    }
                }
                b"property" => in_prop = true,
                _ => {}
            },
            Ok(Event::End(e)) if e.name().as_ref() == b"property" => in_prop = false,
            Ok(Event::Text(e)) if in_prop => {
                let text = e.unescape().unwrap_or_default().to_string();
                let rel_dir = current_target
                    .strip_prefix(&base_url)
                    .unwrap_or("")
                    .trim_matches('/');
                for pattern in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
                    if rel_dir.is_empty() {
                        lines.push(pattern.to_string());
                    } else {
                        lines.push(format!("{rel_dir}/{pattern}"));
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }

    lines.sort();
    lines.dedup();
    if lines.is_empty() {
        Ok(String::new())
    } else {
        Ok(format!(
            "# Auto-generated from svn:ignore by SVN2Git\n{}\n",
            lines.join("\n")
        ))
    }
}
