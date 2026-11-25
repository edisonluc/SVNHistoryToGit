package main

import (
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"unicode"
	"unicode/utf8"

	"github.com/egoist/mygo"
	"github.com/egoist/mygo/ui"
)

type authorLibrary struct {
	query, mode, selected string
	edit                  author
	deleting              bool
	file                  string
	preview               []authorImportLine
	scanScope             *authorScope
	scanned               []scannedAuthor
	editingOverride       string
	manual                manualOverrideEditor
}

type manualOverrideEditor struct {
	open    bool
	index   int
	entry   author
	problem string
}

type scannedAuthor struct {
	author
	InLibrary bool `json:"in_library"`
}

type authorScope struct {
	url, uuid, credential string
	revision              int64
	paths                 []string
}

func (d *draft) authorScope() *authorScope {
	if d.Source == nil {
		return nil
	}
	paths := []string{d.Source.Trunk}
	for _, o := range d.Options {
		if o.Selected {
			paths = append(paths, o.Path)
		}
	}
	slices.Sort(paths)
	return &authorScope{d.Source.URL, d.Source.UUID, d.SVNID, d.Source.Revision, paths}
}

func (s *authorScope) matches(other *authorScope) bool {
	return s != nil && other != nil && s.url == other.url && s.uuid == other.uuid &&
		s.credential == other.credential && s.revision == other.revision && slices.Equal(s.paths, other.paths)
}

func (r author) trimmed() author {
	return author{strings.TrimSpace(r.SVNUser), strings.TrimSpace(r.Name), strings.TrimSpace(r.Email)}
}

func (r author) problem() string {
	if strings.TrimSpace(r.SVNUser) == "" || strings.TrimSpace(r.Name) == "" || r.Email == "" {
		return "请填写 SVN 用户名、Git 姓名和邮箱"
	}
	if strings.IndexFunc(r.SVNUser+r.Name+r.Email, unicode.IsControl) >= 0 || strings.ContainsAny(r.Name+r.Email, "<>") {
		return "用户名、姓名和邮箱须为单行文本，Git 姓名和邮箱不能含有尖括号"
	}
	local, domain, ok := strings.Cut(r.Email, "@")
	if !ok || local == "" || domain == "" || strings.Contains(domain, "@") || strings.IndexFunc(r.Email, unicode.IsSpace) >= 0 {
		return "请填写有效的 Git 邮箱"
	}
	return ""
}

func authorProblem(authors []author) string {
	seen := make(map[string]bool, len(authors))
	for _, r := range authors {
		if problem := r.problem(); problem != "" {
			return problem
		}
		if seen[r.SVNUser] {
			return "SVN 用户名重复：" + r.SVNUser
		}
		seen[r.SVNUser] = true
	}
	return ""
}

type authorImportLine struct {
	line                 int
	status, problem, raw string
	entry, previous      author
}

func previewAuthors(text string, book []author) []authorImportLine {
	existing := make(map[string]author, len(book))
	for _, r := range book {
		existing[r.SVNUser] = r
	}
	var rows []authorImportLine
	seen := map[string]bool{}
	for idx, raw := range strings.Split(strings.TrimPrefix(text, "\uFEFF"), "\n") {
		raw = strings.TrimSpace(raw)
		if raw == "" || strings.HasPrefix(raw, "#") {
			continue
		}
		row := authorImportLine{line: idx + 1, raw: raw, status: "无效"}
		user, identity, equal := strings.Cut(raw, "=")
		name, address, bracket := strings.Cut(identity, "<")
		email, closed := strings.CutSuffix(address, ">")
		row.entry = (author{user, name, email}).trimmed()
		switch {
		case !utf8.ValidString(raw):
			row.problem = "请将文件保存为 UTF-8 文本"
		case !equal || !bracket || !closed:
			row.problem = "格式应为 用户名 = 姓名 <邮箱>"
		case row.entry.problem() != "":
			row.problem = row.entry.problem()
		case seen[row.entry.SVNUser]:
			row.problem = "文件中重复的 SVN 用户名"
		default:
			seen[row.entry.SVNUser] = true
			prior, found := existing[row.entry.SVNUser]
			row.previous = prior
			switch {
			case !found:
				row.status = "新增"
			case prior == row.entry:
				row.status = "未更改"
			default:
				row.status = "更新"
			}
		}
		rows = append(rows, row)
	}
	return rows
}

func importChanges(rows []authorImportLine) []author {
	var entries []author
	for _, row := range rows {
		if row.status == "新增" || row.status == "更新" {
			entries = append(entries, row.entry)
		}
	}
	return entries
}

func (a *app) openAuthors() {
	a.openLibrary("authors")
	p := &a.authorsPage
	p.mode, p.selected, p.query, p.preview, p.deleting = "", "", "", nil, false
	var book []author
	a.start("读取作者库", func() error {
		return a.engine.call(request{"op": "authors"}, &book, nil)
	}, func(err error) {
		if err == nil {
			a.setAuthorBook(book)
		}
	})
}

