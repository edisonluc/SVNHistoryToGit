use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::error::{Result, Svn2GitError};

const KEYRING_SERVICE: &str = "svn2git-desktop";

pub const ENV_ASKPASS_ADDR: &str = "SVN2GIT_ASKPASS_ADDR";
pub const ENV_ASKPASS_TOKEN: &str = "SVN2GIT_ASKPASS_TOKEN";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    SvnPassword,
    GitHttpsToken,
    GitSshKey,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialProfile {
    pub id: String,
    pub name: String,
    pub kind: CredentialKind,
    pub host_pattern: String,
    pub username: String,
    /// Optional SSH private key path when kind == GitSshKey
    #[serde(default)]
    pub ssh_key_path: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct ResolvedSecret {
    pub username: String,
    pub secret: String,
    pub ssh_key_path: Option<String>,
}

/// Stores secrets securely in Windows Credential Manager (OS Keyring).
pub struct KeyringVault;

impl KeyringVault {
    pub fn store_secret(profile_id: &str, secret: &str) -> Result<()> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, profile_id)
            .map_err(|e| Svn2GitError::Credential(e.to_string()))?;
        entry
            .set_password(secret)
            .map_err(|e| Svn2GitError::Credential(e.to_string()))?;
        Ok(())
    }

    pub fn load_secret(profile_id: &str) -> Result<String> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, profile_id)
            .map_err(|e| Svn2GitError::Credential(e.to_string()))?;
        entry
            .get_password()
            .map_err(|e| Svn2GitError::Credential(e.to_string()))
    }

    pub fn optional_secret(profile_id: &str) -> Result<Option<String>> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, profile_id)
            .map_err(|e| Svn2GitError::Credential(e.to_string()))?;
        match entry.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(Svn2GitError::Credential(e.to_string())),
        }
    }

    pub fn delete_secret(profile_id: &str) -> Result<()> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, profile_id)
            .map_err(|e| Svn2GitError::Credential(e.to_string()))?;
        match entry.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(Svn2GitError::Credential(e.to_string())),
        }
    }
}

/// Local ephemeral loopback credential server for `<2ms` native `GIT_ASKPASS` / `SSH_ASKPASS`.
/// Never writes passwords to disk or passes them via CLI arguments.
pub struct AskPassServer {
    addr: String,
    token: String,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl AskPassServer {
    pub async fn start(username: String, password: String) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?.to_string();
        let token = Uuid::new_v4().to_string();
        let expected_token = token.clone();
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accept_res = listener.accept() => {
                        let Ok((stream, _)) = accept_res else { break };
                        let expected = expected_token.clone();
                        let user = username.clone();
                        let pass = password.clone();
                        tokio::spawn(async move {
                            let (reader, mut writer) = stream.into_split();
                            let mut buf_reader = BufReader::new(reader);
                            let mut line = String::new();
                            if buf_reader.read_line(&mut line).await.is_ok() {
                                let trimmed = line.trim();
                                let mut parts = trimmed.splitn(2, ' ');
                                let got_token = parts.next().unwrap_or("");
                                let prompt = parts.next().unwrap_or("").to_lowercase();
                                if got_token == expected {
                                    let reply = if prompt.starts_with("username") || prompt.starts_with("user name") {
                                        user
                                    } else {
                                        pass
                                    };
                                    let _ = writer.write_all(reply.as_bytes()).await;
                                    let _ = writer.write_all(b"\n").await;
                                }
                            }
                            let _ = writer.shutdown().await;
                        });
                    }
                }
            }
        });

        Ok(Self {
            addr,
            token,
            shutdown_tx: Some(shutdown_tx),
        })
    }

    /// Build environment variables for `git` / `git svn` to use our built-in native askpass binary.
    pub fn build_env(&self, askpass_binary: Option<&PathBuf>) -> HashMap<String, String> {
        let mut env = HashMap::new();
        env.insert("GIT_TERMINAL_PROMPT".to_string(), "0".to_string());
        env.insert(ENV_ASKPASS_ADDR.to_string(), self.addr.clone());
        env.insert(ENV_ASKPASS_TOKEN.to_string(), self.token.clone());
        if let Some(bin) = askpass_binary {
            let bin_str = bin.to_string_lossy().to_string();
            env.insert("GIT_ASKPASS".to_string(), bin_str.clone());
            env.insert("SSH_ASKPASS".to_string(), bin_str);
            env.insert("SSH_ASKPASS_REQUIRE".to_string(), "force".to_string());
        }
        env
    }
}

impl Drop for AskPassServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// Client helper executed inside `svn2git-cli --internal-askpass <prompt>` (<2ms latency).
pub fn execute_internal_askpass(prompt: &str) -> Result<String> {
    use std::io::{BufRead, Write};
    use std::net::TcpStream;

    let addr = std::env::var(ENV_ASKPASS_ADDR)
        .map_err(|_| Svn2GitError::Credential("missing SVN2GIT_ASKPASS_ADDR".into()))?;
    let token = std::env::var(ENV_ASKPASS_TOKEN)
        .map_err(|_| Svn2GitError::Credential("missing SVN2GIT_ASKPASS_TOKEN".into()))?;

    let mut stream = TcpStream::connect(&addr)?;
    let sanitized_prompt = prompt.replace(['\r', '\n'], " ");
    writeln!(stream, "{token} {sanitized_prompt}")?;
    stream.flush()?;

    let mut reader = std::io::BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

/// Locate the sibling `svn2git-cli` executable (or current exe if it supports `--internal-askpass`).
pub fn resolve_askpass_binary() -> Option<PathBuf> {
    let current = std::env::current_exe().ok()?;
    let dir = current.parent()?;
    #[cfg(windows)]
    let candidate = dir.join("svn2git-cli.exe");
    #[cfg(not(windows))]
    let candidate = dir.join("svn2git-cli");
    if candidate.exists() {
        return Some(candidate);
    }
    Some(current)
}
