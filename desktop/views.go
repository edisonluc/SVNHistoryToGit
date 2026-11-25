package main

import (
	"fmt"
	"strings"

	"github.com/egoist/mygo/ui"
)

var (
	accent = ui.Hex("#4F46E5")
	ink    = ui.Hex("#111827")
	muted  = ui.Hex("#6B7280")
	line   = ui.Hex("#E5E7EB")
	paper  = ui.Hex("#ffffff")
	canvas = ui.Hex("#F3F4F6")
	tint   = ui.Hex("#EEF0FB")
	warn   = ui.Hex("#B42318")
	theme  = workspaceTheme()
	steps  = []string{"SVN 项目", "Git 目标", "确认迁移", "运行与结果"}
	titles = []string{"选择 SVN 项目", "选择 Git 目标", "确认迁移", "运行与结果"}
)

func workspaceTheme() *ui.Theme {
	t := ui.LightTheme().WithAccent(accent)
	t.Background, t.Surface = paper, ui.Hex("#F7F8FA")
	t.SurfaceHover, t.SurfacePressed = ui.Hex("#EEF0F4"), ui.Hex("#E2E5EA")
	t.Text, t.TextMuted, t.Border = ink, muted, line
	t.AccentHover, t.AccentPressed = ui.Hex("#4338CA"), ui.Hex("#3730A3")
	t.AccentText, t.InactiveAccent = paper, accent
	t.Danger, t.Warning, t.Success = ui.Hex("#DC2626"), ui.Hex("#D97706"), ui.Hex("#16A34A")
	t.Selection, t.Focus = ui.RGBA(79, 70, 229, 0.16), ui.RGBA(79, 70, 229, 0.55)
	t.Radius, t.FontSize, t.Spacing = 8, 13, 4
	t.ScrollbarWidth = 6
	return t
}

