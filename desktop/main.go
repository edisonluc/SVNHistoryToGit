package main

import (
	"fmt"
	"log"
	"os"
	"path/filepath"
	"strings"

	"github.com/egoist/mygo"
	"github.com/egoist/mygo/ui"
)

type directory struct {
	URL, Name    string
	Open, Loaded bool
	Children     []*directory
}

func (n *directory) populate(r browseResult) error {
	n.Children = nil
	for _, s := range r.Dirs {
		addr, err := joinDirectory(r.URL, s)
		if err != nil {
			return err
		}
		n.Children = append(n.Children, &directory{URL: addr, Name: s})
	}
	n.Loaded = true
	return nil
}

type authForm struct {
	open, showProfiles bool
}
type credentialProfile struct {
	ID, Name, Kind, Username string
	Host                     string `json:"host_pattern"`
	Key                      string `json:"ssh_key_path"`
}
type toolStatus struct {
	Name, Status, Version, Hint, Detail string
	Available           bool
	Purpose             string `json:"required_for"`
}
type app struct {
	sidebarHidden bool
	library                                                                   string
	authorsPage                                                               authorLibrary
	credentialPage                                                            credentialLibrary
	lastAnnouncedStage                                                        string
	doctorOpen, doctorMore                                                    bool
	doctorErr, doctorNotice                                                   string
	tools                                                                     []toolStatus
	d                                                                         draft
	stored                                                                    *draft
	engine                                                                    cli
	win                                                                       *mygo.Window
	tree                                                                      *directory
	svnAuth, gitAuth                                                          authForm
	custom, advanced, branchesOpen, tagsOpen, rulesOpen, authorsOpen, logOpen bool
	branchFilter, tagFilter                                                   string
	busy, msg, problem                                                        string
	historyOpen, historyLoading, closePrompt, newPrompt, closing              bool
	historyErr, historySelection                                              string
	history                                                                   []job
	job                                                                       job
	logs                                                                      []string
	running, frozen                                                           bool
	authorBook                                                                []author
}

