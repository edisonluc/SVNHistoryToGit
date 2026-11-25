package main

import (
	"fmt"
	"slices"
	"sort"

	"github.com/egoist/mygo/ui"
)

const doctorBusy = "检测本机工具"

// These are the version probes used by the core. Only fixed vendor URLs are opened.
const svnPackages = "https://subversion.apache.org/packages.html#windows"

var doctorHelp = map[string]struct{ command, url string }{
	"svn":      {"svn --version --quiet", svnPackages},
	"git":      {"git --version", "https://git-scm.com/install/windows"},
	"git-lfs":  {"git lfs version", "https://git-lfs.com/"},
	"svnrdump": {"svnrdump --version --quiet", svnPackages},
	"svnadmin": {"svnadmin --version --quiet", svnPackages},
	"git-svn":  {"git svn --version", "https://github.com/git-for-windows/git/releases/tag/v2.54.0.windows.1"},
}

func (a *app) diagnose() {
	if a.busy != "" {
		return
	}
	if !a.doctorOpen {
		a.doctorMore = false
	}
	a.doctorOpen = true
	a.tools, a.doctorErr, a.doctorNotice = nil, "", ""
	problem, message := a.problem, a.msg
	var reply struct {
		Tools []toolStatus `json:"tools"`
	}
	a.start(doctorBusy, func() error {
		if err := a.engine.call(request{"op": "doctor"}, &reply, nil); err != nil {
			return err
		}
		if len(reply.Tools) == 0 {
			return fmt.Errorf("迁移核心没有返回工具检测结果")
		}
		return nil
	}, func(err error) {
		a.problem, a.msg = problem, message
		if err != nil {
			a.doctorErr = safeDiagnostic(err.Error())
			return
		}
		a.tools = reply.Tools
	})
}

func (a *app) diagnosticTools() (required, optional []toolStatus) {
	names := []string{"svn", "git"}
	lfs := a.d.LFSEnabled
	if a.d.Step == 3 {
		lfs = a.job.Manifest.LFSThreshold != nil
	}
	if lfs {
		names = append(names, "git-lfs")
	}
	// Missing report entries cannot certify that a required tool is available.
	for _, name := range names {
		tool := toolStatus{Name: name, Status: "unchecked", Hint: "请重新检测；若仍无结果，请确认桌面程序与迁移核心来自同一压缩包。"}
		for _, candidate := range a.tools {
			if candidate.Name == name {
				tool = candidate
				break
			}
		}
		required = append(required, tool)
	}
	for _, tool := range a.tools {
		if slices.Contains(names, tool.Name) {
			continue
		}
		optional = append(optional, tool)
	}
	sort.SliceStable(required, func(i, j int) bool { return !required[i].Available && required[j].Available })
	return required, optional
}

func (a *app) diagnosisView(c *ui.Context) {
	if !a.doctorOpen {
		return
	}
	required, optional := a.diagnosticTools()
	issues := 0
	for _, tool := range required {
		if !tool.Available {
			issues++
		}
	}
	checking := a.busy == doctorBusy
	ui.DialogBase(c, &a.doctorOpen, func(back, panel ui.Element) {
		w, h := c.Size()
		height := float32(340 + 65*(len(required)-2) + 115*issues)
		if a.doctorMore {
			height = 615
		}
		if checking || a.doctorErr != "" {
			height = 245
		}
		back.Background(ui.RGBA(0, 0, 0, 0.4))
		panel.Label("工具诊断").Width(min(650, w-32)).Height(min(height, 660, h-32)).Padding(18).Gap(12).
			Background(paper).Border(1, line).Radius(14).Shadow(0, 12, 38, 0, ui.RGBA(0, 0, 0, 0.18))
		panel.Children(func() {
			sectionTitle(c, glyphTools, "工具诊断")
			ui.Scroll(c.Key("doctor-content")).Grow(1).MinHeight(0).FillWidth().Children(func() {
				ui.Column(c).FillWidth().Gap(10).Children(func() {
					switch {
					case checking:
						ui.Text(c, "正在检测本机命令…").Role(ui.RoleStatus)
					case a.doctorErr != "":
						ui.Text(c, "工具检测未完成").Bold().TextColor(warn).Role(ui.RoleAlert)
						hint(c, a.doctorErr)
					default:
						summary, color := "本次迁移工具已就绪", ink
						if issues > 0 {
							summary, color = fmt.Sprintf("本次迁移有 %d 项工具需要处理", issues), warn
						}
						ui.Text(c, summary).FontSize(17).Bold().TextColor(color).Role(ui.RoleStatus)
						if len(required) == 3 {
							hint(c, "当前配置启用了 Git LFS，需要 SVN、Git 和 Git LFS。")
						} else {
							hint(c, "常规迁移只需 SVN 和 Git 命令行工具，其他工具按需安装。")
						}
						a.doctorRows(c, required, true)
						if len(optional) > 0 {
							ui.Collapsible(c.Key("doctor-optional"), fmt.Sprintf("可选工具 · %d 项", len(optional)), &a.doctorMore, func() {
								ui.Column(c).FillWidth().Padding(6, 0).Gap(8).Children(func() {
									hint(c, "以下工具按功能选用，缺少时仍可继续当前向导。")
									a.doctorRows(c, optional, false)
								})
							})
						}
						if issues > 0 || a.doctorMore {
							hint(c, "补装后点「重新检测」。如果安装时修改了 PATH，请从开始菜单或文件资源管理器重新打开 SVNHistoryToGit。")
						}
					}
				})
			})
			if a.doctorNotice != "" {
				ui.Text(c, a.doctorNotice).FontSize(12).TextColor(muted).Role(ui.RoleStatus)
			}
			ui.Row(c).FillWidth().AlignItems(ui.Center).Gap(8).Children(func() {
				ui.Button(c, "重新检测").Disabled(a.busy != "").OnClick(a.diagnose)
				ui.Spacer(c)
				primary(c, "关闭诊断", false, func() { a.doctorOpen = false })
			})
		})
	})
}

