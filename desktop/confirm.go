package main

import (
	"fmt"
	"slices"
	"strings"

	"github.com/egoist/mygo/ui"
)

func refKind(k string) string {
	if k == "tag" {
		return "标签"
	}
	if k == "trunk" {
		return "主线"
	}
	return "分支"
}
func refCount(refs []scopeRef) string {
	b, t := 0, 0
	for _, r := range refs {
		if r.Kind == "tag" {
			t++
		} else {
			b++
		}
	}
	return fmt.Sprintf("%d 个目标分支 · %d 个标签", b, t)
}
func (a *app) confirmView(c *ui.Context, small bool) {
	if a.d.Source == nil {
		ui.Button(c, "返回选择 SVN 项目").OnClick(func() { a.navigate(0) })
		return
	}
	i := a.d.Source
	var route ui.Element
	if small {
		route = ui.Column(c)
	} else {
		route = ui.Row(c)
	}
	route.Gap(15).Padding(12, 16).Radius(10).Background(theme.Surface).Border(1, line).Children(func() {
		ui.Column(c).Grow(1).MinWidth(0).Gap(4).Children(func() {
			hint(c, fmt.Sprintf("来源 · SVN 项目  /  r%d", i.Revision))
			code(c, i.URL)
		})
		if !small {
			ui.Text(c, "→").TextColor(accent).FontSize(20)
		}
		ui.Column(c).Grow(1).MinWidth(0).Gap(4).Children(func() {
			hint(c, "目标 · 新建 Git 分支  /  "+a.d.Main)
			code(c, a.d.Remote)
		})
	})
	var layout ui.Element
	if small {
		layout = ui.Column(c)
	} else {
		layout = ui.Row(c).AlignItems(ui.Start)
	}
	card := func() ui.Element {
		p := panel(c)
		if !small {
			p = p.Grow(1).Basis(0)
		}
		return p
	}
	layout.Gap(15).Children(func() {
		card().Children(func() {
			sectionTitle(c, glyphFolder, "选择迁移范围")
			ui.Row(c).Padding(9, 12).Background(tint).Radius(5).Gap(8).Children(func() {
				ui.Text(c, "✓").TextColor(accent).Bold()
				ui.Text(c, displaySourcePath(i.URL, i.Trunk)).Font("Consolas").Grow(1)
				hint(c, "主线及历史 · 必选")
			})
			a.optionsView(c, "branch", &a.branchesOpen, &a.branchFilter)
			a.optionsView(c, "tag", &a.tagsOpen, &a.tagFilter)
			ui.Row(c).Wrap().Gap(8).Children(func() {
				ui.Button(c, "修改 Git 目标").OnClick(func() { a.navigate(1) })
				ui.Button(c, "刷新 SVN 修订").OnClick(func() { a.inspect(2) })
			})
		})
		card().Children(func() {
			m := a.d.manifest()
			sectionTitle(c, glyphList, "本次发布清单")
			ui.Text(c, refCount(m.Refs)).TextColor(accent).Bold()
			problems := refErrors(m.Refs, a.d.remoteRefs)
			for j, r := range m.Refs {
				ui.Column(c.Key(r.Kind + "\n" + r.Path)).Gap(3).Children(func() {
					code(c, fmt.Sprintf("%s  %s", refKind(r.Kind), displaySourcePath(i.URL, r.Path)))
					code(c, "→  "+r.fullRef())
					if p := problems[j]; p != "" {
						ui.Text(c, p).FontSize(13).TextColor(warn)
					}
				})
			}
			ui.Divider(c)
			ui.Collapsible(c, "历史与文件规则", &a.rulesOpen, func() {
				hint(c, fmt.Sprintf("还原所选路径至 r%d 的修订，追溯复制和改名前的祖先。Git 不记录空目录及一般 SVN 属性。", i.Revision))
				ui.Checkbox(c, &a.d.ExcludeEnabled, "排除匹配的文件")
				if a.d.ExcludeEnabled {
					hint(c, "相对每个选定来源的路径，每行一个 glob 表达式。")
					ui.TextArea(c, &a.d.Exclusions).Label("排除规则").Placeholder("*.tmp").Height(78)
				}
				ui.Checkbox(c, &a.d.LFSEnabled, "使用 Git LFS")
				if a.d.LFSEnabled {
					field(c, "大文件阈值（MiB）", &a.d.LFSSize, "50")
				}
			})
			globs, threshold, ruleErr := a.d.fileRules()
			if len(globs) > 0 {
				hint(c, "排除路径 · "+strings.Join(globs, "，"))
			}
			if threshold != nil {
				hint(c, fmt.Sprintf("Git LFS · 阈值 %d MiB", *threshold))
			}
			if !a.d.ExcludeEnabled && !a.d.LFSEnabled {
				hint(c, "保留全部版本化文件")
			}
			if ruleErr != nil {
				ui.Text(c, ruleErr.Error()).TextColor(warn)
			}
			if i.ExternalWarning != "" {
				ui.Text(c, i.ExternalWarning).TextColor(warn).Selectable()
			}
			if len(i.Externals) > 0 {
				ui.Text(c, "SVN 外部引用").TextColor(warn).Bold()
				hint(c, "外部仓库的文件需单独迁移。")
				for _, e := range i.Externals {
					if !strings.Contains(e, "://") {
						code(c, e)
					}
				}
			}
			a.authorsView(c)
			ui.Collapsible(c, "工作目录", &a.advanced, func() {
				field(c, "任务工作目录（可选）", &a.d.Workdir, "默认由应用为每个任务创建")
			})
			if err := authorProblem(a.d.Authors); err != "" {
				ui.Text(c, err).TextColor(warn)
			}
			hint(c, "推送前会再次检查清单中的名称，只创建新的分支和标签。")
			primary(c, "开始迁移", len(problems) > 0 || authorProblem(a.d.Authors) != "" || ruleErr != nil || !a.d.RemoteOK, func() { a.execute(false) })
		})
	})
}
func (a *app) optionsView(c *ui.Context, kind string, open *bool, filter *string) {
	total, selected := 0, 0
	for _, o := range a.d.Options {
		if o.Kind == kind {
			total++
			if o.Selected {
				selected++
			}
		}
	}
	name := refKind(kind)
	caption := fmt.Sprintf("%s    已选 %d / %d", name, selected, total)
	ui.Collapsible(c.Key(kind), caption, open, func() {
		ui.Column(c).Gap(6).Padding(4).Children(func() {
			if total == 0 {
				hint(c, "此目录下没有可选"+name)
				return
			}
			if total > 5 {
				ui.TextInput(c.Key("filter"), filter).Placeholder("查找" + name).Label("查找" + name)
			}
			for idx := range a.d.Options {
				o := &a.d.Options[idx]
				if o.Kind != kind || !strings.Contains(strings.ToLower(o.Path), strings.ToLower(*filter)) {
					continue
				}
				ui.Column(c.Key(o.Path)).Gap(4).Children(func() {
					ui.Checkbox(c, &o.Selected, o.Path)
					if o.Selected {
						field(c, "目标"+name+"名", &o.Name, "Git 引用名")
					}
				})
			}
		})
	})
}
func (a *app) scanAuthors(scope *authorScope) {
	if scope == nil {
		return
	}
	req := authRequest("scan_authors", "url", scope.url, "svn_credential_id", scope.credential)
	req["revision"], req["paths"] = scope.revision, scope.paths
	var rows []scannedAuthor
	a.start("扫描所选 SVN 路径的作者", func() error {
		return a.engine.call(req, &rows, nil)
	}, func(err error) {
		if err != nil || !scope.matches(a.d.authorScope()) {
			return
		}
		p := &a.authorsPage
		p.scanScope, p.scanned, p.editingOverride = scope, rows, ""
		a.authorsOpen = true
	})
}

