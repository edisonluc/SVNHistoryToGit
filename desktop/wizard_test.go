package main

import (
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"testing"

	"github.com/egoist/mygo/ui"
)

func testApp(t *testing.T) *app {
	t.Helper()
	a := &app{engine: cli{dir: t.TempDir()}}
	a.reset()
	return a
}
func sampleSource(a *app) {
	a.d.Address = "https://svn.example.org/repos/ERP"
	a.d.Project = a.d.Address
	a.d.Remote = "https://git.example.org/team/erp.git"
	a.d.apply(inspection{URL: a.d.Address, UUID: "u", Revision: 8421, Trunk: "trunk", Branches: []string{"branches/release", "branches/hotfix"}, Tags: []string{"tags/v1"}})
	a.d.RemoteOK = true
	a.d.Step = 2
}
func TestNativeBranchSelectionMapsOnlySelectedDirectory(t *testing.T) {
	a := testApp(t)
	project := "https://svn.example.org/%E7%A4%BA%E4%BE%8B/SampleApp"
	branch := project + "/branches/V0.X"
	a.d.Address = project
	a.chooseProject(project)
	a.tree = &directory{URL: project, Name: "SampleApp", Loaded: true, Open: true, Children: []*directory{
		{URL: project + "/branches", Name: "branches", Loaded: true, Open: true, Children: []*directory{
			{URL: branch, Name: "V0.X", Loaded: true},
		}},
	}}
	tt := ui.NewTester(a.view, 1100, 1000)
	if err := tt.Click("V0.X"); err != nil {
		t.Fatal(err)
	}
	if a.d.Project != branch || a.d.Trunk != "" || a.d.Branches != "" || a.d.Tags != "" {
		t.Fatalf("selected %s with layout %q %q %q", a.d.Project, a.d.Trunk, a.d.Branches, a.d.Tags)
	}
	a.d.apply(inspection{URL: branch, UUID: "u", Revision: 12, Trunk: a.d.Trunk})
	a.d.Main = "release"
	a.d.Step = 2
	tt.Frame()
	if !tt.HasText("主线  branches/V0.X") || !tt.HasText("→  refs/heads/release") || !tt.HasText("1 个目标分支 · 0 个标签") {
		t.Fatal(tt.Texts())
	}
	m := a.d.manifest()
	if m.URL != branch || !reflect.DeepEqual(m.Refs, []scopeRef{{"", "trunk", "release"}}) {
		t.Fatal(m)
	}
	a.chooseProject(project)
	if a.d.Source != nil || a.d.Trunk != "trunk" || a.d.Branches != "branches" || a.d.Tags != "tags" {
		t.Fatal("selecting a project must restore its layout and invalidate the branch inspection")
	}
}

func TestDirectSVNSourceURLs(t *testing.T) {
	const project = "https://svn.example.org/SampleApp"
	for _, tc := range []struct{ suffix, trunk string }{
		{"/branches/V0.X/", ""}, {"/branches/%E6%B5%8B%E8%AF%95", ""}, {"/trunk", ""},
		{"/branches/trunk", ""}, {"/branches", "trunk"}, {"/branches/V0.X/src", "trunk"},
	} {
		a := testApp(t)
		a.chooseProject(project + tc.suffix)
		if a.d.Trunk != tc.trunk || (tc.trunk == "" && (a.d.Branches != "" || a.d.Tags != "")) {
			t.Errorf("layout for %s: %q %q %q", tc.suffix, a.d.Trunk, a.d.Branches, a.d.Tags)
		}
	}
	if _, ok := parentTrunk(project + "/branches/trunk"); ok {
		t.Fatal("pasting a branch named trunk must keep the branch URL")
	}
}

func TestUninspectedBranchDraftRestoresBranchAsMainline(t *testing.T) {
	a := testApp(t)
	a.d.Address = "https://svn.example.org/SampleApp"
	a.d.Project = a.d.Address + "/branches/V0.X"
	file := a.draftPath()
	for _, trunk := range []string{"trunk", "custom/mainline"} {
		a.d.Trunk = trunk
		if err := saveDraft(file, a.d); err != nil {
			t.Fatal(err)
		}
		d, err := readDraft(file)
		if err != nil {
			t.Fatal(err)
		}
		if trunk == "trunk" && (d.Trunk != "" || d.Branches != "" || d.Tags != "") || trunk != "trunk" && d.Trunk != trunk {
			t.Fatalf("restored layout for %q: %q %q %q", trunk, d.Trunk, d.Branches, d.Tags)
		}
	}
	a.d.Trunk, a.d.LayoutEdited = "trunk", true
	if err := saveDraft(file, a.d); err != nil {
		t.Fatal(err)
	}
	d, err := readDraft(file)
	if err != nil || d.Trunk != "trunk" {
		t.Fatal("an explicitly configured layout must survive a restart", d, err)
	}
}

