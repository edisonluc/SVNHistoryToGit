package main

import (
	_ "embed"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strings"
	"time"

	"github.com/egoist/mygo/ui"
	"golang.org/x/mod/semver"
)

const repositoryPath = "edisonluc/SVNHistoryToGit"
const repositoryURL = "https://github.com/" + repositoryPath
const latestReleaseAPI = "https://api.github.com/repos/" + repositoryPath + "/releases/latest"

//go:embed mygo.json
var packageManifest []byte

var product struct{ Name, Version, Publisher string }

func init() {
	if err := json.Unmarshal(packageManifest, &product); err != nil {
		panic(err)
	}
	if product.Name == "" || product.Publisher == "" || !semver.IsValid("v"+product.Version) {
		panic("mygo.json 缺少有效的名称、作者或版本号")
	}
}

type releaseResult struct {
	latest string
	newer  bool
}

func checkRelease(client *http.Client, endpoint, releasesURL, current string) (releaseResult, error) {
	req, err := http.NewRequest(http.MethodGet, endpoint, nil)
	if err != nil {
		return releaseResult{}, err
	}
	req.Header.Set("Accept", "application/vnd.github+json")
	req.Header.Set("User-Agent", product.Name+"/"+current)
	resp, err := client.Do(req)
	if err != nil {
		return releaseResult{}, fmt.Errorf("无法连接 GitHub，请检查网络后重试")
	}
	defer resp.Body.Close()
	var tag string
	switch resp.StatusCode {
	case http.StatusNotFound:
		return releaseResult{}, nil
	case http.StatusForbidden, http.StatusTooManyRequests:
		// GitHub's public /releases/latest link redirects to the stable release
		// even when anonymous API access is rate limited.
		tag, err = releasePageTag(client, releasesURL)
		if err != nil {
			return releaseResult{}, fmt.Errorf("GitHub API 返回 HTTP %d。%w", resp.StatusCode, err)
		}
		if tag == "" {
			return releaseResult{}, nil
		}
	case http.StatusOK:
		var release struct {
			Tag        string `json:"tag_name"`
			Draft      bool
			Prerelease bool
		}
		if err := json.NewDecoder(io.LimitReader(resp.Body, 1<<20)).Decode(&release); err != nil {
			return releaseResult{}, fmt.Errorf("无法读取 GitHub 的发布信息")
		}
		if release.Draft || release.Prerelease {
			return releaseResult{}, nil
		}
		tag = release.Tag
	default:
		return releaseResult{}, fmt.Errorf("GitHub 返回 HTTP %d，请稍后重试", resp.StatusCode)
	}
	tag = "v" + strings.TrimPrefix(tag, "v")
	if !semver.IsValid(tag) {
		return releaseResult{}, fmt.Errorf("发布标签不是有效的版本号")
	}
	if semver.Prerelease(tag) != "" {
		return releaseResult{}, nil
	}
	return releaseResult{latest: tag, newer: semver.Compare(tag, "v"+current) > 0}, nil
}

func releasePageTag(client *http.Client, releasesURL string) (string, error) {
	resp, err := client.Head(releasesURL + "/latest")
	if err != nil {
		return "", fmt.Errorf("无法连接发布页，请稍后重试")
	}
	defer resp.Body.Close()
	u := resp.Request.URL
	page := u.Scheme + "://" + u.Host + u.Path
	if resp.StatusCode == http.StatusNotFound || (resp.StatusCode == http.StatusOK && page == releasesURL) {
		return "", nil
	}
	if resp.StatusCode == http.StatusOK {
		if tag, ok := strings.CutPrefix(page, releasesURL+"/tag/"); ok && tag != "" {
			return tag, nil
		}
	}
	return "", fmt.Errorf("无法从发布页确认版本，请稍后重试")
}

type aboutDialog struct {
	open, checking bool
	status         string
	newer          bool
}

func (a *app) checkForUpdates() {
	if a.about.checking {
		return
	}
	a.about.checking, a.about.newer = true, false
	a.about.status = "正在检查 GitHub 上的最新版本…"
	go func() {
		result, err := checkRelease(&http.Client{Timeout: 10 * time.Second}, latestReleaseAPI, repositoryURL+"/releases", product.Version)
		a.win.Update(func() {
			a.about.checking = false
			a.about.newer = result.newer
			switch {
			case err != nil:
				a.about.status = err.Error()
			case result.latest == "":
				a.about.status = "未找到公开的正式发布版本。"
			case result.newer:
				a.about.status = "发现新版本 " + result.latest
			default:
				a.about.status = "没有可用更新。GitHub 正式版为 " + result.latest + "。"
			}
		})
	}()
}

func (a *app) aboutView(c *ui.Context) {
	if !a.about.open {
		return
	}
	ui.DialogBase(c, &a.about.open, func(back, panel ui.Element) {
		w, _ := c.Size()
		back.Background(ui.RGBA(0, 0, 0, 0.4))
		panel.Label("关于 "+product.Name).Width(min(440, w-32)).Padding(20).Gap(12).
			Background(paper).Border(1, line).Radius(14)
		panel.Children(func() {
			ui.Text(c, product.Name).FontSize(20).Bold()
			ui.Text(c, "当前版本 v"+product.Version)
			ui.Text(c, "作者 "+product.Publisher)
			ui.Link(c, repositoryURL, repositoryURL)
			if a.about.status != "" {
				ui.Text(c, a.about.status).Role(ui.RoleStatus).FontSize(13)
			}
			if a.about.newer {
				ui.Link(c, "查看新版发布说明与下载", repositoryURL+"/releases/latest")
			}
			ui.Row(c).FillWidth().AlignItems(ui.Center).Gap(8).Children(func() {
				ui.Button(c, "检查更新").Disabled(a.about.checking).OnClick(a.checkForUpdates)
				ui.Spacer(c)
				primary(c, "关闭", false, func() { a.about.open = false })
			})
		})
	})
}
