use serde::Serialize;
use url::Url;

use super::{invalid, validate_relative};
use crate::credential::ResolvedSecret;
use crate::error::{Result, Svn2GitError};
use crate::orchestrator::dummy_config_for_url;
use crate::process::{run_command, ProcessRegistry};
use crate::svn::build_svn_command;
use crate::svn::inspector::{fetch_svn_info, list_svn_directory, SvnRepoInfo};

pub(super) fn source_url(value: &str) -> Result<Url> {
    let url = Url::parse(value.trim()).map_err(|_| invalid("请输入可访问的 SVN 仓库地址"))?;
    if !matches!(url.scheme(), "http" | "https" | "svn" | "svn+ssh" | "file")
        || url.cannot_be_a_base()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || (matches!(url.scheme(), "http" | "https") && !url.username().is_empty())
    {
        return Err(invalid("SVN 地址不正确。请在认证区域填写密码"));
    }
    Ok(url)
}

pub(super) fn at_path(project: &str, relative: &str) -> Result<String> {
    validate_relative(relative)?;
    let mut url = source_url(project)?;
    if !relative.is_empty() {
        url.path_segments_mut()
            .map_err(|_| invalid("SVN 项目地址无效"))?
            .pop_if_empty()
            .extend(relative.split('/'));
    }
    Ok(url.into())
}

pub async fn info(
    url: &str,
    secret: Option<&ResolvedSecret>,
    revision: Option<u64>,
) -> Result<SvnRepoInfo> {
    let base = source_url(url)?;
    let peg = revision.map(|r| r.to_string()).unwrap_or_default();
    let cfg = dummy_config_for_url(&format!("{base}@{peg}"));
    fetch_svn_info(&ProcessRegistry::new(), 0, &cfg, secret, None, None).await
}

async fn directories(
    url: &str,
    revision: u64,
    secret: Option<&ResolvedSecret>,
) -> Result<Vec<String>> {
    let cfg = dummy_config_for_url(&format!("{url}@{revision}"));
    let entries = list_svn_directory(&ProcessRegistry::new(), 0, &cfg, None, secret, None).await?;
    Ok(entries
        .into_iter()
        .filter(|e| e.kind == "dir")
        .map(|e| e.name)
        .collect())
}

#[derive(Serialize)]
pub struct Browse {
    pub url: String,
    pub uuid: String,
    pub revision: u64,
    pub dirs: Vec<String>,
}

pub async fn browse(url: &str, secret: Option<&ResolvedSecret>) -> Result<Browse> {
    let info = info(url, secret, None).await?;
    let dirs = directories(&info.url, info.head_revision, secret).await?;
    Ok(Browse {
        url: info.url,
        uuid: info.uuid,
        revision: info.head_revision,
        dirs,
    })
}

#[derive(Serialize)]
pub struct Project {
    pub url: String,
    pub uuid: String,
    pub revision: u64,
    pub trunk: String,
    pub branches: Vec<String>,
    pub tags: Vec<String>,
    pub externals: Vec<String>,
    pub external_warning: Option<String>,
}

fn external_directories(xml: &str, project: &str) -> Result<Vec<String>> {
    let document =
        roxmltree::Document::parse(xml).map_err(|_| invalid("无法解析 svn:externals 检查结果"))?;
    if !document.root_element().has_tag_name("properties") {
        return Err(invalid("SVN 属性检查结果不完整"));
    }
    let mut paths = Vec::new();
    for target in document
        .root_element()
        .children()
        .filter(|el| el.has_tag_name("target"))
    {
        if !target.children().any(|el| {
            el.has_tag_name("property")
                && el.attribute("name") == Some("svn:externals")
                && el.text().is_some_and(|s| !s.trim().is_empty())
        }) {
            continue;
        }
        let path = target
            .attribute("path")
            .ok_or_else(|| invalid("SVN 属性缺少来源路径"))?;
        let project_path = path
            .strip_prefix(project)
            .filter(|p| p.starts_with('/') || p.is_empty())
            .unwrap_or("");
        paths.push(if project_path.is_empty() {
            ".".to_owned()
        } else {
            project_path.trim_start_matches('/').to_owned()
        });
    }
    Ok(paths)
}

