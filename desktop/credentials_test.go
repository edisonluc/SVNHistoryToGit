package main

import (
	"slices"
	"strings"
	"testing"

	"github.com/egoist/mygo/ui"
)

func TestLegacyCredentialUsesLoginAsOneReadableLabel(t *testing.T) {
	for _, name := range []string{"alice", ""} {
		p := credentialProfile{Name: name, Username: "alice"}
		if got := p.label(); got != "alice" {
			t.Fatalf("legacy label %q", got)
		}
	}
}

func TestCredentialSuggestionsRankServerAndPortAndExcludeOtherTransports(t *testing.T) {
	records := []credentialProfile{
		{ID: "other-port", Kind: gitToken, Host: "git.example:9443"},
		{ID: "legacy", Kind: gitToken, Host: "*"},
		{ID: "unassigned", Kind: gitToken},
		{ID: "match", Kind: gitToken, Host: "Git.Example:8443"},
		{ID: "other-account", Kind: gitToken, Host: "git.example:8443"},
		{ID: "ssh", Kind: gitSSH, Host: "git.example"},
		{ID: "svn", Kind: svnPassword, Host: "git.example:8443"},
	}
	groups := credentialSuggestions(records, repositoryServer("https://git.example:8443/group/repo", false), false)
	if len(groups[0]) != 2 || groups[0][0].ID != "match" || groups[0][1].ID != "other-account" || len(groups[1]) != 2 || len(groups[2]) != 1 || groups[2][0].ID != "other-port" {
		t.Fatal(groups)
	}
	groups = credentialSuggestions(records, repositoryServer("git@git.example:group/repo", false), false)
	if len(groups[0]) != 1 || groups[0][0].ID != "ssh" || len(groups[1])+len(groups[2]) != 0 {
		t.Fatal(groups)
	}
	if !repositoryServer("https://svn.example/repo", true).matches(credentialProfile{Kind: svnPassword, Host: "https://svn.example/"}) {
		t.Fatal("legacy full URL host prefix lost its server match")
	}
	for _, pair := range [][2]string{
		{"https://git.example/a", "https://Git.Example:443/b"},
		{"git@[2001:db8::1]:a", "ssh://git@[2001:db8::1]:22/b"},
		{"git.example:group/a", "ssh://git.example/group/b"},
	} {
		if !sameHost(pair[0], pair[1]) {
			t.Errorf("same server: %v", pair)
		}
	}
	for _, pair := range [][2]string{
		{"https://git.example/a", "http://git.example/a"},
		{"https://git.example/a", "https://git.example:8443/a"},
		{"git@[2001:db8::1]:a", "git@[2001:db8::2]:b"},
		{"https://git.example/a", "git@git.example:a"},
		{"file:///C:/git/one", "file:///C:/git/two"},
	} {
		if sameHost(pair[0], pair[1]) {
			t.Errorf("different servers: %v", pair)
		}
	}
}

func TestCredentialEditorSeparatesMetadataAndSecretRotation(t *testing.T) {
	e := credentialEditor{credentialProfile: credentialProfile{ID: "a", Name: "发布", Username: "alice", Host: "git.example", Kind: gitSSH, Key: "id_ed25519"}, secret: "unused"}
	metadata := e.request()
	if _, ok := metadata["secret"]; ok || metadata["ssh_key_path"] != e.Key || metadata["id"] != "a" {
		t.Fatal("invalid metadata request")
	}
	e.rotate = true
	e.secret = ""
	if value, ok := e.request()["secret"]; !ok || value != "" {
		t.Fatal("cannot explicitly clear an SSH passphrase")
	}
	e.secret = "replacement"
	if e.request()["secret"] != e.secret {
		t.Fatal("cannot replace passphrase")
	}
}

