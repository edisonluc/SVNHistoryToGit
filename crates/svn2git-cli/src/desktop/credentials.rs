use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use svn2git_core::credential::{CredentialKind, CredentialProfile, KeyringVault};
use svn2git_core::error::Result as CoreResult;
use svn2git_core::storage::Storage;

#[derive(Deserialize)]
pub(super) struct SaveCredential {
    id: Option<String>,
    kind: CredentialKind,
    username: String,
    name: Option<String>,
    host_pattern: Option<String>,
    ssh_key_path: Option<String>,
    secret: Option<String>,
}

trait Vault {
    fn read(&self, id: &str) -> CoreResult<Option<String>>;
    fn write(&self, id: &str, secret: Option<&str>) -> CoreResult<()>;
}

impl Vault for KeyringVault {
    fn read(&self, id: &str) -> CoreResult<Option<String>> {
        KeyringVault::optional_secret(id)
    }

    fn write(&self, id: &str, secret: Option<&str>) -> CoreResult<()> {
        match secret {
            Some(value) => KeyringVault::store_secret(id, value),
            None => KeyringVault::delete_secret(id),
        }
    }
}

enum SecretChange<'a> {
    Keep,
    Write(Option<&'a str>),
}

fn persist(
    storage: &Storage,
    vault: &impl Vault,
    id: &str,
    original: Option<&CredentialProfile>,
    updated: Option<&CredentialProfile>,
    change: SecretChange<'_>,
) -> Result<()> {
    storage.change_credential(id, original, updated, || {
        let previous = match change {
            SecretChange::Keep => None,
            SecretChange::Write(replacement) => {
                let previous = vault.read(id)?;
                vault.write(id, replacement)?;
                Some(previous)
            }
        };
        Ok(move || {
            if let Some(previous) = previous {
                vault.write(id, previous.as_deref())?;
            }
            Ok(())
        })
    })?;
    Ok(())
}

fn valid_host(host: &str) -> bool {
    if host.is_empty() {
        return true;
    }
    if host.contains(['/', '\\', '%', '@', '?', '#', '*'])
        || host.chars().any(|c| c.is_whitespace() || c.is_control())
        || host.ends_with(':')
    {
        return false;
    }
    url::Url::parse(&format!("http://{host}/")).is_ok_and(|u| u.host().is_some())
}

pub(super) fn save(storage: &Storage, profile: SaveCredential) -> Result<Value> {
    save_with(storage, &KeyringVault, profile)
}

fn save_with(storage: &Storage, vault: &impl Vault, input: SaveCredential) -> Result<Value> {
    let original = match input.id.as_deref().filter(|id| !id.is_empty()) {
        Some(id) => Some(
            storage
                .get_credential(id)?
                .context("凭据已移除，请重新选择")?,
        ),
        None => None,
    };
    let username = input.username.trim();
    if username.is_empty() {
        bail!("请输入用户名");
    }
    let mut profile = original.clone().unwrap_or_else(|| CredentialProfile {
        id: uuid::Uuid::new_v4().to_string(),
        kind: input.kind,
        name: username.to_owned(),
        username: String::new(),
        host_pattern: String::new(),
        ssh_key_path: None,
        created_at: chrono::Utc::now().to_rfc3339(),
    });
    if input.kind != profile.kind {
        bail!("凭据类型已经改变，请新建凭据");
    }
    profile.username = username.to_owned();
    if let Some(name) = input.name {
        profile.name = name.trim().to_owned();
        if profile.name.is_empty() {
            bail!("请输入显示名称");
        }
    }
    if let Some(host) = input.host_pattern {
        let host = host.trim();
        if host != profile.host_pattern {
            if !valid_host(host) {
                bail!("服务器须填写主机名或主机名与端口");
            }
            profile.host_pattern = host.to_lowercase();
        }
    }
    if input.kind == CredentialKind::GitSshKey {
        if let Some(path) = input.ssh_key_path {
            let path = path.trim();
            if profile.ssh_key_path.as_deref() != Some(path) {
                if !Path::new(path).is_file() || std::fs::File::open(path).is_err() {
                    bail!("请选择可读的 SSH 私钥文件");
                }
                profile.ssh_key_path = Some(path.to_owned());
            }
        }
        if profile
            .ssh_key_path
            .as_deref()
            .unwrap_or_default()
            .is_empty()
        {
            bail!("请选择可读的 SSH 私钥文件");
        }
    } else if input.secret.as_deref() == Some("") || original.is_none() && input.secret.is_none() {
        bail!("请输入密码或访问令牌");
    }
    let change = match input.secret.as_deref() {
        Some(secret) => SecretChange::Write(Some(secret)),
        None if original.is_none() => SecretChange::Write(Some("")),
        None => SecretChange::Keep,
    };
    persist(
        storage,
        vault,
        &profile.id,
        original.as_ref(),
        Some(&profile),
        change,
    )?;
    Ok(json!(profile))
}

pub(super) fn remove(storage: &Storage, id: &str) -> Result<Value> {
    remove_with(storage, &KeyringVault, id)
}

