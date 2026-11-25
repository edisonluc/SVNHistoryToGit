use std::collections::{BTreeMap, HashMap};
use std::io::BufRead;
use std::sync::Arc;

use crate::error::{Result, Svn2GitError};
use crate::process::decode_bytes;

#[derive(Debug, Clone)]
pub struct DumpRevision {
    pub revision: u64,
    pub author: String,
    pub date: String,
    pub log: String,
    pub nodes: Vec<DumpNode>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
    File,
    Dir,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeAction {
    Add,
    Change,
    Delete,
    Replace,
}

#[derive(Debug, Clone)]
pub struct DumpNode {
    pub path: String,
    pub kind: NodeKind,
    pub action: NodeAction,
    pub copyfrom_path: Option<String>,
    pub copyfrom_rev: Option<u64>,
    pub props: HashMap<String, Vec<u8>>,
    pub text: Option<Vec<u8>>,
}

/// Streaming parser for Subversion Dumpfile Format v2 / v3 (`svnrdump dump` / `svnadmin dump`).
pub struct DumpStreamParser<R: BufRead> {
    reader: R,
    pending_rev_header: Option<HashMap<String, String>>,
    eof: bool,
}

impl<R: BufRead> DumpStreamParser<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            pending_rev_header: None,
            eof: false,
        }
    }

    pub fn next_revision(&mut self) -> Result<Option<DumpRevision>> {
        if self.eof {
            return Ok(None);
        }

        let rev_headers = match self.pending_rev_header.take() {
            Some(h) => h,
            None => loop {
                let Some(headers) = self.read_headers()? else {
                    self.eof = true;
                    return Ok(None);
                };
                if headers.contains_key("Revision-number") {
                    break headers;
                }
                // Skip format version / UUID headers
            },
        };

        let rev_num = rev_headers
            .get("Revision-number")
            .and_then(|v| v.parse::<u64>().ok())
            .ok_or_else(|| Svn2GitError::Svn("missing or invalid Revision-number".into()))?;

        let prop_len = rev_headers
            .get("Prop-content-length")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        let rev_props = if prop_len > 0 {
            let mut buf = vec![0u8; prop_len];
            self.reader.read_exact(&mut buf)?;
            parse_prop_block(&buf)
        } else {
            HashMap::new()
        };

        let author = rev_props
            .get("svn:author")
            .map(|b| decode_bytes(b).trim().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let date = rev_props
            .get("svn:date")
            .map(|b| decode_bytes(b).trim().to_string())
            .unwrap_or_default();
        let log = rev_props
            .get("svn:log")
            .map(|b| decode_bytes(b))
            .unwrap_or_default();

        let mut nodes = Vec::new();
        loop {
            let Some(headers) = self.read_headers()? else {
                self.eof = true;
                break;
            };
            if headers.contains_key("Revision-number") {
                self.pending_rev_header = Some(headers);
                break;
            }
            if !headers.contains_key("Node-path") {
                continue;
            }

            let path = headers
                .get("Node-path")
                .cloned()
                .unwrap_or_default()
                .trim_start_matches('/')
                .to_string();
            let kind = match headers.get("Node-kind").map(String::as_str) {
                Some("file") => NodeKind::File,
                Some("dir") => NodeKind::Dir,
                _ => NodeKind::Unknown,
            };
            let action = match headers.get("Node-action").map(String::as_str) {
                Some("add") => NodeAction::Add,
                Some("delete") => NodeAction::Delete,
                Some("replace") => NodeAction::Replace,
                _ => NodeAction::Change,
            };
            let copyfrom_path = headers
                .get("Node-copyfrom-path")
                .map(|p| p.trim_start_matches('/').to_string());
            let copyfrom_rev = headers
                .get("Node-copyfrom-rev")
                .and_then(|v| v.parse::<u64>().ok());

            let node_prop_len = headers
                .get("Prop-content-length")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(0);
            let node_text_len = headers
                .get("Text-content-length")
                .and_then(|v| v.parse::<usize>().ok());
            let content_len = headers
                .get("Content-length")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(node_prop_len + node_text_len.unwrap_or(0));

            let props = if node_prop_len > 0 {
                let mut buf = vec![0u8; node_prop_len];
                self.reader.read_exact(&mut buf)?;
                parse_prop_block(&buf)
            } else {
                HashMap::new()
            };

            let text = if let Some(tlen) = node_text_len {
                let mut buf = vec![0u8; tlen];
                self.reader.read_exact(&mut buf)?;
                Some(buf)
            } else if content_len > node_prop_len {
                let rem = content_len - node_prop_len;
                let mut buf = vec![0u8; rem];
                self.reader.read_exact(&mut buf)?;
                Some(buf)
            } else {
                None
            };

            nodes.push(DumpNode {
                path,
                kind,
                action,
                copyfrom_path,
                copyfrom_rev,
                props,
                text,
            });
        }

        Ok(Some(DumpRevision {
            revision: rev_num,
            author,
            date,
            log,
            nodes,
        }))
    }

    fn read_headers(&mut self) -> Result<Option<HashMap<String, String>>> {
        let mut headers = HashMap::new();
        let mut line = String::new();

        // Skip leading blank lines
        loop {
            line.clear();
            let n = self.reader.read_line(&mut line)?;
            if n == 0 {
                return Ok(None);
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if !trimmed.is_empty() {
                if let Some((k, v)) = trimmed.split_once(':') {
                    headers.insert(k.trim().to_string(), v.trim().to_string());
                }
                break;
            }
        }

        // Read remaining header lines until empty line
        loop {
            line.clear();
            let n = self.reader.read_line(&mut line)?;
            if n == 0 {
                break;
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            if let Some((k, v)) = trimmed.split_once(':') {
                headers.insert(k.trim().to_string(), v.trim().to_string());
            }
        }

        Ok(Some(headers))
    }
}

fn parse_prop_block(data: &[u8]) -> HashMap<String, Vec<u8>> {
    let mut map = HashMap::new();
    let mut idx = 0usize;
    let mut current_key: Option<String> = None;

    while idx < data.len() {
        let Some(nl) = data[idx..].iter().position(|&b| b == b'\n') else {
            break;
        };
        let line = std::str::from_utf8(&data[idx..idx + nl])
            .unwrap_or("")
            .trim_end_matches('\r');
        idx += nl + 1;

        if line == "PROPS-END" {
            break;
        }
        if let Some(len_str) = line.strip_prefix("K ") {
            if let Ok(len) = len_str.trim().parse::<usize>() {
                if idx + len <= data.len() {
                    current_key = Some(decode_bytes(&data[idx..idx + len]));
                    idx += len;
                    if idx < data.len() && data[idx] == b'\n' {
                        idx += 1;
                    }
                }
            }
        } else if let Some(len_str) = line.strip_prefix("V ") {
            if let Ok(len) = len_str.trim().parse::<usize>() {
                if idx + len <= data.len() {
                    if let Some(k) = current_key.take() {
                        map.insert(k, data[idx..idx + len].to_vec());
                    }
                    idx += len;
                    if idx < data.len() && data[idx] == b'\n' {
                        idx += 1;
                    }
                }
            }
        } else if let Some(len_str) = line.strip_prefix("D ") {
            if let Ok(len) = len_str.trim().parse::<usize>() {
                if idx + len <= data.len() {
                    let k = decode_bytes(&data[idx..idx + len]);
                    map.remove(&k);
                    idx += len;
                    if idx < data.len() && data[idx] == b'\n' {
                        idx += 1;
                    }
                }
            }
        }
    }

    map
}

/// Structural-sharing file entry stored in the in-memory versioned tree index.
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub mark_id: u64,
    pub mode: &'static str, // "100644", "100755", "120000"
}

/// Persistent versioned tree index using `Arc<BTreeMap<...>>` structural sharing per revision
/// so SVN `Node-copyfrom-path` / `Node-copyfrom-rev` resolves in microseconds.
#[derive(Debug, Default)]
pub struct VersionedTreeIndex {
    snapshots: HashMap<u64, Arc<BTreeMap<String, FileEntry>>>,
    current: BTreeMap<String, FileEntry>,
}

impl VersionedTreeIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put_file(&mut self, path: String, entry: FileEntry) {
        self.current.insert(path, entry);
    }

    pub fn delete_path(&mut self, path: &str) {
        self.current.remove(path);
        let prefix = format!("{path}/");
        self.current.retain(|k, _| !k.starts_with(&prefix));
    }

    /// Copy a file or directory subtree from a historical revision `copyfrom_rev`.
    pub fn copy_from(
        &mut self,
        copyfrom_rev: u64,
        copyfrom_path: &str,
        dest_path: &str,
    ) -> Vec<(String, FileEntry)> {
        let Some(snapshot) = self.snapshots.get(&copyfrom_rev).cloned() else {
            return Vec::new();
        };

        let mut copied = Vec::new();
        if let Some(entry) = snapshot.get(copyfrom_path) {
            self.current.insert(dest_path.to_string(), entry.clone());
            copied.push((dest_path.to_string(), entry.clone()));
            return copied;
        }

        let src_prefix = if copyfrom_path.is_empty() {
            String::new()
        } else {
            format!("{copyfrom_path}/")
        };

        for (k, entry) in snapshot.iter() {
            let rel = if src_prefix.is_empty() {
                Some(k.as_str())
            } else {
                k.strip_prefix(&src_prefix)
            };
            if let Some(rel_path) = rel {
                let target = if dest_path.is_empty() {
                    rel_path.to_string()
                } else {
                    format!("{dest_path}/{rel_path}")
                };
                self.current.insert(target.clone(), entry.clone());
                copied.push((target, entry.clone()));
            }
        }
        copied
    }

    pub fn commit_snapshot(&mut self, rev: u64) {
        self.snapshots.insert(rev, Arc::new(self.current.clone()));
    }

    pub fn files_under(&self, prefix: &str) -> Vec<(String, FileEntry)> {
        let p = if prefix.is_empty() {
            String::new()
        } else {
            format!("{prefix}/")
        };
        self.current
            .iter()
            .filter_map(|(k, v)| {
                if p.is_empty() {
                    Some((k.clone(), v.clone()))
                } else {
                    k.strip_prefix(&p).map(|rel| (rel.to_string(), v.clone()))
                }
            })
            .collect()
    }
}
