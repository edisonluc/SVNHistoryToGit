package main

import (
	"fmt"
	"net"
	"net/url"
	"path/filepath"
	"slices"
	"strings"

	"github.com/egoist/mygo/ui"
)

const (
	svnPassword = "svn_password"
	gitToken    = "git_https_token"
	gitSSH      = "git_ssh_key"
)

type credentialServer struct {
	scheme, hostname, port string
}

func repositoryServer(raw string, svn bool) credentialServer {
	kind := gitAddress
	if svn {
		kind = svnAddress
	}
	if !validAddress(raw, kind) {
		return credentialServer{}
	}
	if !strings.Contains(raw, "://") {
		if filepath.IsAbs(raw) {
			return credentialServer{}
		}
		// Git also accepts SCP-style SSH addresses, including bracketed IPv6.
		end := strings.Index(raw, "]:")
		if end >= 0 {
			end++
		} else {
			end = strings.IndexByte(raw, ':')
		}
		if svn || end < 0 || strings.ContainsAny(raw[:end], "/\\") {
			return credentialServer{}
		}
		raw = "ssh://" + raw[:end] + "/"
	}
	u, err := url.Parse(raw)
	if err != nil || u.Hostname() == "" {
		return credentialServer{}
	}
	server := credentialServer{u.Scheme, strings.ToLower(u.Hostname()), u.Port()}
	switch u.Scheme {
	case "https", "http", "svn", "ssh", "svn+ssh":
		if server.port == "" {
			server.port = defaultPort(u.Scheme)
		}
		return server
	}
	return credentialServer{}
}

func defaultPort(scheme string) string {
	switch scheme {
	case "https":
		return "443"
	case "http":
		return "80"
	case "svn":
		return "3690"
	default:
		return "22"
	}
}

func sameCredentialServer(old, next string) bool {
	if old == next {
		return true
	}
	for _, svn := range []bool{false, true} {
		server := repositoryServer(old, svn)
		if server.hostname != "" && server == repositoryServer(next, svn) {
			return true
		}
	}
	return false
}

func (s credentialServer) kind(svn bool) string {
	if svn {
		return svnPassword
	}
	if s.scheme == "ssh" {
		return gitSSH
	}
	return gitToken
}

func (p credentialProfile) hostless() bool { return p.Host == "" || p.Host == "*" }

func (s credentialServer) matches(p credentialProfile) bool {
	if strings.Contains(p.Host, "://") {
		// The earlier desktop editor suggested a repository URL as its host prefix.
		return s.hostname != "" && s == repositoryServer(p.Host, p.Kind == svnPassword)
	}
	u, err := url.Parse("//" + p.Host)
	if err != nil || u.User != nil || u.Path != "" || !strings.EqualFold(u.Hostname(), s.hostname) {
		return false
	}
	port := u.Port()
	if port == "" {
		port = defaultPort(s.scheme)
	}
	return s.port == port
}

func (p credentialProfile) label() string {
	if p.Name == "" || p.Name == p.Username {
		return p.Username
	}
	if p.Username == "" {
		return p.Name
	}
	return p.Name + " · " + p.Username
}

func (p credentialProfile) typeName() string {
	switch p.Kind {
	case svnPassword:
		return "SVN 账号"
	case gitSSH:
		return "Git SSH 私钥"
	default:
		return "Git HTTP(S) 令牌"
	}
}

func (p credentialProfile) serverLabel() string {
	if p.hostless() {
		return "未指定服务器"
	}
	return p.Host
}

type credentialEditor struct {
	credentialProfile
	secret string
	rotate bool
}

func (e credentialEditor) request() request {
	req := request{"op": "credential", "name": e.Name, "host_pattern": e.Host, "username": e.Username, "kind": e.Kind}
	if e.ID != "" {
		req["id"] = e.ID
	}
	if e.ID == "" || e.rotate {
		req["secret"] = e.secret
	}
	if e.Kind == gitSSH {
		req["ssh_key_path"] = e.Key
	}
	return req
}

type credentialLibrary struct {
	profiles                  []credentialProfile
	loaded, editing, deleting bool
	search                    string
	edit                      credentialEditor
	generation                int
	// A wizard's "新增并使用" applies only the new record created by that action.
	useID  *string
	useSVN bool
}