fn remove_with(storage: &Storage, vault: &impl Vault, id: &str) -> Result<Value> {
    if let Some(profile) = storage.get_credential(id)? {
        persist(
            storage,
            vault,
            id,
            Some(&profile),
            None,
            SecretChange::Write(None),
        )?;
    }
    Ok(json!({"id": id}))
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;

    use super::*;
    use svn2git_core::error::Svn2GitError;

    #[derive(Default)]
    struct TestVault {
        entries: RefCell<HashMap<String, String>>,
        unreadable: Cell<bool>,
        fail_write: Cell<bool>,
    }

    impl Vault for TestVault {
        fn read(&self, id: &str) -> CoreResult<Option<String>> {
            assert!(
                !self.unreadable.get(),
                "metadata-only edits must not access the vault"
            );
            Ok(self.entries.borrow().get(id).cloned())
        }

        fn write(&self, id: &str, secret: Option<&str>) -> CoreResult<()> {
            if self.fail_write.get() {
                return Err(Svn2GitError::Credential("vault unavailable".into()));
            }
            let mut entries = self.entries.borrow_mut();
            if let Some(value) = secret {
                entries.insert(id.to_string(), value.to_string());
            } else {
                entries.remove(id);
            }
            Ok(())
        }
    }

    fn input(fields: Value) -> SaveCredential {
        let mut record = json!({"kind":"svn_password","username":"alice"});
        for (key, value) in fields.as_object().unwrap() {
            record[key] = value.clone();
        }
        serde_json::from_value(record).unwrap()
    }

    #[test]
    fn rename_rotate_and_remove_without_exposing_secrets_or_changing_id() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path()).unwrap();
        let vault = TestVault::default();
        let create =
            || input(json!({"name":"团队","host_pattern":"svn.test:8443","secret":"initial"}));
        vault.fail_write.set(true);
        assert!(save_with(&storage, &vault, create()).is_err());
        assert!(storage.list_credentials().unwrap().is_empty());
        vault.fail_write.set(false);
        let saved = save_with(&storage, &vault, create()).unwrap();
        let id = saved["id"].as_str().unwrap();
        assert!(!saved.to_string().contains("initial"));
        let same_host = save_with(
            &storage,
            &vault,
            input(json!({
                "name":"备用","host_pattern":"svn.test:8443","secret":"second"
            })),
        )
        .unwrap();
        assert_ne!(same_host["id"], id);

        vault.unreadable.set(true);
        let renamed = save_with(
            &storage,
            &vault,
            input(json!({"id":id,"name":"发布","host_pattern":"new.test:8443"})),
        )
        .unwrap();
        assert_eq!(renamed["name"], "发布");
        assert_eq!(renamed["created_at"], saved["created_at"]);
        assert_eq!(vault.entries.borrow()[id], "initial");
        vault.unreadable.set(false);
        let rotation = || input(json!({"id":id, "username":"bob", "secret":"rotated"}));
        vault.fail_write.set(true);
        assert!(save_with(&storage, &vault, rotation()).is_err());
        assert_eq!(
            storage.get_credential(id).unwrap().unwrap().username,
            "alice"
        );
        assert_eq!(vault.entries.borrow()[id], "initial");
        vault.fail_write.set(false);
        let updated = save_with(&storage, &vault, rotation()).unwrap();
        assert_eq!(updated["id"], id);
        assert_eq!(updated["host_pattern"], "new.test:8443");
        assert_eq!(vault.entries.borrow()[id], "rotated");
        vault.fail_write.set(true);
        assert!(remove_with(&storage, &vault, id).is_err());
        assert!(storage.get_credential(id).unwrap().is_some());
        vault.fail_write.set(false);
        remove_with(&storage, &vault, id).unwrap();
        remove_with(&storage, &vault, id).unwrap();
        assert!(storage.get_credential(id).unwrap().is_none());
        assert!(!vault.entries.borrow().contains_key(id));
        assert!(save_with(&storage, &vault, rotation()).is_err());
        assert_eq!(storage.list_credentials().unwrap().len(), 1);
    }

    #[test]
    fn ssh_key_path_and_optional_passphrase_can_be_changed_independently() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path()).unwrap();
        let vault = TestVault::default();
        let first_key = dir.path().join("first");
        let next_key = dir.path().join("second");
        std::fs::write(&first_key, b"fixture").unwrap();
        std::fs::write(&next_key, b"fixture").unwrap();
        let create =
            input(json!({"kind":"git_ssh_key","secret":"passphrase","ssh_key_path":first_key}));
        let saved = save_with(&storage, &vault, create).unwrap();
        let id = saved["id"].as_str().unwrap();
        std::fs::remove_file(&first_key).unwrap();
        vault.unreadable.set(true);
        save_with(
            &storage,
            &vault,
            input(json!({"kind":"git_ssh_key","id":id,"name":"新名称"})),
        )
        .unwrap();
        save_with(
            &storage,
            &vault,
            input(json!({"kind":"git_ssh_key","id":id,"ssh_key_path":next_key})),
        )
        .unwrap();
        vault.unreadable.set(false);
        assert_eq!(vault.entries.borrow()[id], "passphrase");
        save_with(
            &storage,
            &vault,
            input(json!({"kind":"git_ssh_key","id":id,"secret":""})),
        )
        .unwrap();
        assert_eq!(vault.entries.borrow()[id], "");
        assert!(save_with(
            &storage,
            &vault,
            input(json!({"kind":"git_https_token","id":id,"secret":"value"}))
        )
        .is_err());
        assert!(save_with(
            &storage,
            &vault,
            input(json!({"kind":"git_https_token","secret":""}))
        )
        .is_err());
        assert!(save_with(
            &storage,
            &vault,
            input(json!({"secret":"value","host_pattern":"https://u:secret@example.com/path"}))
        )
        .is_err());
    }
}
