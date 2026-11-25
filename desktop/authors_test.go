package main

import (
	"encoding/json"
	"slices"
	"strings"
	"testing"

	"github.com/egoist/mygo/ui"
)

func TestAuthorsImportPreviewsChangesAndPreservesGlobalRows(t *testing.T) {
	old := author{"alice", "Old Name", "old@example.org"}
	book := []author{old, {"same", "Same", "same@example.org"}}
	raw := "\uFEFF# exported file\r\n  \r\nalice = Alice A <alice@example.org>\r\nbob = 鲍勃 <bob@example.org>\r\nwrong\nweird = Invalid <no-at>\nbob = Other <other@example.org>\nsame = Same <same@example.org>\n"
	rows := previewAuthors(raw, book)
	var statuses []string
	for _, row := range rows {
		statuses = append(statuses, row.status)
	}
	if !slices.Equal(statuses, []string{"更新", "新增", "无效", "无效", "无效", "未更改"}) {
		t.Fatalf("preview classifications: %v", rows)
	}
	if rows[2].line != 5 || !strings.Contains(rows[4].problem, "重复") || book[0] != old {
		t.Fatalf("incorrect preview or book was modified: %+v / %v", rows, book)
	}
	changes := importChanges(rows)
	if !slices.Equal(changes, []author{{"alice", "Alice A", "alice@example.org"}, {"bob", "鲍勃", "bob@example.org"}}) {
		t.Fatalf("items eligible for import: %+v", changes)
	}
}

func TestMalformedAuthorIdentitiesRejected(t *testing.T) {
	for _, entry := range []author{
		{"user", "First\nLast", "u@example.org"}, {"user", "Name", "bad<>@example.org"},
		{"user", "Name", "@example.org"}, {"user", "Name", "me@"}, {"user", "Name", "bad mail@example.org"},
		{" ", "Name", "n@example.org"}, {"user", " ", "n@example.org"},
	} {
		if authorProblem([]author{entry}) == "" {
			t.Errorf("accepted malformed identity: %+v", entry)
		}
	}
}

func TestEditingOneMigrationLeavesTheBookAndNextDraftAlone(t *testing.T) {
	a := testApp(t)
	sampleSource(a)
	p := &a.authorsPage
	p.scanScope = a.d.authorScope()
	if err := json.Unmarshal([]byte(`[{"svn_user":"alice","git_name":"Alice","git_email":"original@example.org","in_library":true}]`), &p.scanned); err != nil {
		t.Fatal(err)
	}
	original := author{"alice", "Alice", "original@example.org"}
	a.authorBook = []author{original}
	a.authorsOpen = true
	tt := ui.NewTester(a.authorsView, 440, 640)
	if err := tt.Click("仅修改本次"); err != nil {
		t.Fatal(err)
	}
	if len(a.d.Authors) != 1 || !tt.HasText("Git 邮箱") {
		t.Fatal(tt.Texts(), a.d.Authors)
	}
	a.d.Authors[0].Email = "once@example.org"
	tt.Frame()
	if err := tt.Click("完成编辑"); err != nil {
		t.Fatal(err)
	}
	if !tt.HasText("Alice <once@example.org>") || a.d.manifest().Authors[0].Email != "once@example.org" || a.authorBook[0] != original {
		t.Fatal(tt.Texts(), a.d.manifest().Authors, a.authorBook)
	}
	a.setAuthorBook([]author{{"alice", "Alice", "updated@example.org"}})
	tt.Frame()
	if !tt.HasText("Alice <once@example.org>") {
		t.Fatal("editing the shared record overwrote an active one-off identity")
	}
	if err := tt.Click("撤销覆盖"); err != nil || !tt.HasText("Alice <updated@example.org>") {
		t.Fatal(err, tt.Texts())
	}
	a.setAuthorBook(nil)
	tt.Frame()
	if !tt.HasText("默认身份") || !tt.HasText("alice <alice@svn.local>") {
		t.Fatal("deletion must update a live scan", tt.Texts())
	}
	a.reset()
	sampleSource(a)
	if len(a.d.manifest().Authors) != 0 || len(a.authorsPage.scanned) != 0 {
		t.Fatal("new drafts inherited author overrides or scan rows")
	}
}

func TestAuthorLibraryCancelDeleteAndImportKeepOriginalMapping(t *testing.T) {
	a := testApp(t)
	original := author{"alice", "Alice", "alice@example.org"}
	a.authorBook = []author{original}
	a.editGlobalAuthor(original)
	tt := ui.NewTester(func(c *ui.Context) { a.authorsLibraryView(c, true) }, 480, 650)
	if err := tt.Click("删除"); err != nil {
		t.Fatal(err)
	}
	if len(a.authorBook) != 1 || !tt.HasText("删除「alice」的共用映射？") {
		t.Fatal(tt.Texts(), a.authorBook)
	}
	if err := tt.Click("取消删除"); err != nil {
		t.Fatal(err)
	}
	p := &a.authorsPage
	p.mode, p.file = "import", "authors.txt"
	p.preview = previewAuthors("alice = Another <a@example.org>\nerror line", a.authorBook)
	tt.Frame()
	if !tt.HasText("导入 1 条有效更改") || !tt.HasText("error line") {
		t.Fatal(tt.Texts())
	}
	if err := tt.Click("取消导入"); err != nil {
		t.Fatal(err)
	}
	if p.preview != nil || !slices.Equal(a.authorBook, []author{original}) {
		t.Fatal(a.authorBook, p.preview)
	}
}

