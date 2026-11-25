use serde::{Deserialize, Serialize};

use crate::error::{Result, SvnHistoryToGitError};
use crate::process::{run_command, CommandOutput, CommandSpec, ProcessRegistry};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProbeStatus {
    Ready,
    Missing,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolStatus {
    pub name: String,
    pub available: bool,
    pub version: String,
    pub required_for: String,
    pub hint: Option<String>,
    // Older reports had only the available flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ProbeStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    pub tools: Vec<ToolStatus>,
    pub all_core_ready: bool,
    pub fast_engine_ready: bool,
    pub lfs_ready: bool,
}

struct Probe {
    name: &'static str,
    program: &'static str,
    args: &'static [&'static str],
    purpose: &'static str,
    install: &'static str,
}

const PROBES: &[Probe] = &[
    Probe {
        name: "svn", program: "svn", args: &["--version", "--quiet"],
        purpose: "浏览 SVN 目录、读取提交历史与校验文件",
        install: "安装 Subversion 命令行客户端。已安装 TortoiseSVN 的用户可在安装程序中选择“修改”，勾选 Command line client tools。确认其 bin 目录已加入 PATH。",
    },
    Probe {
        name: "git", program: "git", args: &["--version"],
        purpose: "创建 Git 提交、推送并校验目标分支",
        install: "安装 Git for Windows，使命令行和第三方软件可以从 PATH 调用 git。",
    },
    Probe {
        name: "git-lfs", program: "git", args: &["lfs", "version"],
        purpose: "启用 Git LFS 时上传大文件对象",
        install: "需要迁移到 Git LFS 时安装 Git LFS，确保 git-lfs 可从 PATH 调用。",
    },
    Probe {
        name: "svnrdump", program: "svnrdump", args: &["--version", "--quiet"],
        purpose: "旧版快速导入引擎从远端读取 SVN 转储",
        install: "使用旧版在线快速导入时，安装包含 svnrdump 的 Subversion 命令行工具，并将其 bin 目录加入 PATH。",
    },
    Probe {
        name: "svnadmin", program: "svnadmin", args: &["--version", "--quiet"],
        purpose: "手工从本地 SVN 仓库制作离线转储文件",
        install: "需要在本机制作转储时，安装包含 svnadmin 的 Subversion 工具并配置 PATH。已有转储文件可直接交给旧版快速导入引擎。",
    },
    Probe {
        name: "git-svn", program: "git", args: &["svn", "--version"],
        purpose: "旧版 Git SVN 兼容引擎与增量同步",
        install: "普通迁移可以直接使用当前向导。Git for Windows 自 2.54 起已移除 git svn；如需旧版兼容引擎，请配置可运行 git svn 的 MSYS2 环境。",
    },
];

impl Probe {
    fn version<'a>(&self, output: &'a CommandOutput) -> Option<&'a str> {
        let prefix = match self.name {
            "git" => "git version ",
            "git-svn" => "git-svn version ",
            "git-lfs" => "git-lfs/",
            _ => "", // Subversion's --quiet probes print a bare version number.
        };
        output
            .stdout
            .lines()
            .chain(output.stderr.lines())
            .map(str::trim)
            .find(|line| {
                line.strip_prefix(prefix)
                    .is_some_and(|number| number.as_bytes().first().is_some_and(u8::is_ascii_digit))
            })
    }

    fn assess(&self, result: Result<CommandOutput>) -> ToolStatus {
        // Keep the legacy version field useful on errors, but never echo a
        // process's arbitrary stderr (it can contain private config values).
        let (state, message, hint) = match result {
            Ok(out) => match self.version(&out) {
                Some(line) => (ProbeStatus::Ready, line.chars().take(200).collect(), ""),
                None => (
                    ProbeStatus::Failed,
                    "命令结束，但没有返回可识别的版本信息。".to_owned(),
                    "在终端运行版本命令，确认使用的是预期的程序。",
                ),
            },
            Err(SvnHistoryToGitError::ExecutableNotFound(_)) if self.name != self.program => (
                ProbeStatus::Failed,
                format!("需要先让 {} 可用，再检测 {}。", self.program, self.name),
                "先处理 Git 的检测结果，再重新检测。",
            ),
            Err(SvnHistoryToGitError::ExecutableNotFound(_)) => (
                ProbeStatus::Missing,
                format!("当前进程找不到 {} 命令。", self.program),
                self.install,
            ),
            Err(SvnHistoryToGitError::CommandFailed { stderr, code, .. }) => {
                if self.program == "git"
                    && self.name != "git"
                    && stderr.lines().any(|line| {
                        line.starts_with(&format!("git: '{}' is not a git command.", self.args[0]))
                    })
                {
                    (
                        ProbeStatus::Missing,
                        format!("当前 Git 找不到 {} 子命令。", self.args[0]),
                        self.install,
                    )
                } else {
                    let detail = code.map_or_else(
                        || "未能正常启动或完成版本检查。".into(),
                        |code| format!("版本检查返回退出码 {code}。"),
                    );
                    (
                        ProbeStatus::Failed,
                        detail,
                        "在终端运行版本命令查看错误，检查命令路径和执行权限。",
                    )
                }
            }
            Err(SvnHistoryToGitError::CommandTimeout { seconds, .. }) => (
                ProbeStatus::Failed,
                format!("版本检查超过 {seconds} 秒。"),
                "稍后重新检测；如果仍超时，在终端运行版本命令查看是否卡住。",
            ),
            Err(_) => (
                ProbeStatus::Failed,
                "读取版本时发生错误。".into(),
                "在终端运行版本命令查看错误，然后重新检测。",
            ),
        };
        let available = state == ProbeStatus::Ready;
        ToolStatus {
            name: self.name.into(),
            available,
            version: message.clone(),
            required_for: self.purpose.into(),
            hint: (!available).then(|| hint.into()),
            status: Some(state),
            detail: (!available).then_some(message),
        }
    }
}

