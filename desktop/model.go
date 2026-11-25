package main

import (
	"encoding/json"
	"fmt"
	"net/url"
	"os"
	"path"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"unicode"
)

type scopeRef struct {
	Path string `json:"path"`
	Kind string `json:"kind"`
	Name string `json:"name"`
}

func (r scopeRef) fullRef() string {
	if r.Kind == "tag" {
		return "refs/tags/" + r.Name
	}
	return "refs/heads/" + r.Name
}

type author struct {
	SVNUser string `json:"svn_user"`
	Name    string `json:"git_name"`
	Email   string `json:"git_email"`
}
type manifest struct {
	URL          string     `json:"svn_url"`
	UUID         string     `json:"uuid"`
	Revision     int64      `json:"revision"`
	Remote       string     `json:"remote"`
	Web          string     `json:"web_url"`
	Refs         []scopeRef `json:"refs"`
	SVNID        string     `json:"svn_credential_id,omitempty"`
	GitID        string     `json:"git_credential_id,omitempty"`
	Authors      []author   `json:"authors,omitempty"`
	ExcludeGlobs []string   `json:"exclude_globs,omitempty"`
	LFSThreshold *uint64    `json:"lfs_threshold_mb,omitempty"`
}
type inspection struct {
	URL             string   `json:"url"`
	UUID            string   `json:"uuid"`
	Revision        int64    `json:"revision"`
	Trunk           string   `json:"trunk"`
	Branches        []string `json:"branches"`
	Tags            []string `json:"tags"`
	Externals       []string `json:"externals"`
	ExternalWarning string   `json:"external_warning,omitempty"`
}
type option struct {
	scopeRef
	Selected bool
}
type draft struct {
	Step                                    int
	Address, Project, Trunk, Branches, Tags string
	LayoutEdited                            bool `json:",omitempty"`
	Remote, Main, Web, Workdir              string
	WebCustom                               bool
	SVNID, GitID                            string
	Source                                  *inspection
	Options                                 []option
	Authors                                 []author
	RemoteOK                                bool
	remoteRefs                              []string // Sorted names from the most recent remote check.
	ExcludeEnabled, LFSEnabled              bool
	Exclusions, LFSSize                     string
}

func newDraft() draft {
	return draft{Trunk: "trunk", Branches: "branches", Tags: "tags", Main: "main", LFSSize: "50"}
}
func (d draft) fileRules() ([]string, *uint64, error) {
	var globs []string
	if d.ExcludeEnabled {
		for _, s := range strings.Split(d.Exclusions, "\n") {
			if s = strings.TrimSpace(s); s != "" {
				globs = append(globs, s)
			}
		}
		if len(globs) == 0 {
			return nil, nil, fmt.Errorf("请填写至少一个排除规则")
		}
	}
	if !d.LFSEnabled {
		return globs, nil, nil
	}
	size, err := strconv.ParseUint(strings.TrimSpace(d.LFSSize), 10, 64)
	if err != nil || size == 0 || size > ^uint64(0)/(1024*1024) {
		return globs, nil, fmt.Errorf("LFS 阈值须为正整数 MiB，且字节数须可表示")
	}
	return globs, &size, nil
}
func (d *draft) clearSource() {
	d.Source, d.Options = nil, nil
	d.Step = 0
}
func (d *draft) apply(i inspection) {
	if d.autoBranchSubdir() && i.Trunk == "" {
		// Keep the resolved scope when checking a new revision or restoring a draft.
		d.Trunk, d.Branches, d.Tags = "", "", ""
	}
	prior := make(map[string]option)
	if d.Source != nil && d.Source.URL == i.URL && d.Source.UUID == i.UUID && d.Source.Trunk == i.Trunk {
		for _, o := range d.Options {
			prior[o.Kind+"\n"+o.Path] = o
		}
	}
	d.Source = &i
	d.Options = nil
	for _, group := range []struct {
		kind  string
		paths []string
	}{{"branch", i.Branches}, {"tag", i.Tags}} {
		for _, p := range group.paths {
			o, ok := prior[group.kind+"\n"+p]
			if !ok {
				o = option{scopeRef: scopeRef{p, group.kind, path.Base(p)}}
			}
			d.Options = append(d.Options, o)
		}
	}
}
func (d *draft) manifest() manifest {
	if d.Source == nil {
		return manifest{}
	}
	i := d.Source
	m := manifest{URL: i.URL, UUID: i.UUID, Revision: i.Revision, Remote: strings.TrimSpace(d.Remote), Web: strings.TrimSpace(d.Web), SVNID: d.SVNID, GitID: d.GitID, Authors: append([]author(nil), d.Authors...), Refs: []scopeRef{{i.Trunk, "trunk", d.Main}}}
	m.ExcludeGlobs, m.LFSThreshold, _ = d.fileRules()
	for _, o := range d.Options {
		if o.Selected {
			m.Refs = append(m.Refs, o.scopeRef)
		}
	}
	return m
}
func validRefName(name string) bool {
	if name == "" || name == "@" || name == "HEAD" || strings.HasPrefix(name, "-") || strings.HasSuffix(name, ".") || strings.ContainsAny(name, " ~^:?*[\\") || strings.Contains(name, "..") || strings.Contains(name, "@{") || strings.IndexFunc(name, unicode.IsControl) >= 0 {
		return false
	}
	for _, seg := range strings.Split(name, "/") {
		if seg == "" || strings.HasPrefix(seg, ".") || strings.HasSuffix(seg, ".lock") {
			return false
		}
	}
	return true
}
func remoteConflict(name string, existing []string) string {
	for parent := name; parent != "."; parent = path.Dir(parent) {
		if _, found := slices.BinarySearch(existing, parent); found {
			return parent
		}
	}
	prefix := name + "/"
	i, _ := slices.BinarySearch(existing, prefix)
	if i < len(existing) && strings.HasPrefix(existing[i], prefix) {
		return existing[i]
	}
	return ""
}

