package main

import (
	"fmt"

	"github.com/egoist/mygo/ui"
)

const sidebarWidth float32 = 240

var workspaceTabs = []struct {
	id, name string
	icon     *ui.SVG
}{
	{"", "迁移", glyphBranch}, {"history", "任务历史", glyphHistory},
	{"authors", "作者库", glyphUser}, {"credentials", "凭据库", glyphKey},
}

func (a *app) openLibrary(page string) {
	if a.library == "credentials" && page != a.library {
		a.discardCredentialEditor()
	}
	a.historyOpen = false
	a.library = page
	a.problem, a.msg = "", ""
}

func (a *app) leaveLibrary() { a.openLibrary("") }

func (a *app) currentWorkspace() string {
	if a.historyOpen {
		return "history"
	}
	return a.library
}

func (a *app) switchWorkspace(page string) {
	if a.busy != "" || page == a.currentWorkspace() {
		return
	}
	switch page {
	case "authors":
		a.openAuthors()
	case "credentials":
		a.openCredentials()
	case "history":
		a.leaveLibrary()
		a.historyOpen = true
		a.loadHistory()
	default:
		a.leaveLibrary()
	}
}

func (a *app) newMigration() {
	if a.busy != "" {
		return
	}
	if a.d.Step < 3 && (a.d.Address != "" || a.d.Remote != "") {
		a.newPrompt = true
		return
	}
	a.leaveLibrary()
	a.reset()
}

func (a *app) workspaceMenu(c *ui.Context, small bool) {
	bar := c.TitleBar()
	ui.Row(c).FillWidth().Height(32).Shrink(0).AlignItems(ui.Center).Gap(2).
		Padding(0, 10+bar.Right, 0, 10+bar.Left).Background(theme.Surface).
		BorderWidth(0, 0, 1, 0).BorderColor(line).DragWindow().KeepFocus().Role(ui.RoleMenuBar).Children(func() {
		ui.Image(c, appIconImage).Size(17, 17).Margin(0, 6, 0, 0)
		menu := func(name string, items func(*ui.Menu)) {
			b := ui.ButtonBase(c.Key(name)).Label(name).Height(24).Padding(0, 8).Center().Radius(4).Cursor(ui.CursorPointer)
			if b.Hovered() {
				b.Background(theme.SurfaceHover)
			}
			b.Children(func() { ui.Text(c, name).FontSize(13) })
			b.PopoverMenuButton(items)
		}
		menu("文件", func(m *ui.Menu) {
			if m.Item("新建迁移").Disabled(a.busy != "").Chosen() {
				a.newMigration()
			}
			if m.Item("返回迁移").Disabled(a.currentWorkspace() == "" || a.busy != "").Chosen() {
				a.switchWorkspace("")
			}
		})
		menu("视图", func(m *ui.Menu) {
			for _, item := range workspaceTabs {
				if m.Item(item.name).Checked(item.id == a.currentWorkspace()).Disabled(a.busy != "").Chosen() {
					a.switchWorkspace(item.id)
				}
			}
			m.Separator()
			if m.Item("显示侧栏").Checked(!a.sidebarHidden && !small).Disabled(small).Chosen() {
				a.sidebarHidden = !a.sidebarHidden
			}
		})
		menu("工具", func(m *ui.Menu) {
			if m.Item("工具诊断").Disabled(a.busy != "").Chosen() {
				a.diagnose()
			}
		})
		menu("帮助", func(m *ui.Menu) {
			if m.Item("关于 " + product.Name).Chosen() {
				a.about.open = true
			}
			if m.Item("检查更新").Disabled(a.about.checking).Chosen() {
				a.about.open = true
				a.checkForUpdates()
			}
		})
		ui.Spacer(c)
		ui.Text(c, product.Name+" v"+product.Version).FontSize(12).TextColor(muted).SingleLine()
	})
}

func toolbarAction(c *ui.Context, icon *ui.SVG, label string, selected, disabled bool, action func()) {
	b := ui.ButtonBase(c.Key(label)).Size(28, 28).Center().Radius(6).
		Label(label).Tooltip(label).Cursor(ui.CursorPointer).Disabled(disabled).OnClick(action)
	if selected || b.Hovered() {
		b.Background(theme.SurfacePressed)
	}
	b.Children(func() { ui.Icon(c, icon).FontSize(16).TextColor(ink) })
}

