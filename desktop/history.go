package main

import (
	"fmt"
	"net/url"
	"path"
	"strings"
	"time"

	"github.com/egoist/mygo/ui"
)

// historyName keeps a long repository URL in the details and the project name in the sidebar.
func historyName(j job) string {
	if validAddress(j.Title, svnAddress) {
		u, _ := url.Parse(j.Title)
		if name := path.Base(u.Path); name != "/" && name != "." {
			return name
		}
		return u.Host
	}
	if j.Title != "" {
		return safeDiagnostic(j.Title)
	}
	return j.ID
}

var legacyStatusNames = map[string]string{
	"draft": "草稿", "prechecking": "正在预检", "precheck_failed": "预检失败",
	"author_mapping": "正在映射作者", "ready": "就绪", "running": "正在运行",
	"paused": "已暂停", "verifying": "正在校验", "completed": "迁移完成",
	"syncing_delta": "正在同步增量", "failed": "失败", "cancelled": "已取消",
}

func historyStage(j job) string {
	if !strings.HasPrefix(j.ID, "legacy:") {
		return stageTitle(j.Stage)
	}
	status := legacyStatusNames[j.Stage]
	if status == "" {
		status = j.Stage
	}
	return "旧版任务 · " + status
}

func (a *app) historyView(c *ui.Context, small bool) {
	var selected *job
	for i := range a.history {
		if a.history[i].ID == a.historySelection {
			selected = &a.history[i]
			break
		}
	}
	if selected == nil && !small && len(a.history) > 0 {
		selected = &a.history[0]
	}
	a.libraryPanes(c, small, selected != nil, func() {
		if small {
			a.historyFeedback(c)
		}
		for _, item := range a.history {
			name := historyName(item)
			subtitle := historyStage(item)
			if updated, err := time.Parse(time.RFC3339Nano, item.UpdatedAt); err == nil {
				subtitle += " · " + updated.Local().Format("01-02 15:04")
			}
			recordRow(c, item.ID, "查看 "+name, name, subtitle, glyphHistory, selected != nil && selected.ID == item.ID, func() {
				a.historySelection = item.ID
			})
		}
	}, func() {
		if small {
			ui.Row(c).Children(func() {
				ui.Button(c, "返回记录列表").OnClick(func() { a.historySelection = "" })
			})
		}
		a.historyFeedback(c)
		if selected == nil {
			return
		}
		a.historyRecord(c, *selected)
	})
}

func (a *app) historyFeedback(c *ui.Context) {
	switch {
	case a.historyLoading:
		ui.Text(c, "正在读取任务记录…").FontSize(13).TextColor(muted).Role(ui.RoleStatus)
	case a.historyErr != "":
		ui.Text(c, safeDiagnostic(a.historyErr)).TextColor(warn).Role(ui.RoleAlert).Selectable()
		ui.Row(c).Children(func() { ui.Button(c, "重新读取").OnClick(a.loadHistory) })
	case len(a.history) == 0:
		libraryEmpty(c, glyphHistory, "暂无迁移记录", "开始迁移后，可以在这里查看进度和结果。")
	}
}

func (a *app) historyRecord(c *ui.Context, j job) {
	caption := historyStage(j)
	if updated, err := time.Parse(time.RFC3339Nano, j.UpdatedAt); err == nil {
		caption += " · 最后更新 " + updated.Local().Format("2006-01-02 15:04")
	}
	ui.Row(c).FillWidth().Gap(12).AlignItems(ui.Center).Children(func() {
		ui.Column(c).MinWidth(0).Grow(1).Children(func() { libraryDetailHeader(c, historyName(j), caption) })
		primary(c, "打开任务", a.busy != "", func() { a.loadJob(j.ID) })
	})
	svn := j.Manifest.URL
	if svn == "" {
		svn = j.Title
	}
	if validAddress(svn, svnAddress) || validAddress(j.Manifest.Remote, gitAddress) {
		libraryGroup(c, "迁移范围", glyphBranch, func() {
			type entry struct{ label, value string }
			var rows []entry
			if validAddress(svn, svnAddress) {
				rows = append(rows, entry{"SVN 仓库", svn})
			}
			if j.Manifest.Revision > 0 {
				rows = append(rows, entry{"锁定修订", fmt.Sprintf("r%d", j.Manifest.Revision)})
			}
			if validAddress(j.Manifest.Remote, gitAddress) {
				rows = append(rows, entry{"Git 仓库", j.Manifest.Remote})
			}
			for _, ref := range j.Manifest.Refs {
				if ref.Kind == "trunk" {
					rows = append(rows, entry{"目标分支", ref.Name})
					break
				}
			}
			for i, row := range rows {
				librarySetting(c, row.label, row.value, i == len(rows)-1, nil)
			}
		})
	}
	libraryGroup(c, "执行记录", glyphList, func() {
		if j.Message != "" {
			librarySetting(c, "最近消息", safeDiagnostic(j.Message), false, nil)
		}
		if j.complete() && !strings.HasPrefix(j.ID, "legacy:") {
			librarySetting(c, "已迁移的提交", fmt.Sprint(j.CommitCount), false, nil)
		}
		librarySetting(c, "任务编号", j.ID, true, nil)
	})
}