func (p *credentialLibrary) selectProfile(profile credentialProfile) {
	p.edit = credentialEditor{credentialProfile: profile}
	p.generation++
	p.editing = true
	p.useID = nil
	p.deleting = false
}

func (p *credentialLibrary) find(id string) (credentialProfile, bool) {
	for _, profile := range p.profiles {
		if profile.ID == id {
			return profile, true
		}
	}
	return credentialProfile{}, false
}

func (a *app) discardCredentialEditor() {
	page := &a.credentialPage
	page.selectProfile(credentialProfile{})
	page.editing = false
}

func (a *app) refreshCredentialProfiles() {
	var profiles []credentialProfile
	a.start("读取凭据库", func() error { return a.engine.call(request{"op": "credentials"}, &profiles, nil) }, func(err error) {
		if err == nil {
			a.credentialPage.profiles = profiles
			a.credentialPage.loaded = true
		}
	})
}

func (a *app) openCredentials() {
	a.openLibrary("credentials")
	a.discardCredentialEditor()
	a.credentialPage.search = ""
	a.refreshCredentialProfiles()
}

func (a *app) credentialAddress(svn bool) string {
	if a.d.Step == 3 {
		if svn {
			return a.job.Manifest.URL
		}
		return a.job.Manifest.Remote
	}
	if svn {
		return a.d.Address
	}
	return a.d.Remote
}

func (a *app) credentialSummary(title, id string) string {
	if id != "" {
		if p, ok := a.credentialPage.find(id); ok {
			return title + " · " + p.label()
		}
	}
	return title
}

func (a *app) authView(c *ui.Context, svn bool, f *authForm, id *string, replace bool) {
	page := &a.credentialPage
	addr := a.credentialAddress(svn)
	server := repositoryServer(strings.TrimSpace(addr), svn)
	ui.Column(c).Gap(6).Padding(6).Children(func() {
		if *id != "" {
			if profile, ok := page.find(*id); ok {
				ui.Text(c, "当前凭据 · "+profile.label()).Bold()
			} else if page.loaded {
				hint(c, "原凭据已移除，请重新选择")
			} else {
				hint(c, "正在读取所选凭据")
			}
		}
		ui.Row(c).Gap(6).Wrap().Children(func() {
			ui.Button(c, "选择已存凭据").Disabled(a.busy != "" || server.hostname == "").OnClick(func() {
				f.showProfiles = !f.showProfiles
				if f.showProfiles {
					a.refreshCredentialProfiles()
				}
			})
			ui.Button(c, "新增并使用凭据").Disabled(a.busy != "" || server.hostname == "").OnClick(func() {
				a.openCredentials()
				page.selectProfile(credentialProfile{Kind: server.kind(svn), Host: net.JoinHostPort(server.hostname, server.port)})
				page.useID, page.useSVN = id, svn
			})
			ui.Button(c, "管理凭据").Disabled(a.busy != "").OnClick(func() {
				a.openCredentials()
				if p, ok := page.find(*id); ok {
					page.selectProfile(p)
				}
			})
			if *id != "" && !replace {
				ui.Button(c, "清除凭据").Disabled(a.busy != "").OnClick(func() {
					*id = ""
					a.credentialChanged(svn)
				})
			}
		})
		if f.showProfiles {
			a.profilesView(c, svn, f, id)
		}
	})
}

func credentialSuggestions(profiles []credentialProfile, server credentialServer, svn bool) [3][]credentialProfile {
	var groups [3][]credentialProfile
	if server.hostname == "" {
		return groups
	}
	for _, p := range profiles {
		if p.Kind != server.kind(svn) {
			continue
		}
		group := 2
		if p.hostless() {
			group = 1
		} else if server.matches(p) {
			group = 0
		}
		groups[group] = append(groups[group], p)
	}
	return groups
}