// Refresh scanned identities when the library changes while the wizard is open.
func (a *app) setAuthorBook(book []author) {
	a.authorBook = book
	byUser := make(map[string]author, len(book))
	for _, r := range book {
		byUser[r.SVNUser] = r
	}
	for i, row := range a.authorsPage.scanned {
		r, found := byUser[row.SVNUser]
		if !found {
			r = author{row.SVNUser, row.SVNUser, row.SVNUser + "@svn.local"}
		}
		a.authorsPage.scanned[i] = scannedAuthor{r, found}
	}
}

func (a *app) editGlobalAuthor(r author) {
	p := &a.authorsPage
	p.selected, p.edit, p.mode, p.deleting, p.preview = r.SVNUser, r, "edit", false, nil
	a.problem, a.msg = "", ""
}

func (a *app) saveGlobalAuthors(entries []author) {
	if problem := authorProblem(entries); problem != "" {
		a.problem = problem
		return
	}
	p := &a.authorsPage
	a.start("保存作者映射", func() error {
		return a.engine.call(request{"op": "save_authors", "authors": entries}, &struct{}{}, nil)
	}, func(err error) {
		if err != nil {
			return
		}
		replaced := make(map[string]bool, len(entries))
		for _, r := range entries {
			replaced[r.SVNUser] = true
		}
		book := slices.DeleteFunc(a.authorBook, func(r author) bool { return replaced[r.SVNUser] })
		book = append(book, entries...)
		slices.SortFunc(book, func(l, r author) int { return strings.Compare(l.SVNUser, r.SVNUser) })
		a.setAuthorBook(book)
		if p.mode == "import" {
			p.mode, p.preview = "", nil
		} else {
			a.editGlobalAuthor(entries[0])
		}
		a.msg = fmt.Sprintf("已保存 %d 条作者映射", len(entries))
	})
}

func (a *app) deleteGlobalAuthor(user string) {
	p := &a.authorsPage
	a.start("删除作者映射", func() error {
		return a.engine.call(request{"op": "delete_author", "svn_user": user}, &struct{}{}, nil)
	}, func(err error) {
		if err == nil {
			a.setAuthorBook(slices.DeleteFunc(a.authorBook, func(r author) bool { return r.SVNUser == user }))
			p.mode, p.selected, p.deleting = "", "", false
			a.msg = "已删除 " + user + " 的作者映射"
		}
	})
}

func (a *app) pickAuthorsFile() {
	var path string
	var data []byte
	a.start("读取作者文件", func() error {
		files, err := mygo.Dialog.Open(mygo.OpenDialogOptions{Parent: a.win, Title: "导入 authors.txt",
			Filters: []mygo.FileFilter{{Name: "作者映射文本", Extensions: []string{"txt"}}, {Name: "所有文件", Extensions: []string{"*"}}}})
		if err != nil || len(files) == 0 {
			return err
		}
		path = files[0]
		data, err = os.ReadFile(path)
		return err
	}, func(err error) {
		if err == nil && path != "" {
			p := &a.authorsPage
			p.mode, p.selected, p.deleting = "import", "", false
			p.file, p.preview = filepath.Base(path), previewAuthors(string(data), a.authorBook)
		}
	})
}

