package main

import (
	"encoding/json"
	"fmt"
	"os"

	"github.com/egoist/mygo/ui"
)

func (a *app) execute(retry bool) {
	if a.running || a.busy != "" {
		return
	}
	var req request
	if retry {
		if a.job.ID == "" || a.job.complete() || len(a.job.Legacy) > 0 {
			return
		}
		req = request{"op": "retry", "id": a.job.ID}
		if a.job.Manifest.SVNID != "" {
			req["svn_credential_id"] = a.job.Manifest.SVNID
		}
		if a.job.Manifest.GitID != "" {
			req["git_credential_id"] = a.job.Manifest.GitID
		}
	} else {
		if a.d.Source == nil || !a.d.RemoteOK {
			return
		}
		m := a.d.manifest()
		if !validAddress(m.URL, svnAddress) || !validAddress(m.Remote, gitAddress) || !validOptionalAddress(m.Web, pageAddress) {
			a.problem = addressHint
			return
		}
		_, _, ruleErr := a.d.fileRules()
		if len(refErrors(m.Refs, a.d.remoteRefs)) > 0 || authorProblem(m.Authors) != "" || ruleErr != nil {
			return
		}
		req = request{"op": "run", "manifest": m}
		if a.d.Workdir != "" {
			req["workdir"] = a.d.Workdir
		}
		a.persist()
		a.job = job{Manifest: m}
	}
	a.d.Step = 3
	a.running = true
	a.logs = nil
	a.logOpen = false
	var result job
	initialID := a.job.ID
	a.start("运行迁移", func() error {
		id := initialID
		err := a.engine.call(req, &result, func(stage, message string) {
			if stage == "created" {
				id = message
			}
			a.win.Update(func() {
				if stage == "created" {
					a.job.ID = message
				}
				a.job.Stage = stage
				a.job.Message = safeDiagnostic(message)
				if len(a.logs) >= 250 {
					a.logs = a.logs[len(a.logs)-249:]
				}
				a.logs = append(a.logs, stageTitle(stage)+"  "+a.job.Message)
			})
		})
		if err != nil && id != "" {
			var saved job
			if getErr := a.engine.call(request{"op": "get", "id": id}, &saved, nil); getErr == nil {
				result = saved
			}
		}
		return err
	}, func(err error) {
		a.running = false
		if result.ID != "" {
			a.job = result
		}
		if err != nil && a.job.ID == "" {
			a.d.Step = 2
			return
		}
		if a.job.ID != "" && !retry && a.stored != nil {
			if e := os.Remove(a.draftPath()); e == nil || os.IsNotExist(e) {
				a.stored = nil
			}
		}
		a.loadHistory()
	})
}
func (a *app) copy(c *ui.Context, label, value string) {
	ui.Button(c, label).OnClick(func() {
		c.WriteClipboardThen(value, func(err error) {
			if err != nil {
				a.problem = err.Error()
			} else {
				a.msg = "已复制"
				a.problem = ""
			}
		})
	})
}
func (a *app) resultView(c *ui.Context) {
	j := a.job
	if len(j.Legacy) > 0 {
		a.legacyView(c, j)
		return
	}
	status := stageTitle(j.Stage)
	if !a.running && !j.complete() && j.Stage != "" && j.Stage != "failed" && j.Stage != "remote_verify" {
		status = "任务待恢复"
	}
	announcement := status
	if !a.running && j.FailedStage != "" && j.Message != "" {
		announcement += "。" + safeDiagnostic(j.Message)
	}
	if key := j.ID + "\n" + announcement; key != a.lastAnnouncedStage {
		a.lastAnnouncedStage = key
		c.Announce(announcement)
	}
	panel(c).Children(func() {
		ui.Text(c, status).FontSize(20).Bold().TextColor(accent)
		if a.running {
			ui.Progress(c, -1).Label("迁移运行中")
		}
		if j.ID != "" {
			hint(c, "任务 "+j.ID)
		}
		if j.Message != "" && j.Message != j.ID {
			ui.Text(c, safeDiagnostic(j.Message)).Selectable()
		}
		if a.problem != "" || a.msg != "" || a.busy != "" && !a.running {
			a.feedback(c)
		}
		if j.Manifest.URL != "" {
			if validAddress(j.Manifest.URL, svnAddress) {
				code(c, fmt.Sprintf("SVN   %s  ·  r%d", j.Manifest.URL, j.Manifest.Revision))
			}
			if validAddress(j.Manifest.Remote, gitAddress) {
				code(c, "Git   "+j.Manifest.Remote)
			}
			for _, r := range j.Manifest.Refs {
				if r.Kind == "trunk" {
					hint(c, "目标分支  "+r.Name)
					break
				}
			}
		}
		if j.complete() {
			ui.Text(c, fmt.Sprintf("%s · %d 个不同提交", refCount(j.Manifest.Refs), j.CommitCount)).Bold()
			hint(c, "文件与历史已核验，确认清单上的远端引用已逐项回读匹配。")
			ui.Row(c).Gap(8).Wrap().Children(func() {
				if webURL(j.Manifest.Web) {
					primary(c, "打开远端仓库", false, func() {
						c.OpenURLThen(j.Manifest.Web, func(err error) {
							if err != nil {
								a.problem = err.Error()
							}
						})
					})
				}
				if command := j.Manifest.cloneCommand(); command != "" {
					a.copy(c, "复制克隆命令（PowerShell）", command)
				}
				if validAddress(j.Manifest.Remote, gitAddress) {
					a.copy(c, "复制克隆地址", j.Manifest.Remote)
				}
			})
		} else if !a.running && j.ID != "" {
			ui.Column(c).Gap(6).Children(func() {
				ui.Collapsible(c, "更新 SVN 凭据", &a.svnAuth.open, func() { a.authView(c, true, &a.svnAuth, &a.job.Manifest.SVNID, true) })
				ui.Collapsible(c, "更新 Git 凭据", &a.gitAuth.open, func() { a.authView(c, false, &a.gitAuth, &a.job.Manifest.GitID, true) })
			})
			ui.Row(c).Gap(8).Wrap().Children(func() {
				if j.FailedStage == "verify" {
					primary(c, "查看差异", false, func() { a.logOpen = true })
					ui.Button(c, j.retryLabel()).Disabled(a.busy != "").OnClick(func() { a.execute(true) })
				} else {
					primary(c, j.retryLabel(), a.busy != "", func() { a.execute(true) })
				}
				ui.Button(c, "刷新任务状态").Disabled(a.busy != "").OnClick(func() { a.loadJob(j.ID) })
				if !j.PublicationStarted && j.Stage == "failed" {
					ui.Button(c, "修改清单后新建任务").Disabled(a.busy != "").OnClick(a.revise)
				}
				if a.problem == "" && j.FailedStage != "" {
					ui.Button(c, "工具诊断").Disabled(a.busy != "").OnClick(a.diagnose)
				}
			})
		}
	})
	if len(j.Verified) > 0 {
		panel(c).Children(func() {
			if j.complete() {
				title(c, "已交付的引用")
			} else {
				title(c, "已核验的本地引用")
			}
			for _, r := range j.Verified {
				ui.Column(c.Key(r.Full)).Gap(3).Children(func() {
					ui.Text(c, r.Full).Font("Consolas").Bold().Selectable()
					hint(c, fmt.Sprintf("%d 个文件 · %d 条 SVN 修订", r.Files, r.Revisions))
					code(c, r.Commit)
					if r.Commit != r.OID {
						code(c, "对象  "+r.OID)
					}
					ui.Row(c).Children(func() { a.copy(c, "复制提交 ID", r.Commit) })
				})
			}
			if len(j.Extra) > 0 {
				title(c, "远端其他分支和标签")
				for _, r := range j.Extra {
					code(c, r)
				}
			}
		})
	}
	ui.Collapsible(c, "阶段日志与任务详情", &a.logOpen, func() {
		panel(c).Children(func() {
			if j.Workdir != "" {
				code(c, "工作目录  "+j.Workdir)
			}
			if j.FailedStage != "" {
				hint(c, "停止阶段 · "+stageTitle(j.FailedStage))
			}
			if a.running && len(a.logs) >= 250 {
				hint(c, "显示最近 250 条进度记录")
			}
			for _, msg := range a.logs {
				code(c, safeDiagnostic(msg))
			}
			if j.Message != "" {
				code(c, safeDiagnostic(j.Message))
			}
			if a.problem != "" && a.problem != j.Message {
				code(c, safeDiagnostic(a.problem))
			}
			if len(j.Manifest.Refs) > 0 {
				title(c, "确认清单")
				for _, r := range j.Manifest.Refs {
					code(c, refKind(r.Kind)+"  "+displaySourcePath(j.Manifest.URL, r.Path)+"  →  "+r.Name)
				}
			}
		})
	})
}
func (a *app) legacyView(c *ui.Context, j job) {
	var old struct {
		Name, Stage, Status, Error string
		Config, Report             json.RawMessage
	}
	err := json.Unmarshal(j.Legacy, &old)
	panel(c).Children(func() {
		title(c, "旧版任务 · "+old.Name)
		if err != nil {
			ui.Text(c, err.Error()).TextColor(warn)
		}
		hint(c, "此记录展示旧引擎保存的配置与报告。")
		code(c, old.Status+"  "+old.Stage)
		if old.Error != "" {
			ui.Text(c, safeDiagnostic(old.Error)).TextColor(warn).Selectable()
		}
		for _, v := range []struct {
			title string
			data  json.RawMessage
		}{{"迁移报告", old.Report}, {"任务配置", old.Config}} {
			title(c, v.title)
			if len(v.data) == 0 || string(v.data) == "null" {
				hint(c, "此任务未保存"+v.title)
				continue
			}
			code(c, legacyJSON(v.data))
		}
	})
}