func (a *app) doctorRows(c *ui.Context, tools []toolStatus, required bool) {
	ui.Column(c).FillWidth().Background(theme.Surface).Border(1, line).Radius(10).Clip().Children(func() {
		for i, tool := range tools {
			row := ui.Column(c.Key(tool.Name)).FillWidth().Padding(10, 12).Gap(4)
			if i < len(tools)-1 {
				row.BorderWidth(0, 0, 1, 0).BorderColor(line)
			}
			row.Children(func() {
				label, color := "可用", muted
				if !tool.Available {
					if required {
						color = warn
					}
					switch tool.Status {
					case "missing":
						label = "未找到"
					case "failed":
						label = "检测失败"
					case "unchecked":
						label = "未检测"
					default:
						label = "未就绪"
					}
				}
				ui.Row(c).FillWidth().AlignItems(ui.Center).Gap(8).Children(func() {
					ui.Text(c, tool.Name).FontSize(14).Bold()
					ui.Text(c, label).FontSize(12).TextColor(color)
					if tool.Available && tool.Version != "" {
						ui.Spacer(c)
						ui.Text(c, tool.Version).MinWidth(0).SingleLine().FontSize(12).TextColor(muted).Tooltip(tool.Version).Selectable()
					}
				})
				if tool.Purpose != "" {
					hint(c, tool.Purpose)
				}
				if tool.Available {
					return
				}
				if tool.Detail != "" {
					hint(c, safeDiagnostic(tool.Detail))
				}
				if tool.Hint != "" {
					ui.Text(c, tool.Hint).FontSize(13).Selectable()
				}
				if tool.Status == "unchecked" {
					return
				}
				help, ok := doctorHelp[tool.Name]
				if !ok {
					return
				}
				ui.Row(c).FillWidth().Wrap().Gap(6).Children(func() {
					ui.Button(c, "打开安装说明").Label(tool.Name + " 安装说明").OnClick(func() {
						c.OpenURLThen(help.url, func(err error) {
							if err != nil {
								a.doctorNotice = "无法打开浏览器：" + safeDiagnostic(err.Error())
							}
						})
					})
					ui.Button(c, "复制检测命令").Label("复制 " + tool.Name + " 检测命令").Tooltip(help.command).OnClick(func() {
						c.WriteClipboardThen(help.command, func(err error) {
							if err != nil {
								a.doctorNotice = "复制失败：" + safeDiagnostic(err.Error())
								return
							}
							a.doctorNotice = "已复制：" + help.command
						})
					})
					if tool.Name == "git-lfs" && required && a.d.Step == 2 {
						ui.Button(c, "关闭本次 Git LFS").Tooltip("改为用普通 Git 文件迁移").OnClick(func() {
							problem := a.problem
							a.d.LFSEnabled = false
							if !a.persist() {
								a.d.LFSEnabled = true
								a.doctorNotice, a.problem = a.problem, problem
								return
							}
							a.doctorNotice = "已关闭本次 Git LFS，将按普通 Git 文件迁移。"
						})
					}
				})
			})
		}
	})
}