func panel(c *ui.Context) ui.Element {
	return ui.Column(c).Background(theme.Surface).Border(1, line).Radius(12).Padding(16, 18).Gap(9).MinWidth(0)
}
func title(c *ui.Context, text string) { ui.Text(c, text).FontSize(15).Bold().TextColor(ink) }
func hint(c *ui.Context, text string) {
	ui.Text(c, text).FontSize(12).TextColor(muted).LineHeight(1.4).Selectable()
}
func sectionTitle(c *ui.Context, icon *ui.SVG, text string) {
	ui.Row(c).Gap(8).AlignItems(ui.Center).Children(func() {
		ui.Icon(c, icon).FontSize(17).TextColor(accent)
		title(c, text)
	})
}
func code(c *ui.Context, text string) {
	ui.Text(c, text).Font("Consolas, Microsoft YaHei UI").FontSize(13).TextColor(ink).Selectable()
}
func field(c *ui.Context, label string, value *string, placeholder string) ui.Element {
	ui.Text(c, label).FontSize(13).Bold().TextColor(ink)
	return ui.TextInput(c.Key(label), value).Label(label).Placeholder(placeholder).FillWidth().Background(paper).Height(32)
}
func (a *app) addressField(c *ui.Context, label string, value *string, placeholder string, kind addressType) ui.Element {
	return field(c, label, value, placeholder).OnPaste(func(text string) bool {
		v := strings.TrimSpace(text)
		if !validAddress(v, kind) && (strings.Contains(v, ":") || strings.ContainsAny(v, "@?#")) {
			a.problem = addressHint
			return true
		}
		return false
	})
}
func primary(c *ui.Context, label string, disabled bool, fn func()) {
	ui.PrimaryButton(c, label).Padding(6, 14).MinHeight(30).Cursor(ui.CursorPointer).Disabled(disabled).OnClick(fn)
}
func (a *app) feedback(c *ui.Context) {
	if a.busy != "" {
		ui.Text(c, a.busy+"…").TextColor(accent).Role(ui.RoleStatus)
	} else if a.problem != "" {
		ui.Text(c, safeDiagnostic(a.problem)).Background(ui.Hex("#fff1eb")).TextColor(warn).Padding(10, 14).Radius(7).Role(ui.RoleAlert).Selectable()
		if a.library == "" {
			ui.Row(c).Children(func() { ui.Button(c, "工具诊断").OnClick(a.diagnose) })
		}
	} else if a.msg != "" {
		ui.Text(c, a.msg).TextColor(accent).Role(ui.RoleStatus).Selectable()
	}
}
func (a *app) view(c *ui.Context) {
	c.SetTheme(theme)
	if a.d.Step != 3 {
		a.lastAnnouncedStage = ""
	}
	width, _ := c.Size()
	small := width < 800
	c.SetMenuPresentation(ui.PopoverMenus)
	ui.Column(c).Fill().Background(paper).Font("Segoe UI, Microsoft YaHei UI").TextColor(ink).Children(func() {
		a.workspaceBar(c, small)
		if a.currentWorkspace() != "" {
			a.libraryView(c, small)
		} else {
			a.wizardWorkspace(c, small)
		}
		a.statusBar(c)
	})
	a.diagnosisView(c)
	a.aboutView(c)
	if ui.AlertDialog(c, &a.closePrompt, "迁移仍在运行", "关闭窗口后将失去实时进度。再次打开应用可在任务历史中查看状态并恢复。", "关闭窗口", "继续查看") == 0 {
		a.closing = true
		a.win.Close()
	}
	if ui.AlertDialog(c, &a.newPrompt, "新建迁移？", "当前配置将保存为上次草稿。", "返回", "保存并新建") == 1 {
		if a.persist() {
			a.reset()
		}
	}
}
func (a *app) wizardWorkspace(c *ui.Context, small bool) {
	icons := [...]*ui.SVG{glyphFolder, glyphBranch, glyphList, glyphRun}
	ui.Row(c).FillWidth().Grow(1).MinHeight(0).AlignItems(ui.Stretch).Children(func() {
		if !small && !a.sidebarHidden {
			ui.Column(c).Width(sidebarWidth).Shrink(0).Padding(10, 8).Gap(2).Background(canvas).
				BorderWidth(0, 1, 0, 0).BorderColor(line).Children(func() {
				ui.Row(c).Padding(4, 5, 8, 5).AlignItems(ui.Center).Gap(7).Children(func() {
					ui.Icon(c, glyphFolder).FontSize(15).TextColor(muted)
					ui.Text(c, "仓库迁移").FontSize(13).Bold()
				})
				for idx, name := range steps {
					current := idx == a.d.Step
					enabled := a.d.Step != 3 && idx < a.d.Step && a.busy == ""
					b := ui.ButtonBase(c.Key(idx)).Label(name).Row().Height(31).Padding(0, 16).Gap(8).
						Justify(ui.Start).AlignItems(ui.Center).AccessSelected(current).
						Disabled(!enabled).DimDisabled(false).OnClick(func() { a.navigate(idx) })
					col := muted
					if current {
						b.Background(theme.SurfacePressed)
						col = ink
					} else if enabled {
						col = ink
						b.Cursor(ui.CursorPointer)
						if b.Hovered() {
							b.Background(theme.SurfaceHover)
						}
					}
					b.Children(func() {
						ui.Icon(c, icons[idx]).FontSize(15).TextColor(col)
						ui.Text(c, name).FontSize(13).TextColor(col)
						if idx < a.d.Step && a.d.Step < 3 {
							ui.Spacer(c)
							ui.Icon(c, glyphCheck).FontSize(12).TextColor(muted)
						}
					})
				}
			})
		}
		width, _ := c.Size()
		available := width
		if !small && !a.sidebarHidden {
			available -= sidebarWidth
		}
		padding := float32(24)
		if small {
			padding = 16
		}
		ui.Scroll(c.Key(fmt.Sprintf("step-%d", a.d.Step))).Basis(0).Grow(1).MinWidth(0).MinHeight(0).Children(func() {
			ui.Column(c).FillWidth().MaxWidth(1160).Justify(ui.Start).AlignSelf(ui.Start).Padding(padding).Gap(15).Children(func() {
				ui.Row(c).FillWidth().AlignItems(ui.Center).Children(func() {
					ui.Text(c, titles[a.d.Step]).FontSize(19).Bold()
					if small || a.sidebarHidden {
						ui.Spacer(c)
						ui.Text(c, fmt.Sprintf("%d / %d", a.d.Step+1, len(steps))).FontSize(12).TextColor(muted)
					}
				})
				if a.d.Step != 3 {
					a.feedback(c)
				}
				ui.Column(c.Key(a.d.Step)).FillWidth().Gap(16).Disabled(a.busy != "" && a.d.Step != 3).DimDisabled(false).Children(func() {
					switch a.d.Step {
					case 0:
						a.sourceView(c)
					case 1:
						a.targetView(c)
					case 2:
						a.confirmView(c, available < 740)
					case 3:
						a.resultView(c)
					}
				})
			})
		})
	})
}

