use std::collections::{HashMap, HashSet};

use anyhow::{bail, Result};
use serde_json::{json, Value};
use svn2git_core::guided::{convert, jobs};
use svn2git_core::storage::{AuthorEntry, Storage};

pub(super) fn save(storage: &Storage, authors: &[AuthorEntry]) -> Result<Value> {
    let mut users = HashSet::new();
    for author in authors {
        let user = author.svn_user.trim();
        let name = author.git_name.trim();
        let email = author.git_email.trim();
        let valid_email = email.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty() && !domain.is_empty() && !domain.contains('@')
        });
        if user.is_empty()
            || user.chars().any(char::is_control)
            || name.is_empty()
            || name.contains(['<', '>'])
            || name.chars().any(char::is_control)
            || !valid_email
            || email.contains(['<', '>'])
            || email.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            bail!("请检查 SVN 用户 {user:?} 的用户名、Git 姓名和邮箱");
        }
        if !users.insert(user) {
            bail!("SVN 用户名重复：{user}");
        }
    }
    storage.upsert_authors(authors)?;
    Ok(json!({}))
}

pub(super) fn remove(storage: &Storage, user: &str) -> Result<Value> {
    let user = user.trim();
    if user.is_empty() {
        bail!("请指定要删除的 SVN 用户名");
    }
    storage.delete_author(user)?;
    Ok(json!({}))
}

pub(super) async fn scan(
    storage: &Storage,
    url: &str,
    revision: u64,
    paths: &[String],
    credential_id: Option<&str>,
) -> Result<Value> {
    let secret = jobs::credential(storage, credential_id, true)?;
    let users = convert::scan_authors(url, revision, paths, secret.as_ref()).await?;
    let mut book: HashMap<_, _> = storage
        .list_authors()?
        .into_iter()
        .map(|row| (row.svn_user.clone(), row))
        .collect();
    let mut rows = Vec::with_capacity(users.len());
    for (user, count) in users {
        let saved = book.remove(&user);
        let in_library = saved.is_some();
        let mut row = saved.unwrap_or_else(|| AuthorEntry {
            git_name: user.clone(),
            git_email: format!("{user}@svn.local"),
            svn_user: user,
            commit_count: 0,
            updated_at: String::new(),
        });
        row.commit_count = count;
        let mut result = json!(row);
        result["in_library"] = json!(in_library);
        rows.push(result);
    }
    Ok(Value::Array(rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_batches_without_partially_updating_the_library() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path()).unwrap();
        let make = |user: &str, email: &str| {
            serde_json::from_value::<AuthorEntry>(json!({
                "svn_user":user, "git_name":"Alice", "git_email":email
            }))
            .unwrap()
        };
        let alice = make("alice", "alice@example.org");
        save(&storage, std::slice::from_ref(&alice)).unwrap();
        for bad in [
            make("bob", "bad@"),
            make("bob", "bad name@example.org"),
            make(" alice ", "duplicate@example.org"),
        ] {
            assert!(save(&storage, &[make("alice", "new@example.org"), bad]).is_err());
            let book = storage.list_authors().unwrap();
            assert_eq!(book.len(), 1);
            assert_eq!(book[0].git_email, "alice@example.org");
        }
        assert!(remove(&storage, " ").is_err());
        remove(&storage, "alice").unwrap();
        assert!(storage.list_authors().unwrap().is_empty());
    }
}