func TestSelectProjectInsideBranch(t *testing.T) {
	a := testApp(t)
	root := "https://svn.example.org/%E7%A4%BA%E4%BE%8B/SampleApp"
	version := root + "/branches/V0.X"
	project := version + "/SampleApp"
	a.d.Address = root
	a.tree = &directory{URL: version, Name: "V0.X", Loaded: true, Open: true, Children: []*directory{
		{URL: project, Name: "SampleApp", Loaded: true},
		{URL: version + "/Tools", Name: "Tools", Loaded: true},
	}}
	tt := ui.NewTester(a.view, 1080, 900)
	if err := tt.Click("SampleApp"); err != nil {
		t.Fatal(err)
	}
	if a.d.Project != project || !tt.HasText("迁移主线 · 将检查 trunk，若不存在则使用当前目录") {
		t.Fatal(tt.Texts())
	}
	a.d.apply(inspection{URL: project, UUID: "sample", Revision: 5})
	if a.d.Trunk != "" || a.d.Branches != "" || a.d.Tags != "" {
		t.Fatal("refresh would look for trunk again")
	}
	a.d.Main = "V0.X"
	a.d.Step = 2
	tt.Frame()
	if !tt.HasText("主线  branches/V0.X/SampleApp") || !tt.HasText("→  refs/heads/V0.X") {
		t.Fatal(tt.Texts())
	}
	want := []scopeRef{{"", "trunk", "V0.X"}}
	if m := a.d.manifest(); m.URL != project || !reflect.DeepEqual(m.Refs, want) {
		t.Fatal(m)
	}
}

func TestBranchSubprojectLayoutRespectsTrunkAndSavedChoices(t *testing.T) {
	a := testApp(t)
	root := "https://svn.example.org/repos/SampleApp"
	project := root + "/branches/V0.X/SampleApp"
	a.d.Address = root
	a.chooseProject(project)
	file := a.draftPath()
	if err := saveDraft(file, a.d); err != nil {
		t.Fatal(err)
	}
	old, err := readDraft(file)
	if err != nil || !old.autoBranchSubdir() {
		t.Fatal("uninspected draft must detect the layout", err)
	}
	for _, suffix := range []string{"", "/branches", "/branches/V0.X", "/branches/V0.X/SampleApp/tags", "/branches/V0.X/SampleApp/branches", "/branches-other/V0.X/SampleApp"} {
		a.chooseProject(root + suffix)
		if a.d.autoBranchSubdir() {
			t.Errorf("unexpected detection for %s", a.d.Project)
		}
	}
	a.chooseProject(project)
	a.d.LayoutEdited = true
	if a.d.autoBranchSubdir() {
		t.Fatal("manual trunk must be checked exactly as entered")
	}
	a.d.LayoutEdited = false
	a.d.apply(inspection{URL: project, UUID: "sample", Revision: 5, Trunk: "trunk", Tags: []string{"tags/v1"}})
	if a.d.autoBranchSubdir() || a.d.Trunk != "trunk" || a.d.Branches != "branches" || a.d.Tags != "tags" || len(a.d.manifest().Refs) != 1 {
		t.Fatal("a subproject with its own trunk must retain the standard layout")
	}
	if parent, ok := parentTrunk(project + "/trunk"); !ok || parent != project {
		t.Fatal(parent, ok)
	}
	if _, ok := parentTrunk(root + "/branches/V0.X/trunk"); ok {
		t.Fatal("pasting a trunk directly inside a branch also selected its siblings")
	}
}