func (a *app) profilesView(c *ui.Context, svn bool, f *authForm, id *string) {
	addr := strings.TrimSpace(a.credentialAddress(svn))
	groups := credentialSuggestions(a.credentialPage.profiles, repositoryServer(addr, svn), svn)
	count := len(groups[0]) + len(groups[1]) + len(groups[2])
	if count == 0 {
		hint(c, "没有适用于此地址和认证方式的凭据")
		return
	}
	hint(c, "将所选凭据用于 "+addr)
	ui.Scroll(c.Key("profiles")).Height(min(210, float32(66*count+24))).Gap(4).Children(func() {
		for n, records := range groups {
			if len(records) == 0 {
				continue
			}
			hint(c, []string{"当前服务器", "未指定服务器", "其他服务器，请核对后使用"}[n])
			for _, p := range records {
				label := p.label() + " · " + p.serverLabel()
				ui.Row(c.Key(p.ID)).Gap(6).Children(func() {
					ui.Text(c, label).Grow(1)
					ui.Button(c, "使用").Label("使用 " + label).Disabled(a.busy != "").OnClick(func() {
						*id = p.ID
						f.showProfiles = false
						a.msg = "已选择凭据 · " + p.label()
						a.credentialChanged(svn)
					})
				})
			}
		}
	})
}

func (a *app) credentialsLibraryView(c *ui.Context, small bool) {
	page := &a.credentialPage
	query := strings.ToLower(strings.TrimSpace(page.search))
	matches := func(p credentialProfile) bool {
		return strings.Contains(strings.ToLower(strings.Join([]string{p.Name, p.Host, p.Username}, "\n")), query)
	}
	if !small && !page.editing && a.busy == "" {
		if idx := slices.IndexFunc(page.profiles, matches); idx >= 0 {
			page.selectProfile(page.profiles[idx])
		}
	}
	add := func() { page.selectProfile(credentialProfile{Kind: svnPassword}) }
	list := func() {
		ui.Row(c).FillWidth().Gap(2).AlignItems(ui.Center).Children(func() {
			ui.SearchField(c.Key("search"), &page.search).Placeholder("搜索凭据").Label("搜索凭据").MinWidth(0).Grow(1)
			libraryTool(c, "新增凭据", glyphPlus, add)
		})
		count := 0
		ui.Column(c).FillWidth().Gap(2).Children(func() {
			for _, p := range page.profiles {
				if !matches(p) {
					continue
				}
				count++
				icon := glyphKey
				if p.Kind == svnPassword {
					icon = glyphUser
				}
				recordRow(c, p.ID, "编辑 "+p.label(), p.label(), p.typeName()+" · "+p.serverLabel(), icon,
					page.editing && page.edit.ID == p.ID, func() { page.selectProfile(p) })
			}
		})
		if count == 0 && a.busy == "" {
			if query == "" {
				hint(c, "暂无已存凭据")
			} else {
				hint(c, "没有匹配的凭据")
			}
		}
	}
	detail := func() {
		if !page.editing {
			if a.busy != "" {
				return
			}
			if query != "" {
				libraryEmpty(c, glyphSearch, "没有匹配的凭据", "可按名称、服务器或用户名搜索。")
				return
			}
			libraryEmpty(c, glyphKey, "尚未添加凭据", "保存 SVN 账号、Git 访问令牌或 SSH 私钥，迁移时可以直接选择。")
			ui.Row(c).Children(func() { primary(c, "新增凭据", a.busy != "", add) })
			return
		}
		ui.Column(c.Key(page.generation)).FillWidth().Gap(20).Disabled(a.busy != "").DimDisabled(false).Children(func() {
			if small {
				ui.Row(c).Children(func() { ui.Button(c, "返回列表").OnClick(a.discardCredentialEditor) })
			}
			a.credentialForm(c)
		})
	}
	a.libraryPanes(c, small, page.editing, list, detail)
	if ui.AlertDialog(c, &page.deleting, "删除凭据？", fmt.Sprintf("将移除「%s」及其系统凭据。引用此账号的任务需要重新选择凭据。", page.edit.label()), "取消", "删除") == 1 {
		a.deleteLibraryCredential()
	}
}