func (a *app) workspaceBar(c *ui.Context, small bool) {
	a.workspaceMenu(c, small)
	page := a.currentWorkspace()
	ui.Toolbar(c, func() {
		if !small {
			toolbarAction(c, glyphSidebar, "切换侧栏", !a.sidebarHidden, false, func() { a.sidebarHidden = !a.sidebarHidden })
		}
		toolbarAction(c, glyphPlus, "新建迁移", false, a.busy != "", a.newMigration)
		toolbarAction(c, glyphFolder, "返回迁移", false, a.busy != "" || page == "", func() { a.switchWorkspace("") })
		ui.Box(c).Width(1).Height(18).Background(line).Margin(0, 5)
		toolbarAction(c, glyphHistory, "查看任务历史", page == "history", a.busy != "", func() { a.switchWorkspace("history") })
		toolbarAction(c, glyphUser, "管理作者", page == "authors", a.busy != "", func() { a.switchWorkspace("authors") })
		toolbarAction(c, glyphKey, "管理凭据", page == "credentials", a.busy != "", func() { a.switchWorkspace("credentials") })
		ui.Spacer(c)
		toolbarAction(c, glyphTools, "工具诊断", false, a.busy != "", a.diagnose)
	}).Height(45).Shrink(0).FillWidth().Padding(8, 10).Gap(4).Background(theme.Surface).BorderWidth(0, 0, 1, 0).BorderColor(line)
	ui.Row(c).FillWidth().Height(35).Shrink(0).AlignItems(ui.Stretch).Background(theme.Surface).
		BorderWidth(0, 0, 1, 0).BorderColor(line).Children(func() {
		if !small && !a.sidebarHidden {
			section, count := "迁移流程", ""
			switch page {
			case "authors":
				section, count = "作者列表", fmt.Sprintf("%d 位", len(a.authorBook))
			case "credentials":
				section, count = "凭据列表", fmt.Sprintf("%d 条", len(a.credentialPage.profiles))
			case "history":
				section, count = "历史记录", fmt.Sprintf("%d 项", len(a.history))
			}
			ui.Row(c).Width(sidebarWidth).Shrink(0).AlignItems(ui.Center).Padding(0, 10).Background(canvas).BorderWidth(0, 1, 0, 0).BorderColor(line).Children(func() {
				ui.Box(c).Height(30).Center().Padding(0, 6).BorderWidth(0, 0, 2, 0).BorderColor(accent).Children(func() { ui.Text(c, section).FontSize(12) })
				ui.Spacer(c)
				if count != "" {
					ui.Text(c, count).FontSize(11).TextColor(muted)
				}
			})
		}
		for _, item := range workspaceTabs {
			active := item.id == page
			b := ui.ButtonBase(c.Key(item.id)).Label(item.name).Tooltip(item.name).AccessSelected(active).Cursor(ui.CursorPointer).
				Disabled(a.busy != "").OnClick(func() { a.switchWorkspace(item.id) }).
				Row().AlignItems(ui.Center).Justify(ui.Center).Gap(6).Padding(0, 12).BorderWidth(0, 1, 0, 0).BorderColor(line)
			if active {
				b.Background(paper)
			} else if b.Hovered() {
				b.Background(theme.SurfaceHover)
			}
			b.Children(func() {
				col := muted
				if active {
					col = ink
				}
				ui.Icon(c, item.icon).FontSize(14).TextColor(col)
				ui.Text(c, item.name).SingleLine().FontSize(13).TextColor(col)
			})
			if active {
				b.DrawOver(func(p *ui.Painter, r ui.Rect) { p.Fill(ui.Rect{X: r.X, Y: r.Y + r.H - 2, W: r.W, H: 2}, accent, 0) })
			}
		}
	})
}

func (a *app) libraryView(c *ui.Context, small bool) {
	ui.Column(c.Key(a.currentWorkspace())).FillWidth().Grow(1).MinHeight(0).Children(func() {
		if a.busy != "" || a.problem != "" || a.msg != "" {
			ui.Column(c).FillWidth().Padding(5, 14).Gap(3).Children(func() { a.feedback(c) })
		}
		ui.Column(c).FillWidth().Grow(1).MinHeight(0).Disabled(a.busy != "").DimDisabled(false).Children(func() {
			switch a.currentWorkspace() {
			case "authors":
				a.authorsLibraryView(c, small)
			case "credentials":
				a.credentialsLibraryView(c, small)
			case "history":
				a.historyView(c, small)
			}
		})
	})
}

func (a *app) libraryPanes(c *ui.Context, small, editing bool, list, detail func()) {
	ui.Row(c).FillWidth().Grow(1).MinHeight(0).AlignItems(ui.Stretch).Background(paper).Children(func() {
		if small && !editing || !small && !a.sidebarHidden {
			side := ui.Scroll(c.Key("list")).MinHeight(0).MinWidth(0).Background(canvas)
			if small {
				side.Grow(1)
			} else {
				side.Width(sidebarWidth).Shrink(0).BorderWidth(0, 1, 0, 0).BorderColor(line)
			}
			side.Children(func() { ui.Column(c).FillWidth().Justify(ui.Start).Padding(10, 8).Gap(7).Children(list) })
		}
		if !small || editing {
			inset := float32(24)
			if small {
				inset = 16
			}
			ui.Scroll(c.Key("detail")).Basis(0).Grow(1).MinWidth(0).MinHeight(0).Children(func() {
				ui.Column(c).FillWidth().MaxWidth(920).AlignSelf(ui.Start).Justify(ui.Start).Padding(inset).Gap(12).Children(detail)
			})
		}
	})
}

func (a *app) statusBar(c *ui.Context) {
	var status, detail string
	switch a.currentWorkspace() {
	case "authors":
		status, detail = "作者库", "共享作者映射"
	case "credentials":
		status, detail = "凭据库", "本机凭据"
	case "history":
		status = "任务历史"
	default:
		status = steps[a.d.Step]
		if a.d.Step == 3 {
			if m := a.job.Manifest; m.Revision > 0 {
				detail = fmt.Sprintf("r%d", m.Revision)
				for _, ref := range m.Refs {
					if ref.Kind == "trunk" {
						detail += "   " + ref.Name
						break
					}
				}
			}
		} else {
			if a.d.Source != nil {
				detail = fmt.Sprintf("r%d", a.d.Source.Revision)
			}
			if a.d.Step >= 1 && a.d.Main != "" {
				detail += "   " + a.d.Main
			}
		}
	}
	if a.busy != "" {
		status = a.busy + "…"
	}
	ui.Row(c).FillWidth().Height(26).Shrink(0).AlignItems(ui.Center).Gap(10).Padding(0, 10).
		Background(theme.Surface).BorderWidth(1, 0, 0, 0).BorderColor(line).Children(func() {
		ui.Text(c, status).FontSize(11).TextColor(muted).SingleLine()
		ui.Spacer(c)
		if detail != "" {
			ui.Text(c, detail).FontSize(11).TextColor(muted).SingleLine()
		}
	})
}