func TestConfirmScopeAndUndoRestoreMappedIdentity(t *testing.T) {
	a := testApp(t)
	sampleSource(a)
	alice := author{"alice", "Alice", "shared@example.org"}
	p := &a.authorsPage
	p.scanScope = a.d.authorScope()
	p.scanned = []scannedAuthor{{author: alice, InLibrary: true}}
	a.d.Authors = []author{{"alice", "Alice", "once@example.org"}}
	a.authorsOpen = true
	tt := ui.NewTester(a.authorsView, 440, 640)
	if !tt.HasText("仅本次") || !tt.HasText("Alice <once@example.org>") {
		t.Fatal(tt.Texts())
	}
	if err := tt.Click("撤销覆盖"); err != nil {
		t.Fatal(err)
	}
	if !tt.HasText("来自作者库") || !tt.HasText("Alice <shared@example.org>") || len(a.d.manifest().Authors) != 0 {
		t.Fatal(tt.Texts(), a.d.manifest().Authors)
	}
	a.d.Main = "other-target"
	tt.Frame()
	if !tt.HasText("Alice <shared@example.org>") {
		t.Fatal("renaming a Git target should retain the SVN author list")
	}
	a.d.Options[0].Selected = true
	tt.Frame()
	if tt.HasText("Alice <shared@example.org>") || !tt.HasText("查看本次作者") {
		t.Fatal("changed scope showed previous authors", tt.Texts())
	}
	p.scanScope = a.d.authorScope()
	p.scanned = []scannedAuthor{{author: alice, InLibrary: true}}
	a.d.Source.Revision++
	tt.Frame()
	if len(p.scanned) > 0 {
		t.Fatal("changing the pinned revision must invalidate the scan")
	}
}

func TestAuthorSearchAndSelectionOnNarrowScreen(t *testing.T) {
	a := testApp(t)
	a.authorBook = []author{{"alice", "Alice A", "alice@example.org"}, {"bob", "Bob B", "b@other.org"}}
	a.authorsPage.query = "OTHER.org"
	tt := ui.NewTester(func(c *ui.Context) { a.authorsLibraryView(c, true) }, 480, 650)
	if !tt.HasText("bob") || tt.HasText("alice") {
		t.Fatal(tt.Texts())
	}
	if err := tt.Click("bob"); err != nil {
		t.Fatal(err)
	}
	if !tt.HasText("Git 姓名") || a.authorsPage.edit.Name != "Bob B" {
		t.Fatal(tt.Texts())
	}
	if err := tt.Click("返回列表"); err != nil {
		t.Fatal(err)
	}
	if !tt.HasText("bob") || tt.HasText("Git 姓名") {
		t.Fatal(tt.Texts())
	}
}

func TestWideAuthorLibraryOpensFirstMappingAndKeepsEditedRecord(t *testing.T) {
	a := testApp(t)
	a.authorBook = []author{{"alice", "Alice", "alice@example.org"}, {"bob", "Bob", "bob@example.org"}}
	tt := ui.NewTester(func(c *ui.Context) { a.authorsLibraryView(c, false) }, 1050, 675)
	if a.authorsPage.selected != "alice" || !tt.HasText("保存映射") {
		t.Fatal("first record has no detail view", tt.Texts())
	}
	if err := tt.Click("编辑 bob"); err != nil {
		t.Fatal(err)
	}
	a.authorsPage.edit.Name = "B. Example"
	tt.Frame()
	if a.authorsPage.selected != "bob" || a.authorsPage.edit.Name != "B. Example" || a.authorBook[1].Name != "Bob" {
		t.Fatal("rendering must preserve edits until explicitly saved")
	}
}

func TestSavedManualOverrideCanBeFixedOutsideScannedScope(t *testing.T) {
	a := testApp(t)
	sampleSource(a)
	a.d.Authors = []author{{"archived", "", "broken"}, {"other", "Other", "other@example.org"}}
	a.authorsPage.scanScope = a.d.authorScope()
	a.authorsPage.scanned = []scannedAuthor{{author: author{"alice", "Alice", "alice@example.org"}}}
	tt := ui.NewTester(a.authorsView, 480, 850)
	if !tt.HasText("archived") || !tt.HasText("other") {
		t.Fatal("saved override disappeared after scanning an unrelated path", tt.Texts())
	}
	if err := tt.Click("编辑 archived"); err != nil {
		t.Fatal(err)
	}
	a.authorsPage.manual.entry = author{"archive-user", "Archive", "a@example.org"}
	tt.Frame()
	if err := tt.Click("应用到本次"); err != nil {
		t.Fatal(err)
	}
	if len(a.d.Authors) != 2 || a.d.Authors[0].SVNUser != "archive-user" || authorProblem(a.d.Authors) != "" {
		t.Fatal(a.d.Authors)
	}
	a.d.Source.Revision++
	tt.Frame()
	if err := tt.Click("移除 other"); err != nil {
		t.Fatal(err)
	}
	if len(a.d.Authors) != 1 || !tt.HasText("archive-user") || !tt.HasText("查看本次作者") {
		t.Fatal(tt.Texts(), a.d.Authors)
	}
}