func (a *app) credentialForm(c *ui.Context) {
	page := &a.credentialPage
	e := &page.edit
	heading := "编辑凭据"
	if e.ID == "" {
		heading = "新增凭据"
	}
	libraryDetailHeader(c, heading, "")
	libraryGroup(c, "账号信息", glyphUser, func() {
		librarySetting(c, "凭据类型", "", false, func() {
			if e.ID != "" || page.useID != nil {
				ui.Text(c, e.typeName()).FontSize(13)
				return
			}
			ui.PopoverMenuButton(c, e.typeName(), func(m *ui.Menu) {
				for _, kind := range []string{svnPassword, gitToken, gitSSH} {
					p := credentialProfile{Kind: kind}
					if m.Item(p.typeName()).Radio(e.Kind == kind).Chosen() && e.Kind != kind {
						e.Kind, e.secret, e.Key = kind, "", ""
					}
				}
			}).Label("选择凭据类型").FillWidth().Justify(ui.SpaceBetween).Background(c.Theme().Background)
		})
		librarySetting(c, "显示名称", "", false, func() {
			libraryInput(c, "显示名称", &e.Name, "例如：发布账号")
		})
		librarySetting(c, "服务器", "用于匹配仓库地址和端口", false, func() {
			libraryInput(c, "服务器", &e.Host, "主机名或主机名:端口").Disabled(page.useID != nil)
		})
		librarySetting(c, "用户名", "", true, func() { libraryInput(c, "用户名", &e.Username, "账号") })
	})
	secretLabel := "密码"
	switch e.Kind {
	case gitToken:
		secretLabel = "访问令牌"
	case gitSSH:
		secretLabel = "私钥口令（可选）"
	}
	libraryGroup(c, "身份验证", glyphKey, func() {
		if e.Kind == gitSSH {
			librarySetting(c, "SSH 私钥路径", "", false, func() {
				libraryInput(c, "SSH 私钥路径", &e.Key, "id_ed25519")
			})
		}
		if e.ID != "" {
			librarySetting(c, "更换凭据", "保留当前值，直到主动更新", !e.rotate, func() {
				if ui.Checkbox(c, &e.rotate, "更新"+secretLabel).Changed() {
					e.secret = ""
				}
			})
		}
		if e.ID == "" || e.rotate {
			librarySetting(c, secretLabel, "", true, func() {
				libraryInput(c.Key(e.Kind), secretLabel, &e.secret, "").Password()
			})
		}
	})
	canSave := strings.TrimSpace(e.Name) != "" && strings.TrimSpace(e.Username) != ""
	if e.Kind == gitSSH {
		canSave = canSave && strings.TrimSpace(e.Key) != ""
	} else if e.ID == "" || e.rotate {
		canSave = canSave && e.secret != ""
	}
	ui.Row(c).FillWidth().Gap(8).AlignItems(ui.Center).Children(func() {
		if e.ID != "" {
			ui.Button(c, "删除凭据").TextColor(warn).OnClick(func() { page.deleting = true })
		}
		ui.Spacer(c)
		label := "保存凭据"
		if page.useID != nil {
			label = "保存并使用"
		}
		primary(c, label, !canSave, a.saveLibraryCredential)
	})
}
func (a *app) saveLibraryCredential() {
	page := &a.credentialPage
	before, exists := page.find(page.edit.ID)
	req := page.edit.request()
	var saved credentialProfile
	a.start("保存凭据", func() error { return a.engine.call(req, &saved, nil) }, func(err error) {
		if err != nil {
			return
		}
		page.profiles = slices.DeleteFunc(page.profiles, func(p credentialProfile) bool { return p.ID == saved.ID })
		page.profiles = append([]credentialProfile{saved}, page.profiles...)
		if exists && before.Host != saved.Host && before.ID == a.d.SVNID && !repositoryServer(a.d.Address, true).matches(saved) {
			a.d.SVNID = ""
			a.credentialChanged(true)
		}
		if exists && before.Host != saved.Host && before.ID == a.d.GitID && !repositoryServer(a.d.Remote, false).matches(saved) {
			a.d.GitID = ""
			a.credentialChanged(false)
		}
		if page.useID != nil {
			*page.useID = saved.ID
			a.credentialChanged(page.useSVN)
			a.leaveLibrary()
		} else {
			page.selectProfile(saved)
		}
		a.msg = "已保存凭据 · " + saved.label()
	})
}

func (a *app) deleteLibraryCredential() {
	page := &a.credentialPage
	p := page.edit.credentialProfile
	a.start("删除凭据", func() error { return a.engine.call(request{"op": "delete_credential", "id": p.ID}, nil, nil) }, func(err error) {
		if err != nil {
			return
		}
		page.profiles = slices.DeleteFunc(page.profiles, func(record credentialProfile) bool { return record.ID == p.ID })
		if p.ID == a.d.SVNID {
			a.d.SVNID = ""
			a.credentialChanged(true)
		}
		if p.ID == a.d.GitID {
			a.d.GitID = ""
			a.credentialChanged(false)
		}
		a.discardCredentialEditor()
		a.msg = "已删除凭据 · " + p.label()
	})
}