pub async fn run_doctor(registry: &ProcessRegistry) -> DoctorReport {
    let mut tools = Vec::with_capacity(PROBES.len());
    for probe in PROBES {
        let spec = CommandSpec::new(
            probe.program,
            probe.args.iter().map(|arg| (*arg).to_owned()).collect(),
        )
        // Git's unknown-subcommand diagnostic is localized.
        .env("LC_ALL", "C")
        .env("LANGUAGE", "C")
        .timeout(10)
        .log_output(false);
        tools.push(probe.assess(run_command(registry, 0, spec, None).await));
    }
    let is_ok = |name: &str| tools.iter().any(|tool| tool.name == name && tool.available);
    DoctorReport {
        all_core_ready: is_ok("svn") && is_ok("git"),
        fast_engine_ready: is_ok("svnrdump") && is_ok("git"),
        lfs_ready: is_ok("git-lfs"),
        tools,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(name: &str) -> &Probe {
        PROBES.iter().find(|probe| probe.name == name).unwrap()
    }

    fn exit_error(stderr: &str) -> SvnHistoryToGitError {
        SvnHistoryToGitError::CommandFailed {
            command: "git lfs version".into(),
            code: Some(1),
            stderr: stderr.into(),
        }
    }

    #[test]
    fn distinguishes_missing_git_extension_from_a_broken_one() {
        let lfs = probe("git-lfs");
        let missing = lfs.assess(Err(exit_error(
            "git: 'lfs' is not a git command. See 'git --help'.",
        )));
        assert_eq!(missing.status, Some(ProbeStatus::Missing));
        let broken = lfs.assess(Err(exit_error(
            "bad config line in private path: secret-value",
        )));
        assert_eq!(broken.status, Some(ProbeStatus::Failed));
        assert!(!broken.available);
        assert!(!serde_json::to_string(&broken)
            .unwrap()
            .contains("secret-value"));
        let timeout = lfs.assess(Err(SvnHistoryToGitError::CommandTimeout {
            command: "git lfs version".into(),
            seconds: 10,
        }));
        assert!(timeout.detail.unwrap().contains("10 秒"));
    }

    #[tokio::test]
    async fn missing_executable_and_missing_git_parent_are_distinguished() {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("absent-svn").to_str().unwrap().to_owned();
        let result = run_command(
            &ProcessRegistry::new(),
            0,
            CommandSpec::new(program, vec![]),
            None,
        )
        .await;
        assert_eq!(
            probe("svn").assess(result).status,
            Some(ProbeStatus::Missing)
        );
        let unchecked =
            probe("git-svn").assess(Err(SvnHistoryToGitError::ExecutableNotFound("git".into())));
        assert_eq!(unchecked.status, Some(ProbeStatus::Failed));
        assert!(unchecked.detail.unwrap().contains("git"));
    }

    #[test]
    fn reads_a_version_from_either_stream_or_reports_no_version() {
        let output = |out: &str, err: &str| {
            Ok(CommandOutput {
                stdout: out.into(),
                stderr: err.into(),
                exit_code: Some(0),
            })
        };
        let git = probe("git");
        let result = git.assess(output("a wrapper warning", "\n git version 2.49.0\n"));
        assert!(result.available);
        assert_eq!(result.status, Some(ProbeStatus::Ready));
        assert_eq!(result.version, "git version 2.49.0");
        assert_eq!(probe("svn").assess(output("1.14.4", "")).version, "1.14.4");
        assert_eq!(git.assess(output("", "")).status, Some(ProbeStatus::Failed));
    }
}