async fn externals(project: &Browse, secret: Option<&ResolvedSecret>) -> Result<Vec<String>> {
    let cfg = dummy_config_for_url(&project.url);
    let args = vec![
        "propget".into(),
        "-R".into(),
        "--xml".into(),
        "svn:externals".into(),
        format!("{}@{}", project.url, project.revision),
    ];
    let cmd = build_svn_command("svn", args, &cfg, secret, None)
        .timeout(60)
        .log_output(false);
    let output = run_command(&ProcessRegistry::new(), 0, cmd, None).await?;
    external_directories(&output.stdout, &project.url)
}

async fn candidates(
    project: &Browse,
    root: &str,
    secret: Option<&ResolvedSecret>,
) -> Result<Vec<String>> {
    validate_relative(root)?;
    if root.is_empty() || (!root.contains('/') && !project.dirs.iter().any(|dir| dir == root)) {
        return Ok(Vec::new());
    }
    let names = directories(&at_path(&project.url, root)?, project.revision, secret).await?;
    Ok(names
        .into_iter()
        .map(|name| format!("{root}/{name}"))
        .collect())
}

pub async fn inspect(
    url: &str,
    trunk: &str,
    branches: &str,
    tags: &str,
    secret: Option<&ResolvedSecret>,
) -> Result<Project> {
    inspect_layout(url, trunk, branches, tags, false, secret).await
}

pub async fn inspect_layout(
    url: &str,
    trunk: &str,
    branches: &str,
    tags: &str,
    auto_branch_subdir: bool,
    secret: Option<&ResolvedSecret>,
) -> Result<Project> {
    let project = browse(url, secret).await?;
    let (trunk, branches, tags) = if auto_branch_subdir
        && (trunk, branches, tags) == ("trunk", "branches", "tags")
        && !project.dirs.iter().any(|dir| dir == trunk)
    {
        ("", "", "")
    } else {
        (trunk, branches, tags)
    };
    let trunk_url = at_path(&project.url, trunk)?;
    if !trunk.is_empty() {
        if !trunk.contains('/') && !project.dirs.iter().any(|dir| dir == trunk) {
            return Err(invalid(format!(
                "所选项目未找到主线目录 {trunk}，请选定项目或指定主线路径"
            )));
        }
        directories(&trunk_url, project.revision, secret).await?;
    }
    let branches = candidates(&project, branches, secret).await?;
    let tags = candidates(&project, tags, secret).await?;
    let (externals, external_warning) = match externals(&project, secret).await {
        Ok(dirs) => (dirs, None),
        Err(err) => {
            let reason = if matches!(err, Svn2GitError::CommandTimeout { .. }) {
                "读取超时"
            } else {
                "读取或解析失败"
            };
            (
                Vec::new(),
                Some(format!(
                    "SVN 外部依赖检查未完成（{reason}）。本次迁移仅包含所选项目内的版本化文件。"
                )),
            )
        }
    };
    Ok(Project {
        url: project.url,
        uuid: project.uuid,
        revision: project.revision,
        trunk: trunk.into(),
        branches,
        tags,
        externals,
        external_warning,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_external_locations_without_persisting_their_credentials() {
        let xml = r#"<properties><target path="https://svn.example.org/project/trunk"><property name="svn:externals">
            tools https://hidden:token@other.example.org/private
        </property></target></properties>"#;
        assert_eq!(
            external_directories(xml, "https://svn.example.org/project").unwrap(),
            ["trunk"]
        );
        assert!(external_directories("<properties/>", "project")
            .unwrap()
            .is_empty());
        assert!(external_directories("<properties", "project").is_err());
    }
}
