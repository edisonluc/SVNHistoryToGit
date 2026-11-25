use std::collections::{BTreeMap, HashSet};

use super::{source, Result, Svn};
use crate::credential::ResolvedSecret;
use crate::guided::invalid;

/// Count each SVN revision once, including shared ancestors of selected copies.
pub async fn scan_authors(
    project: &str,
    revision: u64,
    paths: &[String],
    secret: Option<&ResolvedSecret>,
) -> Result<BTreeMap<String, u64>> {
    if revision == 0 || paths.is_empty() {
        return Err(invalid("请选择要扫描的 SVN 路径和修订"));
    }
    let svn = Svn { secret };
    let mut seen = HashSet::new();
    let mut authors = BTreeMap::new();
    for path in paths {
        let target = format!("{}@{revision}", source::at_path(project, path)?);
        for entry in svn.history(&target, revision, false).await? {
            if seen.insert(entry.rev) {
                *authors.entry(entry.author).or_default() += 1;
            }
        }
    }
    Ok(authors)
}