func refErrors(refs []scopeRef, existing []string) map[int]string {
	problems := map[int]string{}
	seen := map[string]int{}
	paths := map[string]int{}
	for i, r := range refs {
		if j, ok := paths[r.Path]; ok {
			problems[i] = "来源路径重复，请移除一项"
			problems[j] = problems[i]
		}
		paths[r.Path] = i
		if !validRefName(r.Name) {
			problems[i] = "请输入有效的 Git 引用名"
			continue
		}
		key := r.fullRef()
		if conflict := remoteConflict(key, existing); conflict != "" {
			problems[i] = "远端已有 " + conflict + "，请更换目标名称"
		}
		if j, ok := seen[key]; ok {
			problems[i] = "目标名称重复"
			problems[j] = "目标名称重复"
		}
		seen[key] = i
	}
	for key, i := range seen {
		for parent := path.Dir(key); parent != "."; parent = path.Dir(parent) {
			if j, ok := seen[parent]; ok {
				problems[i] = "目标名称与父路径冲突"
				problems[j] = "目标名称与子路径冲突"
			}
		}
	}
	return problems
}
func webURL(raw string) bool {
	return validAddress(raw, pageAddress)
}
func suggestedWeb(remote, branch string) string {
	raw := strings.TrimSpace(remote)
	if !validAddress(raw, gitAddress) {
		return ""
	}
	if !strings.Contains(raw, "://") {
		at := strings.Index(raw, "@")
		if at < 0 {
			return ""
		}
		host, p, ok := strings.Cut(raw[at+1:], ":")
		if !ok {
			return ""
		}
		raw = "ssh://" + host + "/" + p
	}
	u, err := url.Parse(raw)
	if err != nil || u.Hostname() == "" {
		return ""
	}
	route := ""
	switch strings.ToLower(u.Hostname()) {
	case "github.com", "gitee.com":
		route = "/tree/"
	case "gitlab.com":
		route = "/-/tree/"
	case "bitbucket.org":
		route = "/src/"
	default:
		return ""
	}
	p := strings.TrimSuffix(strings.TrimRight(u.EscapedPath(), "/"), ".git")
	if p == "" || branch == "" {
		return ""
	}
	return "https://" + u.Host + p + route + url.PathEscape(branch)
}
func joinDirectory(base, name string) (string, error) {
	u, err := url.Parse(strings.TrimSpace(base))
	if err != nil {
		return "", err
	}
	return u.JoinPath(url.PathEscape(name)).String(), nil
}