func (a *app) draftPath() string { return filepath.Join(a.engine.dir, "wizard-draft.json") }
func (a *app) persist() bool {
	if a.d.Step == 3 || strings.TrimSpace(a.d.Address) == "" && strings.TrimSpace(a.d.Remote) == "" {
		return true
	}
	if err := saveDraft(a.draftPath(), a.d); err != nil {
		a.problem = err.Error()
		return false
	}
	copy := a.d
	copy.scrubSavedAddresses()
	a.stored = &copy
	return true
}
func (a *app) reset() {
	a.historyOpen = false
	a.library = ""
	a.authorsPage = authorLibrary{}
	a.discardCredentialEditor()
	a.d = newDraft()
	a.tree = nil
	a.job = job{}
	a.logs = nil
	a.frozen = false
	a.custom = false
	a.advanced = false
	a.svnAuth = authForm{}
	a.gitAuth = authForm{}
	a.msg = ""
	a.problem = ""
	a.branchFilter = ""
	a.tagFilter = ""
	a.branchesOpen = true
	a.tagsOpen = true
	a.rulesOpen = false
	a.authorsOpen = false
	a.logOpen = false
}
func (a *app) navigate(step int) {
	a.d.Step = step
	a.msg = ""
	a.problem = ""
	a.persist()
}
func (a *app) start(label string, work func() error, finish func(error)) {
	if a.busy != "" {
		return
	}
	a.busy = label
	a.problem = ""
	a.msg = ""
	go func() {
		err := work()
		a.win.Update(func() {
			a.busy = ""
			if err != nil {
				a.problem = err.Error()
			}
			finish(err)
		})
	}()
}
func (a *app) loadHistory() {
	if a.historyLoading {
		return
	}
	a.historyLoading = true
	go func() {
		var jobs []job
		err := a.engine.call(request{"op": "history"}, &jobs, nil)
		a.win.Update(func() {
			a.historyLoading = false
			if err != nil {
				a.historyErr = err.Error()
				return
			}
			for i := range jobs {
				jobs[i].Title = safeDiagnostic(jobs[i].Title)
			}
			a.history = jobs
			a.historyErr = ""
		})
	}()
}
func (a *app) loadJob(id string) {
	var j job
	a.start("读取任务记录", func() error { return a.engine.call(request{"op": "get", "id": id}, &j, nil) }, func(err error) {
		if err == nil {
			if !a.persist() {
				return
			}
			a.leaveLibrary()
			a.job = j
			a.d.Step = 3
			a.historyOpen = false
			a.logOpen = false
			a.logs = j.Logs
			a.svnAuth = authForm{}
			a.gitAuth = authForm{}
		}
	})
}
func authRequest(op, addrKey, addr, credentialKey, credential string) request {
	r := request{"op": op, addrKey: strings.TrimSpace(addr)}
	if credential != "" {
		r[credentialKey] = credential
	}
	return r
}
func sameHost(old, new string) bool {
	return sameCredentialServer(old, new)
}
func (a *app) chooseProject(addr string) {
	if a.d.Project == addr {
		return
	}
	a.d.Project = addr
	a.d.clearSource()
	a.d.defaultLayout()
	a.frozen = false
	a.problem = ""
	a.msg = ""
}
func (a *app) browse(n *directory, trunkFallback string) {
	rq := authRequest("browse", "url", n.URL, "svn_credential_id", a.d.SVNID)
	var r browseResult
	usedFallback := false
	a.start("读取 SVN 目录", func() error {
		err := a.engine.call(rq, &r, nil)
		if err != nil && trunkFallback != "" {
			rq["url"] = trunkFallback
			usedFallback = true
			err = a.engine.call(rq, &r, nil)
		}
		return err
	}, func(err error) {
		if err != nil {
			n.Open = false
			return
		}
		n.URL = r.URL
		if !validAddress(r.URL, svnAddress) {
			a.problem = addressHint
			n.Open = false
			return
		}
		if usedFallback {
			n.Name = r.URL
		}
		if err := n.populate(r); err != nil {
			a.problem = err.Error()
			n.Open = false
			return
		}
		if a.tree == nil {
			n.Open = true
			a.tree = n
			a.chooseProject(r.URL)
			a.msg = fmt.Sprintf("已连接 · r%d · %d 个子目录", r.Revision, len(r.Dirs))
		}
	})
}
func (a *app) connect() {
	if !validAddress(strings.TrimSpace(a.d.Address), svnAddress) {
		a.problem = addressHint
		return
	}
	addr, pastedTrunk := parentTrunk(strings.TrimSpace(a.d.Address))
	fallback := ""
	if pastedTrunk {
		fallback = strings.TrimSpace(a.d.Address)
	}
	a.tree = nil
	a.browse(&directory{URL: addr, Name: addr}, fallback)
}
func (a *app) inspect(next int) {
	if a.d.Project == "" {
		a.problem = "请先连接并选择 SVN 项目"
		return
	}
	if !validAddress(a.d.Project, svnAddress) {
		a.problem = addressHint
		return
	}
	r := authRequest("inspect", "url", a.d.Project, "svn_credential_id", a.d.SVNID)
	r["trunk"] = strings.TrimSpace(a.d.Trunk)
	r["branches"] = strings.TrimSpace(a.d.Branches)
	r["tags"] = strings.TrimSpace(a.d.Tags)
	if a.d.autoBranchSubdir() {
		r["auto_branch_subdir"] = true
	}
	var i inspection
	a.start("读取主线并锁定修订", func() error { return a.engine.call(r, &i, nil) }, func(err error) {
		if err != nil {
			return
		}
		if !validAddress(i.URL, svnAddress) {
			a.problem = addressHint
			return
		}
		a.d.apply(i)
		a.frozen = next == 2
		a.navigate(next)
	})
}
func (a *app) checkRemote() {
	if !validAddress(strings.TrimSpace(a.d.Remote), gitAddress) {
		a.problem = addressHint
		return
	}
	if !validRefName(a.d.Main) {
		a.problem = "请输入有效的目标分支名称"
		return
	}
	if a.d.Web != "" && !webURL(a.d.Web) {
		a.problem = "仓库网页地址须为 HTTP 或 HTTPS 地址"
		return
	}
	r := authRequest("remote", "remote", a.d.Remote, "git_credential_id", a.d.GitID)
	r["target_branch"] = a.d.Main
	var reply struct {
		Existing []string `json:"existing_refs"`
	}
	a.start("检查目标分支", func() error { return a.engine.call(r, &reply, nil) }, func(err error) {
		a.d.RemoteOK = err == nil && reply.Existing != nil
		a.d.remoteRefs = reply.Existing
		if err != nil {
			return
		}
		if !a.d.RemoteOK {
			a.problem = "迁移核心未返回目标分支检查结果，请使用与桌面程序同一版本的迁移核心"
			return
		}
		if !a.frozen {
			a.inspect(2)
		} else {
			a.navigate(2)
		}
	})
}
func (a *app) credentialChanged(svn bool) {
	if a.d.Step == 3 {
		return
	}
	if svn {
		a.tree = nil
		a.frozen = false
	} else {
		a.d.RemoteOK = false
	}
	a.persist()
}
func main() {
	engine, err := defaultClient()
	if err != nil {
		log.Fatal(err)
	}
	a := &app{engine: engine}
	a.reset()
	a.stored, err = readDraft(a.draftPath())
	if err != nil {
		a.problem = "无法读取上次草稿：" + err.Error()
	}
	mygo.App.WhenReady(func() {
		a.win = mygo.NewWindow(mygo.WindowOptions{
			Title: "SVN2Git · 仓库迁移", Width: 1100, Height: 810, MinWidth: 480, MinHeight: 440,
			TitleBarStyle: mygo.TitleBarHidden, TitleBarHeight: 32, Content: ui.View(a.view),
		})
		if err := a.win.SetIcon(appIconPNG); err != nil {
			log.Printf("设置窗口图标失败: %v", err)
		}
		a.win.OnClose(func(e *mygo.CloseEvent) {
			if a.running && !a.closing {
				e.PreventDefault()
				a.closePrompt = true
				a.win.Invalidate()
				return
			}
			a.persist()
		})
		a.loadHistory()
		a.refreshCredentialProfiles()
	})
	if err := mygo.App.Run(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
