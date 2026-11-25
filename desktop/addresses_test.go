package main

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/egoist/mygo/ui"
)

func TestURLInputAndDraftBoundaries(t *testing.T) {
	for _, s := range []string{"https://alice:sensitive@host/repo", "https://sensitive@host/repo", "https://host/repo?access%5ftoken=sensitive", "https://host/repo#auth=sensitive", "https://al%XX:sensitive@host/repo", "https:/alice:sensitive@host/repo"} {
		for _, kind := range []addressType{svnAddress, gitAddress, pageAddress} {
			if validAddress(s, kind) {
				t.Errorf("accepted unsafe URL, kind %d", kind)
			}
		}
	}
	for _, s := range []string{"https://host/repo?view=tree", "https://host/repo#readme"} {
		if validAddress(s, svnAddress) || validAddress(s, gitAddress) || !validAddress(s, pageAddress) {
			t.Errorf("query/fragment handling %s", s)
		}
	}
	for _, s := range []string{"https://host/?p=repo.git;a=tree;h=refs/heads/main", "https://host/repo?path=/目录&version=GBfeature", "https://host/repo#/browse?tab=files"} {
		if !webURL(s) {
			t.Fatal(s)
		}
	}
	for _, s := range []string{"https://host/?p=repo;a=tree;si%67nature=sensitive", "https://host/#/view?sessionId=sensitive", "https://host/?x=%ZZ"} {
		if webURL(s) {
			t.Fatal("unsafe webpage accepted")
		}
	}
	for _, s := range []string{"git@host:repo.git", "ssh://git@host/repo.git", "file:///C:/example.git"} {
		if !validAddress(s, gitAddress) {
			t.Fatal(s)
		}
	}
	if validAddress("//alice:sensitive@host/repo", gitAddress) || validAddress("//host/repo?token=sensitive", gitAddress) {
		t.Fatal("schemeless authenticated URL classified as a local path")
	}
	a := testApp(t)
	tt := ui.NewTester(a.view, 1100, 810)
	input, ok := tt.AccessByRole(ui.RoleTextField, "仓库或项目地址")
	if !ok {
		t.Fatal("SVN 输入框不可访问")
	}
	tt.ClickAt(input.Bounds.X+5, input.Bounds.Y+5)
	tt.SetClipboard("https://host/repo?access_token=sensitive")
	tt.Command("paste")
	if a.d.Address != "" || a.problem == "" {
		t.Fatal("unsafe paste applied or received no feedback")
	}
}
func TestScrubExternalDefinitionsAndOldDraft(t *testing.T) {
	a := testApp(t)
	sampleSource(a)
	file := filepath.Join(t.TempDir(), "draft.json")
	a.d.Source.Externals = []string{"trunk tools https://name:sensitive@host/repo"}
	a.d.Source.ExternalWarning = "sensitive"
	a.d.Web = "https://host/repo?p=erp.git;a=tree"
	if err := saveDraft(file, a.d); err != nil {
		t.Fatal(err)
	}
	b, err := os.ReadFile(file)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(b), "sensitive") || strings.Contains(string(b), "erp.git;a=tree") {
		t.Fatal("wrote transient data to disk")
	}
	if len(a.d.Source.Externals) != 1 || a.d.Source.ExternalWarning == "" || a.d.Web == "" {
		t.Fatal("changed live data")
	}
	a.d.Remote = "https://alice:%XXsensitive@host/repo"
	if err := saveDraft(file, a.d); err == nil {
		t.Fatal("invalid typed remote saved")
	}
	a.d.Source.URL = "https://host/repo?sensitive=1"
	b, err = json.Marshal(a.d)
	if err != nil {
		t.Fatal(err)
	}
	if err = os.WriteFile(file, b, 0600); err != nil {
		t.Fatal(err)
	}
	d, err := readDraft(file)
	if err != nil {
		t.Fatal(err)
	}
	if d.Step != 0 || d.Source != nil || d.Remote != "" || d.Web != "" {
		t.Fatal("restored untrusted URI data")
	}
	b, err = os.ReadFile(file)
	if err != nil || strings.Contains(string(b), "sensitive") {
		t.Fatal("left unsafe old data on disk")
	}
}
func TestCloneCommandUsesTrunkAndPowerShellLiterals(t *testing.T) {
	m := manifest{Remote: "https://host/team/O'Brien.git", Refs: []scopeRef{{"tags/v1", "tag", "v1"}, {"trunk", "trunk", "team's/O’Brien"}}}
	want := "git clone --branch 'team''s/O’’Brien' -- 'https://host/team/O''Brien.git'"
	if got := m.cloneCommand(); got != want {
		t.Fatal(got)
	}
	a := testApp(t)
	a.d.Step = 3
	a.job = job{ID: "task", Stage: "completed", Manifest: m}
	tt := ui.NewTester(a.view, 1100, 810)
	if err := tt.Click("复制克隆命令（PowerShell）"); err != nil || tt.Clipboard() != want {
		t.Fatal(err)
	}
	m.Remote = "https://host/repo?token=sensitive"
	if m.cloneCommand() != "" {
		t.Fatal("copied remote credentials")
	}
}
func TestExternalScanWarningAndLegacyReport(t *testing.T) {
	a := testApp(t)
	sampleSource(a)
	a.d.Source.ExternalWarning = "SVN 外部依赖检查未完成（读取超时）。"
	tt := ui.NewTester(a.view, 1100, 810)
	if !tt.HasText(a.d.Source.ExternalWarning) {
		t.Fatal(tt.Texts())
	}
	raw := json.RawMessage(`{"externals_found":[{"path":"trunk","definition":"tools https://name:sensitive@host/repo"}],"svn_url":"https://host/repo?token=sensitive","commit_count":3}`)
	report := legacyJSON(raw)
	if strings.Contains(report, "sensitive") || !strings.Contains(report, "trunk") || !strings.Contains(report, "commit_count") {
		t.Fatal(report)
	}
}
