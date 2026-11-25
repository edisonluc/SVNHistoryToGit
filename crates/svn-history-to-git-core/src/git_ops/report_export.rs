use crate::config::TaskRecord;
use crate::error::Result;

pub fn export_report_json(task: &TaskRecord) -> Result<String> {
    Ok(serde_json::to_string_pretty(task)?)
}

pub fn export_report_markdown(task: &TaskRecord) -> String {
    let mut md = String::new();
    md.push_str(&format!("# SVNHistoryToGit 迁移审计报告 — {}\n\n", task.name));
    md.push_str(&format!("- **任务 ID**: `{}`\n", task.id));
    md.push_str(&format!("- **状态**: `{}`\n", task.status.as_str()));
    md.push_str(&format!("- **源 SVN**: `{}`\n", task.config.effective_svn_url()));
    if let Some(remote) = &task.config.git_remote_url {
        md.push_str(&format!("- **目标 Git**: `{remote}`\n"));
    }
    md.push_str(&format!("- **工作目录**: `{}`\n", task.config.workdir));
    md.push_str(&format!("- **更新时间**: `{}`\n\n", task.updated_at));

    if let Some(report) = &task.report {
        md.push_str("## 迁移统计\n\n");
        md.push_str("| 指标 | 数值 |\n| :--- | :--- |\n");
        md.push_str(&format!("| 使用引擎 | `{:?}` |\n", report.engine_used));
        md.push_str(&format!("| 耗时 | `{:.2}s` |\n", report.duration_secs));
        md.push_str(&format!("| 已同步 Revision | `r{}` |\n", report.last_synced_rev));
        md.push_str(&format!("| 提交总数 | `{}` |\n", report.commit_count));
        md.push_str(&format!("| 分支数 | `{}` |\n", report.branch_count));
        md.push_str(&format!("| 标签数 | `{}` |\n", report.tag_count));
        md.push_str(&format!("| 最新提交 | `{}` |\n", report.latest_commit));
        md.push_str(&format!(
            "| 自动生成 .gitignore | `{}` |\n",
            if report.gitignore_generated { "是" } else { "否" }
        ));
        md.push_str(&format!("| 过滤排除文件数 | `{}` |\n\n", report.excluded_files_count));

        if let Some(ver) = &report.verification {
            md.push_str("## 字节级树一致性校验\n\n");
            md.push_str(&format!(
                "- **结论**: {}\n",
                if ver.matched {
                    "✅ 100% 字节级一致"
                } else {
                    "❌ 存在差异"
                }
            ));
            md.push_str(&format!(
                "- **比对版本**: SVN `r{}` vs Git `{}`（共核验 {} 个文件）\n",
                ver.svn_revision, ver.git_ref, ver.checked_files
            ));
            if !ver.missing_in_git.is_empty() {
                md.push_str(&format!("- **Git 缺失文件**: {}\n", ver.missing_in_git.join(", ")));
            }
            if !ver.hash_mismatches.is_empty() {
                md.push_str(&format!("- **哈希不一致文件**: {}\n", ver.hash_mismatches.join(", ")));
            }
            md.push('\n');
        }

        if !report.lfs_converted_files.is_empty() {
            md.push_str("## Git LFS 大文件转换清单\n\n");
            for f in &report.lfs_converted_files {
                md.push_str(&format!(
                    "- `{}` ({:.2} MB, oid: `{}`)\n",
                    f.path,
                    f.size_bytes as f64 / (1024.0 * 1024.0),
                    &f.oid[..12.min(f.oid.len())]
                ));
            }
            md.push('\n');
        }

        if !report.externals_found.is_empty() {
            md.push_str("## svn:externals 外链预警\n\n");
            for ext in &report.externals_found {
                md.push_str(&format!("- `{}` → `{}`\n", ext.path, ext.definition));
            }
            md.push('\n');
        }
    }
    md
}

pub fn export_report_html(task: &TaskRecord) -> String {
    let md = export_report_markdown(task);
    let escaped = md
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!(
        r#"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8" />
<title>SVNHistoryToGit 迁移审计报告 — {name}</title>
<style>
  body {{ font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; max-width: 960px; margin: 40px auto; padding: 0 24px; background: #0f172a; color: #e2e8f0; line-height: 1.6; }}
  h1, h2 {{ color: #60a5fa; }}
  pre {{ background: #1e293b; padding: 20px; border-radius: 10px; overflow-x: auto; white-space: pre-wrap; border: 1px solid #334155; }}
</style>
</head>
<body>
  <h1>SVNHistoryToGit 迁移审计报告</h1>
  <pre>{escaped}</pre>
</body>
</html>"#,
        name = task.name,
        escaped = escaped
    )
}