func (a *app) sourceView(c *ui.Context) {
	if a.stored != nil && a.d.Address == "" {
		ui.Row(c).Padding(10, 14).Background(tint).Radius(6).Gap(8).Children(func() {
			ui.Text(c, "上次草稿").Bold()
			ui.Text(c, a.stored.Address).Grow(1).TextColor(muted)
			ui.Button(c, "继续草稿").OnClick(func() {
				d, err := readDraft(a.draftPath())
				if err != nil {
					a.problem = err.Error()
					return
				}
				if d != nil {
					a.d = *d
					a.custom = d.Trunk != "trunk" || d.Branches != "branches" || d.Tags != "tags"
					a.msg = "已恢复草稿"
					a.frozen = false
				}
			})
		})
	} else if len(a.history) > 0 && a.d.Address == "" {
		last := a.history[0]
		ui.Row(c).Padding(10, 14).Background(tint).Radius(6).Gap(8).Children(func() {
			ui.Text(c, "最近任务").Bold()
			ui.Text(c, last.Title+"  "+stageTitle(last.Stage)).Grow(1).TextColor(muted)
			ui.Button(c, "查看任务").OnClick(func() { a.loadJob(last.ID) })
		})
	}
	sectionTitle(c, glyphFolder, "SVN 仓库")
	panel(c).Children(func() {
		hint(c, "默认迁移项目的 trunk。也可以选择或粘贴一个分支，或只迁移该分支中的一个子项目。")
		old := a.d.Address
		if a.addressField(c, "仓库或项目地址", &a.d.Address, "https://svn.example.org/repos/team", svnAddress).Changed() {
			if !sameHost(old, a.d.Address) {
				a.d.SVNID = ""
			}
			a.tree = nil
			a.d.Project = ""
			a.d.clearSource()
			a.frozen = false
			a.msg = ""
			a.problem = ""
		}
		ui.Collapsible(c, a.credentialSummary("SVN 凭据", a.d.SVNID), &a.svnAuth.open, func() { a.authView(c, true, &a.svnAuth, &a.d.SVNID, false) })
		ui.Row(c).Children(func() { primary(c, "连接", strings.TrimSpace(a.d.Address) == "", a.connect) })
	})
	if a.tree != nil || a.d.Project != "" {
		sectionTitle(c, glyphList, "项目目录")
		panel(c).Children(func() {
			if a.tree != nil {
				ui.Scroll(c.Key("dirs")).Height(165).Background(paper).Border(1, line).Radius(8).Padding(6).Children(func() {
					treeTheme := *theme
					treeTheme.Accent, treeTheme.InactiveAccent, treeTheme.AccentText = theme.SurfacePressed, theme.SurfacePressed, ink
					ui.Themed(c, &treeTheme).Children(func() { ui.Tree(c, func() { a.treeItem(c, a.tree) }) })
				})
			}
			code(c, a.d.Project)
			ui.Collapsible(c, "自定义目录布局", &a.custom, func() {
				ui.Column(c).Gap(7).Padding(8).Children(func() {
					hint(c, "使用所选项目下的相对路径。主线留空表示当前目录，分支和标签目录可留空。")
					for _, f := range []struct {
						label string
						v     *string
						value string
					}{{"主线路径", &a.d.Trunk, "当前目录"}, {"分支父目录", &a.d.Branches, "可选"}, {"标签父目录", &a.d.Tags, "可选"}} {
						if field(c, f.label, f.v, f.value).Changed() {
							a.d.LayoutEdited = true
							a.d.clearSource()
							a.frozen = false
							a.problem = ""
						}
					}
				})
			})
			label := "迁移主线 · " + displaySourcePath(a.d.Project, a.d.Trunk)
			if a.d.autoBranchSubdir() {
				label = "迁移主线 · 将检查 trunk，若不存在则使用当前目录"
			} else if a.d.Source != nil {
				label += fmt.Sprintf(" · 已读取 r%d", a.d.Source.Revision)
			}
			hint(c, label)
			ui.Row(c).Justify(ui.End).Children(func() {
				primary(c, "下一步 · Git 目标", a.d.Project == "", func() {
					if a.d.Source != nil {
						a.navigate(1)
					} else {
						a.inspect(1)
					}
				})
			})
		})
	}
}
func (a *app) treeItem(c *ui.Context, n *directory) {
	var open *bool
	var children func()
	if !n.Loaded || len(n.Children) > 0 {
		open = &n.Open
		children = func() {
			for _, child := range n.Children {
				a.treeItem(c, child)
			}
		}
	}
	item := ui.TreeItem(c.Key(n.URL), n.Name, open, children).Selected(n.URL == a.d.Project)
	if item.Clicked() && a.busy == "" {
		a.chooseProject(n.URL)
	}
	if n.Open && !n.Loaded && a.busy == "" {
		a.browse(n, "")
	}
}
func displaySourcePath(project, relative string) string {
	if relative != "" {
		return relative
	}
	if dir := mainlineDirectory(project); dir != "" {
		return dir
	}
	if dir := nestedBranchPath(project); dir != "" {
		return dir
	}
	return "当前目录"
}
func (a *app) targetView(c *ui.Context) {
	sectionTitle(c, glyphBranch, "远端 Git 仓库")
	panel(c).Children(func() {
		hint(c, "填写已有 Git 仓库和一个尚未创建的分支名。推送时会创建该分支，并保留已有分支和标签。")
		old := a.d.Remote
		if a.addressField(c, "克隆地址", &a.d.Remote, "https://git.example.org/team/project.git", gitAddress).Changed() {
			a.d.RemoteOK = false
			if !sameHost(old, a.d.Remote) {
				a.d.GitID = ""
			}
			if !a.d.WebCustom {
				a.d.Web = suggestedWeb(a.d.Remote, a.d.Main)
			}
		}
		if field(c, "目标分支", &a.d.Main, "main").Changed() {
			a.d.RemoteOK = false
			a.problem = ""
			if !a.d.WebCustom {
				a.d.Web = suggestedWeb(a.d.Remote, a.d.Main)
			}
		}
		if !validRefName(a.d.Main) {
			ui.Text(c, "请输入有效的 Git 分支名称").TextColor(warn)
		}
		if a.addressField(c, "仓库网页（可选）", &a.d.Web, "https://git.example.org/team/project", pageAddress).Changed() {
			a.d.WebCustom = a.d.Web != ""
		}
		if strings.ContainsAny(a.d.Web, "?#") && webURL(a.d.Web) {
			hint(c, "此网页地址仅用于本次任务，恢复草稿后需重新填写。")
		}
		ui.Collapsible(c, a.credentialSummary("Git 凭据", a.d.GitID), &a.gitAuth.open, func() { a.authView(c, false, &a.gitAuth, &a.d.GitID, false) })
		ui.Row(c).Gap(8).Wrap().Children(func() {
			ui.Button(c, "返回 SVN 项目").OnClick(func() { a.navigate(0) })
			ui.Spacer(c)
			primary(c, "检查目标分支并继续", strings.TrimSpace(a.d.Remote) == "" || !validRefName(a.d.Main), a.checkRemote)
		})
	})
}
