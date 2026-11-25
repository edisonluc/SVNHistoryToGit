//! Publish the confirmed SVN histories as new branches and tags in a Git repository.
pub mod convert;
pub mod jobs;
pub mod publish;
pub mod source;
mod transform;

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::error::{Result, SvnHistoryToGitError};
use crate::storage::AuthorEntry;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScopeRef {
    pub path: String,
    pub kind: String,
    pub name: String,
}

impl ScopeRef {
    pub fn full_ref(&self) -> String {
        let namespace = if self.kind == "tag" { "tags" } else { "heads" };
        format!("refs/{namespace}/{}", self.name)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub svn_url: String,
    pub uuid: String,
    pub revision: u64,
    pub remote: String,
    #[serde(default)]
    pub web_url: String,
    pub refs: Vec<ScopeRef>,
    pub svn_credential_id: Option<String>,
    pub git_credential_id: Option<String>,
    #[serde(default)]
    pub authors: Vec<AuthorEntry>,
    #[serde(default)]
    pub exclude_globs: Vec<String>,
    pub lfs_threshold_mb: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VerifiedRef {
    pub full_ref: String,
    pub oid: String,
    pub commit_oid: String,
    pub files: u64,
    pub revisions: u64,
}

fn invalid(message: impl Into<String>) -> SvnHistoryToGitError {
    SvnHistoryToGitError::Config(message.into())
}

fn validate_relative(path: &str) -> Result<()> {
    if !path.is_empty()
        && (path.contains(['\\', '\0', '\n', '\r'])
            || path.split('/').any(|part| matches!(part, "" | "." | "..")))
    {
        return Err(invalid(format!("无效的 SVN 相对目录 {path:?}")));
    }
    Ok(())
}

fn has_secret_parameters(url: &url::Url) -> bool {
    [url.query(), url.fragment()]
        .into_iter()
        .flatten()
        .any(|part| {
            let fields = part.replace([';', '?'], "&");
            url::form_urlencoded::parse(fields.as_bytes()).any(|(key, _)| {
                let key = key.to_ascii_lowercase();
                key == "sig"
                    || key == "code"
                    || [
                        "token",
                        "password",
                        "secret",
                        "signature",
                        "credential",
                        "session",
                        "auth",
                        "key",
                    ]
                    .iter()
                    .any(|word| key.contains(word))
            })
        })
}

impl Manifest {
    pub fn validate(&self) -> Result<()> {
        source::source_url(&self.svn_url)?;
        publish::validate_remote(&self.remote)?;
        transform::validate(self)?;
        if self.revision == 0 || self.uuid.is_empty() || self.remote.trim().is_empty() {
            return Err(invalid("请重新检查 SVN 修订和远端仓库"));
        }
        if !self.web_url.is_empty() {
            let url = url::Url::parse(&self.web_url).map_err(|_| invalid("仓库网页地址无效"))?;
            if !matches!(url.scheme(), "https" | "http")
                || url.host_str().is_none()
                || url.password().is_some()
                || !url.username().is_empty()
                || has_secret_parameters(&url)
            {
                return Err(invalid(
                    "仓库网页地址须为 HTTP(S) 地址，凭据请在认证区域填写",
                ));
            }
        }
        if self.refs.iter().filter(|r| r.kind == "trunk").count() != 1 {
            return Err(invalid("清单须包含一条主线"));
        }
        let mut targets = HashSet::new();
        let mut sources = HashSet::new();
        for item in &self.refs {
            validate_relative(&item.path)?;
            let name = &item.name;
            if !matches!(item.kind.as_str(), "trunk" | "branch" | "tag")
                || name == "@"
                || name == "HEAD"
                || name.ends_with('.')
                || name.starts_with('-')
                || name.contains("..")
                || name.contains("@{")
                || name
                    .chars()
                    .any(|c| c.is_control() || " ~^:?*[\\".contains(c))
                || name
                    .split('/')
                    .any(|p| p.is_empty() || p.starts_with('.') || p.ends_with(".lock"))
            {
                return Err(invalid(format!("无效的 Git 目标名称 {name:?}")));
            }
            if !targets.insert(item.full_ref()) || !sources.insert(&item.path) {
                return Err(invalid(format!("清单中存在重复的来源或目标 {name}")));
            }
        }
        for target in &targets {
            if target
                .match_indices('/')
                .any(|(i, _)| targets.contains(&target[..i]))
            {
                return Err(invalid(format!("Git 引用存在目录名称冲突 {target}")));
            }
        }
        for entry in &self.authors {
            if entry.svn_user.trim().is_empty()
                || entry.git_name.trim().is_empty()
                || !entry.git_email.contains('@')
                || [&entry.git_name, &entry.git_email]
                    .iter()
                    .any(|s| s.contains(['\r', '\n', '<', '>', '\0']))
            {
                return Err(invalid("请检查作者的 SVN 用户名、Git 姓名和邮箱"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn manifest() -> Manifest {
        serde_json::from_value(serde_json::json!({
            "svn_url":"https://svn.example.org/repos/project","uuid":"sample","revision":2,
            "remote":"https://git.example.org/team/project.git",
            "refs":[{"kind":"trunk","path":"trunk","name":"main"}]
        }))
        .unwrap()
    }

    #[test]
    fn rejects_ambiguous_scopes_and_inline_credentials_before_saving() {
        let base = manifest();
        base.validate().unwrap();
        let mut web = base.clone();
        web.web_url = "https://git.example.org/gitweb?p=archive.git;a=tree;h=main".into();
        web.validate().unwrap();
        web.web_url = "https://git.example.org/repo#access_token=private".into();
        assert!(web.validate().is_err());
        for (path, target) in [
            ("../another", "valid"),
            ("branches/a", "main"),
            ("branches/a", "main/fix"),
            ("branches/a", "bad.lock"),
        ] {
            let mut plan = base.clone();
            plan.refs.push(ScopeRef {
                path: path.into(),
                kind: "branch".into(),
                name: target.into(),
            });
            assert!(plan.validate().is_err(), "{path} -> {target}");
        }
        let mut sensitive = base;
        sensitive.svn_url = "https://private-token@svn.example.org/repos".into();
        assert!(
            sensitive.validate().is_err(),
            "credentials must not enter persisted manifests"
        );
    }
}