func TestNativeConfirmationUpdatesAndRetainsChoices(t *testing.T) {
	a := testApp(t)
	sampleSource(a)
	tt := ui.NewTester(a.view, 1100, 810)
	if !tt.HasText("1 个目标分支 · 0 个标签") {
		t.Fatal(tt.Texts())
	}
	if err := tt.Click("branches/release"); err != nil {
		t.Fatal(err)
	}
	if !tt.HasText("2 个目标分支 · 0 个标签") {
		t.Fatal(tt.Texts())
	}
	if err := tt.Click("tags/v1"); err != nil {
		t.Fatal(err)
	}
	if !tt.HasText("2 个目标分支 · 1 个标签") {
		t.Fatal(tt.Texts())
	}
	if err := tt.Click("branches/release"); err != nil {
		t.Fatal(err)
	}
	if !tt.HasText("1 个目标分支 · 1 个标签") {
		t.Fatal(tt.Texts())
	}
	if err := tt.Click("修改 Git 目标"); err != nil {
		t.Fatal(err)
	}
	if a.d.Step != 1 || !a.d.Options[2].Selected {
		t.Fatalf("lost selection %+v", a.d)
	}
}
func TestNativeNarrowLayoutAndResultActions(t *testing.T) {
	a := testApp(t)
	sampleSource(a)
	tt := ui.NewTester(a.view, 520, 1400)
	tt.SetScale(1.5)
	scope, ok := tt.Find("选择迁移范围")
	if !ok {
		t.Fatal(tt.Texts())
	}
	list, ok := tt.Find("本次发布清单")
	if !ok || list.Y <= scope.Y {
		t.Fatalf("选择区 %v 清单 %v", scope, list)
	}
	button, ok := tt.Find("开始迁移")
	if !ok || button.Y <= list.Y {
		t.Fatal(button)
	}
	a.d.Step = 3
	a.job = job{ID: "job", Manifest: a.d.manifest(), Stage: "remote_verify"}
	tt.Frame()
	if tt.HasText("迁移完成") || !tt.HasText("重新核对远端") {
		t.Fatal(tt.Texts())
	}
	if got := tt.Announcements(); !slices.Contains(got, "已推送，等待核对") {
		t.Fatalf("missing stage announcement: %v", got)
	}
	a.job.Stage = "completed"
	a.job.Manifest.Web = "https://git.example.org/team/erp"
	a.job.Verified = []verifiedRef{{Full: "refs/heads/main", OID: "123", Commit: "123", Files: 3, Revisions: 2}}
	tt.Frame()
	if !tt.HasText("迁移完成") {
		t.Fatal(tt.Texts())
	}
	if got := tt.Announcements(); !slices.Contains(got, "迁移完成") {
		t.Fatalf("missing completion announcement: %v", got)
	}
	if err := tt.Click("复制提交 ID"); err != nil || tt.Clipboard() != "123" {
		t.Fatal(err, tt.Clipboard())
	}
	if err := tt.Click("打开远端仓库"); err != nil || len(tt.OpenedURLs()) != 1 {
		t.Fatal(err, tt.OpenedURLs())
	}
}
func TestDraftPreservesAdvancedRulesAndRechecksRemote(t *testing.T) {
	a := testApp(t)
	sampleSource(a)
	if g, l, e := a.d.fileRules(); g != nil || l != nil || e != nil {
		t.Fatal(g, l, e)
	}
	a.d.ExcludeEnabled = true
	a.d.Exclusions = "*.tmp\n\ncache/**"
	a.d.LFSEnabled = true
	a.d.LFSSize = "48"
	a.d.Options[2].Selected = true
	a.d.Options[2].Name = "release/v1"
	file := filepath.Join(t.TempDir(), "draft.json")
	if err := saveDraft(file, a.d); err != nil {
		t.Fatal(err)
	}
	d, err := readDraft(file)
	if err != nil {
		t.Fatal(err)
	}
	if d.Step != 1 || d.RemoteOK || len(d.manifest().Refs) != 2 {
		t.Fatal(d)
	}
	m := d.manifest()
	if m.LFSThreshold == nil || *m.LFSThreshold != 48 || !reflect.DeepEqual(m.ExcludeGlobs, []string{"*.tmp", "cache/**"}) || m.Refs[1].Name != "release/v1" {
		t.Fatal(m)
	}
	d.LFSSize = "0"
	if _, _, err := d.fileRules(); err == nil {
		t.Fatal("invalid LFS threshold")
	}
	d.LFSEnabled = false
	d.ExcludeEnabled = false
	if g, v, e := d.fileRules(); g != nil || v != nil || e != nil {
		t.Fatal(g, v, e)
	}
	d.Remote = "https://user:secret@host/repo"
	if err := saveDraft(file, *d); err == nil {
		t.Fatal("stored credentials in URL")
	}
}
func TestDirectoryAndWebURLs(t *testing.T) {
	u, e := joinDirectory("file:///C:/repos", "100% 中文 #@")
	if e != nil || !strings.Contains(u, "%25") || !strings.Contains(u, "%23") {
		t.Fatal(u, e)
	}
	if got := suggestedWeb("git@github.com:team/repo.git", "release/v1"); got != "https://github.com/team/repo/tree/release%2Fv1" {
		t.Fatal(got)
	}
	if got := suggestedWeb("https://custom.example/repo.git", "main"); got != "" {
		t.Fatal(got)
	}
	if got, ok := parentTrunk("https://svn.example.org/proj/trunk/"); !ok || got != "https://svn.example.org/proj" {
		t.Fatal(got)
	}
}

