package main

import (
	"strings"
	"testing"
	"time"

	"github.com/egoist/mygo/ui"
)

func TestDoctorOnlyRequiresToolsForCurrentMigration(t *testing.T) {
	a := testApp(t)
	a.doctorOpen = true
	a.tools = []toolStatus{{Name: "svn", Available: true, Version: "1.14"}, {Name: "git", Available: true, Version: "2.54"}, {Name: "git-lfs"}, {Name: "svnrdump"}, {Name: "git-svn"}, {Name: "svnadmin"}}
	tt := ui.NewTester(a.view, 960, 800)
	if !tt.HasText("本次迁移工具已就绪") || tt.HasText("关闭本次 Git LFS") {
		t.Fatal(tt.Texts())
	}
	if err := tt.Click("可选工具 · 4 项"); err != nil {
		t.Fatal(err)
	}
	tt.Advance(350 * time.Millisecond)
	if err := tt.Click("复制 git-lfs 检测命令"); err != nil || tt.Clipboard() != "git lfs version" {
		t.Fatal(err, tt.Clipboard())
	}
	if err := tt.Click("可选工具 · 4 项"); err != nil {
		t.Fatal(err)
	}
	a.d.Step, a.d.LFSEnabled = 2, true
	tt.Frame()
	if !tt.HasText("本次迁移有 1 项工具需要处理") {
		t.Fatal(tt.Texts())
	}
	if err := tt.Click("关闭本次 Git LFS"); err != nil || a.d.LFSEnabled {
		t.Fatal(err)
	}
	if !tt.HasText("本次迁移工具已就绪") {
		t.Fatal(tt.Texts())
	}
	a.d.Step = 3
	size := uint64(32)
	a.job.Manifest.LFSThreshold = &size
	tt.Frame()
	if _, canChange := tt.Find("关闭本次 Git LFS"); !tt.HasText("本次迁移有 1 项工具需要处理") || canChange {
		t.Fatal(tt.Texts())
	}
}

func TestDoctorMissingToolOffersInstallationAndProbe(t *testing.T) {
	a := testApp(t)
	a.doctorOpen = true
	a.tools = []toolStatus{{Name: "svn", Hint: "安装命令行客户端"}, {Name: "git", Available: true}}
	tt := ui.NewTester(a.view, 480, 440)
	if !tt.HasText("安装命令行客户端") {
		t.Fatal(tt.Texts())
	}
	if err := tt.Click("svn 安装说明"); err != nil || len(tt.OpenedURLs()) != 1 || !strings.HasPrefix(tt.OpenedURLs()[0], "https://") {
		t.Fatal(err, tt.OpenedURLs())
	}
	if err := tt.Click("复制 svn 检测命令"); err != nil || tt.Clipboard() != "svn --version --quiet" {
		t.Fatal(err, tt.Clipboard())
	}
}
