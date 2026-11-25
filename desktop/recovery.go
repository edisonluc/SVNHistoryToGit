package main

import (
	"fmt"
	"path"
	"strconv"
	"strings"
)

// Editing a failed publication creates a new draft. The original task keeps its immutable manifest.
func (a *app) revise() {
	m := a.job.Manifest
	if a.running || a.job.PublicationStarted || a.job.complete() || len(m.Refs) == 0 {
		return
	}
	overrides := a.job.AuthorOverrides
	if overrides == nil {
		overrides = m.Authors
	}
	a.reset()
	d := &a.d
	d.Address = m.URL
	d.Project = m.URL
	d.Remote = m.Remote
	d.Web = m.Web
	d.SVNID = m.SVNID
	d.GitID = m.GitID
	d.Authors = append([]author(nil), overrides...)
	d.ExcludeEnabled = len(m.ExcludeGlobs) > 0
	d.Exclusions = strings.Join(m.ExcludeGlobs, "\n")
	if m.LFSThreshold != nil {
		d.LFSEnabled = true
		d.LFSSize = strconv.FormatUint(*m.LFSThreshold, 10)
	}
	i := inspection{URL: m.URL, UUID: m.UUID, Revision: m.Revision}
	for _, r := range m.Refs {
		switch r.Kind {
		case "trunk":
			d.Trunk = r.Path
			i.Trunk = r.Path
			d.Main = r.Name
		case "branch":
			d.Branches = path.Dir(r.Path)
			i.Branches = append(i.Branches, r.Path)
		case "tag":
			d.Tags = path.Dir(r.Path)
			i.Tags = append(i.Tags, r.Path)
		}
	}
	d.WebCustom = d.Web != "" && d.Web != suggestedWeb(m.Remote, d.Main)
	d.apply(i)
	names := map[string]string{}
	for _, r := range m.Refs {
		names[r.Kind+"\n"+r.Path] = r.Name
	}
	for n := range d.Options {
		o := &d.Options[n]
		o.Name = names[o.Kind+"\n"+o.Path]
		o.Selected = true
	}
	d.Step = 1
	a.custom = d.Trunk != "trunk" || d.Branches != "branches" || d.Tags != "tags"
	a.msg = fmt.Sprintf("已复制 r%d 的清单。继续后将重新检查远端并读取当前 SVN 修订。", m.Revision)
	a.persist()
}
