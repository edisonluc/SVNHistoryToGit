package main

import (
	"slices"
	"testing"

	"github.com/egoist/mygo/ui"
)

func TestReviseKeepsOnlyExplicitAuthorOverrides(t *testing.T) {
	a := testApp(t)
	sampleSource(a)
	global := author{"zhangsan", "张三", "zhangsan@example.com"}
	override := author{"lisi", "李四", "lisi+archive@example.com"}
	m := a.d.manifest()
	m.Authors = []author{global, override}
	a.job = job{ID: "failed", Manifest: m, Stage: "failed", AuthorOverrides: []author{override}}
	a.revise()
	if !slices.Equal(a.d.Authors, []author{override}) {
		t.Fatalf("new draft incorrectly marked global identities as overrides: %v", a.d.Authors)
	}
}

func TestLibraryNavigationKeepsCurrentMigration(t *testing.T) {
	a := testApp(t)
	sampleSource(a)
	a.d.Main = "V0.X"
	a.d.Options[0].Selected = true
	a.d.Step = 1
	tt := ui.NewTester(a.view, 1100, 810)
	for _, name := range []string{"作者库", "凭据库", "任务历史"} {
		if !tt.HasText(name) {
			t.Fatalf("missing workspace tab %s", name)
		}
	}
	a.openLibrary("authors")
	tt.Frame()
	if tt.HasText("检查目标分支并继续") {
		t.Fatal("target form still rendered in author library")
	}
	if err := tt.Click("迁移"); err != nil {
		t.Fatal(err)
	}
	if a.library != "" || !tt.HasText("检查目标分支并继续") || a.d.Main != "V0.X" || !a.d.Options[0].Selected || a.d.Step != 1 {
		t.Fatal("returning from a library must resume the same migration")
	}
	tt.SetSize(480, 720)
	for _, name := range []string{"迁移", "作者库", "凭据库", "新建迁移"} {
		r, ok := tt.Find(name)
		if !ok || r.X < 0 || r.X+r.W > 480 {
			t.Fatalf("toolbar action %s outside narrow window: %v", name, r)
		}
	}
}

func TestLibraryLayoutOnNarrowWindow(t *testing.T) {
	a := testApp(t)
	editing := false
	tt := ui.NewTester(func(c *ui.Context) {
		w, _ := c.Size()
		a.libraryPanes(c, w < 800, editing, func() { ui.Text(c, "记录列表") }, func() { ui.Text(c, "记录详情") })
	}, 480, 680)
	if !tt.HasText("记录列表") || tt.HasText("记录详情") {
		t.Fatal(tt.Texts())
	}
	editing = true
	tt.Frame()
	if tt.HasText("记录列表") || !tt.HasText("记录详情") {
		t.Fatal(tt.Texts())
	}
}

func TestLibraryPanesAlignShortListsAndDetailsAtTop(t *testing.T) {
	a := testApp(t)
	tt := ui.NewTester(func(c *ui.Context) {
		ui.Column(c).Fill().Children(func() {
			a.libraryPanes(c, false, true, func() { ui.Text(c, "第一行") }, func() { ui.Text(c, "编辑项") })
		})
	}, 1000, 650)
	for _, name := range []string{"第一行", "编辑项"} {
		r, found := tt.Find(name)
		if !found || r.Y > 28 {
			t.Fatalf("%s should start below the top inset: %v", name, r)
		}
	}
}

func TestWorkspaceSidebarAndPinnedStatus(t *testing.T) {
	a := testApp(t)
	sampleSource(a)
	tt := ui.NewTester(a.view, 1080, 550)
	if !tt.HasText("迁移流程") {
		t.Fatal("wizard sidebar is missing")
	}
	if err := tt.Click("切换侧栏"); err != nil {
		t.Fatal(err)
	}
	if tt.HasText("迁移流程") || !tt.HasText("本次发布清单") {
		t.Fatal("collapsing the sidebar hid the confirmation")
	}
	if err := tt.Click("切换侧栏"); err != nil {
		t.Fatal(err)
	}
	if !tt.HasText("迁移流程") {
		t.Fatal("sidebar did not reopen")
	}
	status, found := tt.Find("r8421   main")
	if !found || status.Y < 520 || status.Y+status.H > 550 {
		t.Fatalf("status bar not pinned at bottom: %v", status)
	}
	a.d.Step = 3
	a.job = job{ID: "historical", Stage: "remote_verify", Manifest: manifest{Revision: 25, Refs: []scopeRef{{Kind: "trunk", Name: "archived"}}}}
	tt.Frame()
	if !tt.HasText("r25   archived") || tt.HasText("r8421   main") {
		t.Fatal("history result shows status of a different draft")
	}
}