func TestEditFailedJobMakesNewDraftBeforePublication(t *testing.T) {
	a := testApp(t)
	sampleSource(a)
	a.d.Options[0].Selected = true
	a.d.Options[0].Name = "renamed"
	a.d.LFSEnabled = true
	a.d.LFSSize = "12"
	m := a.d.manifest()
	a.job = job{ID: "keep-original", Stage: "failed", FailedStage: "verify", Manifest: m, PublicationStarted: true}
	a.d.Step = 3
	a.revise()
	if a.d.Step != 3 {
		t.Fatal("changed a published task")
	}
	a.job.PublicationStarted = false
	a.revise()
	if a.d.Step != 1 || a.d.RemoteOK || a.frozen || a.job.ID != "" {
		t.Fatal("copied draft must check target and source again")
	}
	if !reflect.DeepEqual(a.d.manifest(), m) {
		t.Fatal(a.d.manifest(), m)
	}
}

func TestRefConflictsUseGitNamespaces(t *testing.T) {
	refs := []scopeRef{{"trunk", "trunk", "release"}, {"branches/test", "branch", "release/next"}, {"tags/test", "tag", "release"}, {"branches/bad", "branch", "old..new"}}
	problems := refErrors(refs, nil)
	if len(problems) != 3 || problems[0] == "" || problems[1] == "" || problems[3] == "" {
		t.Fatal(problems)
	}
}

func TestExistingRepositoryConfirmationChecksSelectedNames(t *testing.T) {
	a := testApp(t)
	sampleSource(a)
	a.d.Main = "V0.X"
	a.d.remoteRefs = []string{"refs/heads/main", "refs/heads/release/old", "refs/tags/V0.X", "refs/tags/v1"}
	tt := ui.NewTester(a.view, 1100, 1000)
	if !tt.HasText("→  refs/heads/V0.X") {
		t.Fatal(tt.Texts())
	}
	for _, tc := range []struct{ source, conflict string }{
		{"branches/release", "refs/heads/release/old"}, {"tags/v1", "refs/tags/v1"},
	} {
		if err := tt.Click(tc.source); err != nil {
			t.Fatal(err)
		}
		if !strings.Contains(strings.Join(tt.Texts(), "\n"), tc.conflict+"，请更换目标名称") {
			t.Fatal(tt.Texts())
		}
		a.execute(false)
		if a.running || a.d.Step != 2 {
			t.Fatal("a conflicting selection started a migration")
		}
		if err := tt.Click(tc.source); err != nil {
			t.Fatal(err)
		}
		if strings.Contains(strings.Join(tt.Texts(), "\n"), "请更换目标名称") {
			t.Fatal(tt.Texts())
		}
	}
}

func TestSavedCredentialNeedsSelectionForCurrentHost(t *testing.T) {
	a := testApp(t)
	a.d.Address = "https://svn.example.org/repos"
	a.svnAuth.open = true
	a.svnAuth.showProfiles = true
	a.credentialPage.profiles = []credentialProfile{{ID: "credential-id", Name: "团队账号", Username: "alice", Host: "other.example.org", Kind: "svn_password"}}
	tt := ui.NewTester(a.view, 1000, 900)
	if a.d.SVNID != "" {
		t.Fatal("applied without selection")
	}
	if err := tt.Click("使用 团队账号 · alice · other.example.org"); err != nil {
		t.Fatal(err)
	}
	if a.d.SVNID != "credential-id" {
		t.Fatal(a.d.SVNID)
	}
	if sameHost(a.d.Address, "https://other.example.org/repos") {
		t.Fatal("carried profile across hosts")
	}
}