func TestCredentialLibraryDistinguishesNoMatchFromEmptyLibrary(t *testing.T) {
	a := testApp(t)
	a.credentialPage.profiles = []credentialProfile{{ID: "sample", Name: "发布", Username: "alice", Kind: gitToken}}
	a.credentialPage.search = "unmatched"
	tt := ui.NewTester(func(c *ui.Context) { a.credentialsLibraryView(c, false) }, 1080, 650)
	if !tt.HasText("没有匹配的凭据") || tt.HasText("尚未添加凭据") {
		t.Fatal(tt.Texts())
	}
	a.credentialPage.search = "alice"
	tt.Frame()
	if a.credentialPage.edit.ID != "sample" || !tt.HasText("编辑凭据") {
		t.Fatal(tt.Texts())
	}
}

func TestNewCredentialTypeSwitchClearsSecretAndKeepsAccount(t *testing.T) {
	a := testApp(t)
	page := &a.credentialPage
	page.selectProfile(credentialProfile{Kind: svnPassword, Name: "归档", Username: "user"})
	page.edit.secret = "unsaved"
	tt := ui.NewTester(func(c *ui.Context) { a.credentialsLibraryView(c, false) }, 1060, 750)
	if err := tt.Click("选择凭据类型"); err != nil {
		t.Fatal(err)
	}
	if err := tt.Click("Git SSH 私钥"); err != nil {
		t.Fatal(err)
	}
	if page.edit.Kind != gitSSH || page.edit.secret != "" || page.edit.Username != "user" || !tt.HasText("SSH 私钥路径") {
		t.Fatal("switching type did not update the authentication form", tt.Texts())
	}
}

func TestCredentialLibrarySearchAndSwitchingNarrowDetails(t *testing.T) {
	a := testApp(t)
	first := credentialProfile{ID: "a", Name: "发布", Username: "alice", Host: "git.example", Kind: gitToken}
	second := credentialProfile{ID: "b", Name: "同步", Username: "bob", Host: "svn.example:8443", Kind: svnPassword}
	a.credentialPage.profiles = []credentialProfile{first, second}
	a.credentialPage.loaded = true
	tt := ui.NewTester(func(c *ui.Context) { a.credentialsLibraryView(c, true) }, 480, 750)
	if err := tt.Click("搜索凭据"); err != nil {
		t.Fatal(err)
	}
	tt.Type("8443")
	if tt.HasText(first.label()) || !tt.HasText(second.label()) {
		t.Fatal(tt.Texts())
	}
	if err := tt.Click("编辑 " + second.label()); err != nil {
		t.Fatal(err)
	}
	if a.credentialPage.edit.ID != "b" || !tt.HasText("返回列表") || !tt.HasText("更新密码") {
		t.Fatal(tt.Texts())
	}
	if _, ok := tt.Find("密码"); ok {
		t.Fatal("secret editor appeared before rotation was requested")
	}
	if err := tt.Click("更新密码"); err != nil {
		t.Fatal(err)
	}
	if _, ok := tt.Find("密码"); !ok {
		t.Fatal("missing password editor")
	}
	a.credentialPage.edit.secret = "temporary"
	if err := tt.Click("删除凭据"); err != nil {
		t.Fatal(err)
	}
	if !slices.ContainsFunc(tt.Texts(), func(text string) bool {
		return strings.Contains(text, second.label()) && strings.Contains(text, "系统凭据")
	}) {
		t.Fatal("deletion prompt does not identify the account", tt.Texts())
	}
	if err := tt.Click("取消"); err != nil {
		t.Fatal(err)
	}
	if err := tt.Click("返回列表"); err != nil {
		t.Fatal(err)
	}
	if a.credentialPage.edit.secret != "" || a.credentialPage.editing || a.credentialPage.search != "8443" {
		t.Fatal("returned list lost its search or retained a secret")
	}
	a.credentialPage.selectProfile(first)
	tt.Frame()
	if a.credentialPage.edit.rotate || !tt.HasText("更新访问令牌") || tt.HasText("更新密码") {
		t.Fatal(tt.Texts())
	}
	if got := a.credentialSummary("Git 凭据", "a"); got != "Git 凭据 · 发布 · alice" {
		t.Fatal(got)
	}
}