// A selected trunk or an immediate child of branches is itself the mainline.
func mainlineDirectory(addr string) string {
	u, err := url.Parse(addr)
	if err != nil {
		return ""
	}
	p := strings.TrimRight(u.Path, "/")
	name := path.Base(p)
	if path.Base(path.Dir(p)) == "branches" {
		return "branches/" + name
	}
	if name == "trunk" {
		return name
	}
	return ""
}

// A project below a branch may have its own trunk, or store its code directly.
func nestedBranchPath(addr string) string {
	u, err := url.Parse(addr)
	if err != nil {
		return ""
	}
	p := strings.TrimRight(u.Path, "/")
	const marker = "/branches/"
	at := strings.LastIndex(p, marker)
	if at < 0 || !strings.Contains(p[at+len(marker):], "/") {
		return ""
	}
	switch path.Base(p) {
	case "branches", "tags", "trunk":
		return ""
	}
	return p[at+1:]
}

func (d *draft) autoBranchSubdir() bool {
	return d.Source == nil && !d.LayoutEdited && d.Trunk == "trunk" &&
		d.Branches == "branches" && d.Tags == "tags" && nestedBranchPath(d.Project) != ""
}

func (d *draft) defaultLayout() {
	d.LayoutEdited = false
	if mainlineDirectory(d.Project) != "" {
		d.Trunk, d.Branches, d.Tags = "", "", ""
		return
	}
	d.Trunk, d.Branches, d.Tags = "trunk", "branches", "tags"
}

func parentTrunk(addr string) (string, bool) {
	u, err := url.Parse(strings.TrimSpace(addr))
	if err != nil {
		return addr, false
	}
	p := strings.TrimRight(u.Path, "/")
	if path.Base(p) != "trunk" || path.Base(path.Dir(p)) == "branches" {
		return addr, false
	}
	u.Path = path.Dir(p)
	u.RawPath = ""
	// Stepping up from a literal trunk inside a branch would select the whole branch.
	if mainlineDirectory(u.String()) != "" {
		return addr, false
	}
	return u.String(), true
}
func saveDraft(filename string, d draft) error {
	if !validOptionalAddress(d.Address, svnAddress) || !validOptionalAddress(d.Project, svnAddress) || !validOptionalAddress(d.Remote, gitAddress) || !validOptionalAddress(d.Web, pageAddress) || (d.Source != nil && !validAddress(d.Source.URL, svnAddress)) {
		return fmt.Errorf("%s", addressHint)
	}
	d.scrubSavedAddresses()
	if err := os.MkdirAll(filepath.Dir(filename), 0700); err != nil {
		return err
	}
	b, err := json.Marshal(d)
	if err != nil {
		return err
	}
	f, err := os.CreateTemp(filepath.Dir(filename), "draft-*.tmp")
	if err != nil {
		return err
	}
	tmp := f.Name()
	defer os.Remove(tmp)
	if _, err = f.Write(b); err == nil {
		err = f.Sync()
	}
	closeErr := f.Close()
	if err != nil {
		return err
	}
	if closeErr != nil {
		return closeErr
	}
	return os.Rename(tmp, filename)
}
func readDraft(filename string) (*draft, error) {
	b, err := os.ReadFile(filename)
	if os.IsNotExist(err) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	var d draft
	if err = json.Unmarshal(b, &d); err != nil {
		return nil, err
	}
	if d.scrubSavedAddresses() {
		if err := saveDraft(filename, d); err != nil {
			return nil, fmt.Errorf("清理旧草稿失败：%w", err)
		}
	}
	if d.Step < 0 || d.Step > 2 {
		d.Step = 0
	}
	if d.Source == nil {
		d.Step = 0
		if !d.LayoutEdited && d.Trunk == "trunk" && d.Branches == "branches" && d.Tags == "tags" {
			d.defaultLayout()
		}
	}
	d.RemoteOK = false
	if d.Step == 2 {
		d.Step = 1
	}
	return &d, nil
}