func (a *app) authorsView(c *ui.Context) {
	scope, p := a.d.authorScope(), &a.authorsPage
	if !p.scanScope.matches(scope) {
		p.scanScope, p.scanned, p.editingOverride = nil, nil, ""
		ui.Button(c, "查看本次作者").OnClick(func() { a.scanAuthors(scope) })
		a.manualOverridesView(c, nil)
		return
	}
	overrides := make(map[string]int, len(a.d.Authors))
	for i, r := range a.d.Authors {
		overrides[r.SVNUser] = i
	}
	scanned := make(map[int]bool)
	for _, row := range p.scanned {
		if idx, ok := overrides[row.SVNUser]; ok {
			scanned[idx] = true
		}
	}
	ui.Collapsible(c, fmt.Sprintf("本次作者 · %d 位", len(p.scanned)), &a.authorsOpen, func() {
		ui.Column(c).Gap(8).Children(func() {
			ui.Button(c, "重新扫描").OnClick(func() { a.scanAuthors(scope) })
			for _, row := range p.scanned {
				ui.Column(c.Key(row.SVNUser)).Gap(4).Children(func() {
					idx, overridden := overrides[row.SVNUser]
					a.confirmAuthorRow(c, row, idx, overridden)
				})
			}
		})
	})
	a.manualOverridesView(c, scanned)
}