func (a *app) authorsLibraryView(c *ui.Context, small bool) {
	p := &a.authorsPage
	query := strings.ToLower(strings.TrimSpace(p.query))
	matches := func(r author) bool {
		return strings.Contains(strings.ToLower(r.SVNUser+"\n"+r.Name+"\n"+r.Email), query)
	}
	if !small && p.mode == "" && a.busy == "" {
		if idx := slices.IndexFunc(a.authorBook, matches); idx >= 0 {
			p.selected, p.edit, p.mode = a.authorBook[idx].SVNUser, a.authorBook[idx], "edit"
		}
	}
	a.libraryPanes(c, small, p.mode != "", func() {
		ui.Row(c).FillWidth().AlignItems(ui.Center).Gap(2).Children(func() {
			ui.SearchField(c.Key("author-search"), &p.query).Label("搜索作者").Placeholder("搜索作者").MinWidth(0).Grow(1)
			libraryTool(c, "新增作者", glyphPlus, func() { a.editGlobalAuthor(author{}) })
			libraryTool(c, "导入 authors.txt", glyphImport, a.pickAuthorsFile)
		})
		count := 0
		ui.Column(c).FillWidth().Gap(2).Children(func() {
			for _, r := range a.authorBook {
				if !matches(r) {
					continue
				}
				count++
				recordRow(c, r.SVNUser, "编辑 "+r.SVNUser, r.SVNUser, r.Name+" · "+r.Email, glyphUser,
					r.SVNUser == p.selected, func() { a.editGlobalAuthor(r) })
			}
		})
		if count == 0 && a.busy == "" {
			if query == "" {
				hint(c, "暂无作者")
			} else {
				hint(c, "没有匹配的作者")
			}
		}
	}, func() {
		if small {
			ui.Row(c).Children(func() {
				ui.Button(c, "返回列表").OnClick(func() { p.mode, p.selected, p.deleting, p.preview = "", "", false, nil })
			})
		}
		switch p.mode {
		case "edit":
			ui.Column(c.Key("edit:" + p.selected)).FillWidth().Gap(20).Children(func() { a.globalAuthorForm(c) })
		case "import":
			ui.Column(c).FillWidth().Gap(20).Children(func() { a.authorImportView(c) })
		default:
			if a.busy != "" {
				return
			}
			if query != "" {
				libraryEmpty(c, glyphSearch, "没有匹配的作者", "可按 SVN 用户名、Git 姓名或邮箱搜索。")
				return
			}
			libraryEmpty(c, glyphUser, "作者映射", "新增一位作者，或导入已有的 authors.txt 映射文件。")
			ui.Row(c).Gap(8).Wrap().Children(func() {
				primary(c, "新增作者", a.busy != "", func() { a.editGlobalAuthor(author{}) })
				ui.Button(c, "导入 authors.txt").Disabled(a.busy != "").OnClick(a.pickAuthorsFile)
			})
		}
	})
}

func (a *app) globalAuthorForm(c *ui.Context) {
	p := &a.authorsPage
	heading := "编辑作者"
	if p.selected == "" {
		heading = "新增作者"
	}
	libraryDetailHeader(c, heading, "多个 SVN 仓库中的同名用户共用这条映射。")
	libraryGroup(c, "提交身份", glyphUser, func() {
		librarySetting(c, "SVN 用户名", "原始提交记录中的账号", false, func() {
			if p.selected == "" {
				libraryInput(c, "SVN 用户名", &p.edit.SVNUser, "用户名")
			} else {
				ui.Text(c, p.selected).FontSize(13).Selectable()
			}
		})
		librarySetting(c, "Git 姓名", "", false, func() { libraryInput(c, "Git 姓名", &p.edit.Name, "姓名") })
		librarySetting(c, "Git 邮箱", "", true, func() { libraryInput(c, "Git 邮箱", &p.edit.Email, "name@example.org") })
	})
	ui.Row(c).FillWidth().AlignItems(ui.Center).Gap(8).Children(func() {
		if p.selected != "" {
			ui.Button(c, "删除").TextColor(warn).OnClick(func() { p.deleting = true })
		}
		ui.Spacer(c)
		primary(c, "保存映射", false, func() {
			r := p.edit.trimmed()
			if p.selected == "" && slices.ContainsFunc(a.authorBook, func(saved author) bool { return saved.SVNUser == r.SVNUser }) {
				a.problem = "该 SVN 用户名已有映射，请从列表中选择并修改"
				return
			}
			a.saveGlobalAuthors([]author{r})
		})
	})
	if ui.AlertDialog(c, &p.deleting, "删除作者", "删除「"+p.selected+"」的共用映射？", "取消删除", "确认删除") == 1 {
		a.deleteGlobalAuthor(p.selected)
	}
}

func (a *app) authorImportView(c *ui.Context) {
	p := &a.authorsPage
	libraryDetailHeader(c, "导入预览", p.file)
	entries := importChanges(p.preview)
	ui.Row(c).FillWidth().AlignItems(ui.Center).Gap(8).Children(func() {
		ui.Button(c, "取消导入").OnClick(func() { p.mode, p.preview = "", nil })
		ui.Spacer(c)
		primary(c, fmt.Sprintf("导入 %d 条有效更改", len(entries)), len(entries) == 0, func() { a.saveGlobalAuthors(entries) })
	})
	if len(p.preview) == 0 {
		hint(c, "文件中没有作者映射")
	} else {
		libraryGroup(c, fmt.Sprintf("作者映射 · %d 行", len(p.preview)), glyphList, func() {
			for i, row := range p.preview {
				item := ui.Column(c.Key(row.line)).FillWidth().Padding(11, 16).Gap(3)
				if i+1 < len(p.preview) {
					item.BorderWidth(0, 0, 1, 0).BorderColor(line)
				}
				item.Children(func() {
					status := fmt.Sprintf("第 %d 行 · %s", row.line, row.status)
					color := muted
					if row.problem != "" {
						status += " · " + row.problem
						color = warn
					}
					ui.Text(c, status).FontSize(12).TextColor(color)
					code(c, row.raw)
					if row.status == "更新" {
						hint(c, "原映射　"+row.previous.Name+" <"+row.previous.Email+">")
					}
				})
			}
		})
	}
}
