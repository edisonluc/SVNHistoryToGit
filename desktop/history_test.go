package main

import (
	"strings"
	"testing"

	"github.com/egoist/mygo/ui"
)

func TestHistoryPreviewPreservesDraftAndAdaptsToNarrowWindow(t *testing.T) {
	a := testApp(t)
	a.d.Step, a.d.Main = 1, "release"
	a.historyOpen = true
	a.history = []job{
		{ID: "first-job", Title: "https://svn.example.org/team/平台", Stage: "completed", Manifest: manifest{Remote: "https://git.example.org/team/platform", Revision: 17, Refs: []scopeRef{{Kind: "trunk", Name: "release"}}}},
		{ID: "legacy:7", Title: "归档迁移", Stage: "failed", Message: "历史校验停止"},
	}
	tt := ui.NewTester(a.view, 1080, 740)
	if !tt.HasText("打开任务") || !tt.HasText("r17") || !tt.HasText("https://git.example.org/team/platform") {
		t.Fatalf("wide view should show the latest task: %v", tt.Texts())
	}
	if err := tt.Click("查看 归档迁移"); err != nil {
		t.Fatal(err)
	}
	if !tt.HasText("历史校验停止") || a.d.Step != 1 || a.d.Main != "release" || a.job.ID != "" {
		t.Fatal("previewing a historical record modified the working migration")
	}
	tt.SetSize(480, 600)
	if !tt.HasText("返回记录列表") {
		t.Fatal(tt.Texts())
	}
	if err := tt.Click("返回记录列表"); err != nil {
		t.Fatal(err)
	}
	if tt.HasText("打开任务") || !tt.HasText("平台") || !tt.HasText("归档迁移") {
		t.Fatal(tt.Texts())
	}
}

func TestEmptyHistoryAndLoadingAreDistinct(t *testing.T) {
	a := testApp(t)
	a.historyOpen = true
	tt := ui.NewTester(a.view, 1024, 700)
	if !tt.HasText("暂无迁移记录") {
		t.Fatal(tt.Texts())
	}
	a.historyLoading = true
	tt.Frame()
	if tt.HasText("暂无迁移记录") || !tt.HasText("正在读取任务记录…") {
		t.Fatal(tt.Texts())
	}
	a.historyLoading = false
	a.historyErr = "无法打开本地数据库"
	tt.Frame()
	if tt.HasText("暂无迁移记录") || !tt.HasText("无法打开本地数据库") || !tt.HasText("重新读取") {
		t.Fatal(tt.Texts())
	}
}

func TestHistoryOnlyShowsSafeRepositoryMetadata(t *testing.T) {
	a := testApp(t)
	a.historyOpen = true
	a.history = []job{{ID: "old-record", Title: "https://person:private@svn.example.org/a",
		Message:  "连接 https://person:private@git.example.org/b 失败",
		Manifest: manifest{Remote: "https://git.example.org/repo?access_token=private"}}}
	tt := ui.NewTester(a.view, 980, 660)
	for _, value := range tt.Texts() {
		if strings.Contains(value, "private") {
			t.Fatal("stored credentials appeared in the history preview")
		}
	}
	if !tt.HasText("打开任务") || !tt.HasText("记录中的地址已隐藏") {
		t.Fatal(tt.Texts())
	}
}