func (a *app) confirmAuthorRow(c *ui.Context, row scannedAuthor, idx int, overridden bool) {
	p := &a.authorsPage
	identity, source := row.author, "默认身份"
	if row.InLibrary {
		source = "来自作者库"
	}
	if overridden {
		identity, source = a.d.Authors[idx], "仅本次"
	}
	ui.Row(c).Wrap().Gap(8).Children(func() {
		ui.Text(c, row.SVNUser).Bold()
		ui.Text(c, source).FontSize(13).TextColor(accent)
	})
	editing := overridden && p.editingOverride == row.SVNUser
	if editing {
		field(c, "Git 姓名", &a.d.Authors[idx].Name, "姓名")
		field(c, "Git 邮箱", &a.d.Authors[idx].Email, "name@example.org")
	} else {
		hint(c, identity.Name+" <"+identity.Email+">")
	}
	ui.Row(c).Wrap().Gap(6).Children(func() {
		if editing {
			ui.Button(c, "完成编辑").Disabled(identity.problem() != "").OnClick(func() { p.editingOverride = "" })
		} else {
			ui.Button(c, "仅修改本次").OnClick(func() {
				if !overridden {
					a.d.Authors = append(a.d.Authors, row.author)
				}
				p.editingOverride = row.SVNUser
			})
		}
		if overridden {
			ui.Button(c, "撤销覆盖").OnClick(func() {
				a.d.Authors = slices.Delete(a.d.Authors, idx, idx+1)
				if p.editingOverride == row.SVNUser {
					p.editingOverride = ""
				}
			})
		}
	})
}

func (a *app) manualOverridesView(c *ui.Context, scanned map[int]bool) {
	p := &a.authorsPage
	open := func(index int, entry author) {
		p.manual = manualOverrideEditor{open: true, index: index, entry: entry}
	}
	for i, r := range a.d.Authors {
		if scanned[i] {
			continue
		}
		user := r.SVNUser
		if user == "" {
			user = "未填写 SVN 用户名"
		}
		ui.Column(c.Key(fmt.Sprintf("manual-%d", i))).Gap(4).Children(func() {
			ui.Row(c).Wrap().Gap(8).Children(func() {
				ui.Text(c, user).Bold()
				ui.Text(c, "仅本次").FontSize(13).TextColor(accent)
			})
			hint(c, r.Name+" <"+r.Email+">")
			ui.Row(c).Wrap().Gap(6).Children(func() {
				ui.Button(c, "编辑映射").Label("编辑 " + user).OnClick(func() { open(i, r) })
				ui.Button(c, "撤销覆盖").Label("移除 " + user).OnClick(func() {
					a.d.Authors = slices.Delete(a.d.Authors, i, i+1)
				})
			})
		})
	}
	ui.Button(c, "手动添加映射").OnClick(func() { open(-1, author{}) })
	m := &p.manual
	ui.Modal(c, &m.open, func() {
		title(c, "本次作者映射")
		field(c, "SVN 用户名", &m.entry.SVNUser, "提交时使用的用户名")
		field(c, "Git 姓名", &m.entry.Name, "姓名")
		field(c, "Git 邮箱", &m.entry.Email, "name@example.org")
		if m.problem != "" {
			ui.Text(c, m.problem).TextColor(warn)
		}
		ui.Row(c).Wrap().Gap(6).Children(func() {
			primary(c, "应用到本次", false, func() {
				r := m.entry.trimmed()
				m.problem = r.problem()
				if m.problem != "" {
					return
				}
				for i, existing := range a.d.Authors {
					if i != m.index && existing.SVNUser == r.SVNUser {
						m.problem = "该 SVN 用户名已有本次覆盖"
						return
					}
				}
				if m.index >= 0 {
					a.d.Authors[m.index] = r
				} else {
					a.d.Authors = append(a.d.Authors, r)
				}
				m.open = false
			})
			ui.Button(c, "取消").OnClick(func() { m.open = false })
		})
	})
}
