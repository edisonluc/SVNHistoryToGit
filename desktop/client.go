package main

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
)

type progressFunc func(stage, message string)
type event struct {
	Type    string          `json:"type"`
	Stage   string          `json:"stage"`
	Message string          `json:"message"`
	Data    json.RawMessage `json:"data"`
}
type request map[string]any
type cli struct{ program, dir string }

func defaultClient() (cli, error) {
	exe, err := os.Executable()
	if err != nil {
		return cli{}, err
	}
	dir := os.Getenv("SVN_HISTORY_TO_GIT_DATA_DIR")
	if dir == "" {
		dir = os.Getenv("SVN2GIT_DATA_DIR")
	}
	if dir == "" {
		if appData := os.Getenv("APPDATA"); appData != "" {
			dir = filepath.Join(appData, product.Name)
			if _, err := os.Stat(dir); errors.Is(err, os.ErrNotExist) {
				legacy := filepath.Join(appData, "SVN2Git")
				if info, err := os.Stat(legacy); err == nil && info.IsDir() {
					dir = legacy
				}
			}
		} else {
			dir = filepath.Join(filepath.Dir(exe), ".data")
		}
	}
	return cli{filepath.Join(filepath.Dir(exe), "svn-history-to-git-cli.exe"), dir}, nil
}
func readEvents(reader io.Reader, onProgress progressFunc) (json.RawMessage, error) {
	dec := json.NewDecoder(reader)
	var result json.RawMessage
	var terminalErr error
	for {
		var e event
		err := dec.Decode(&e)
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return nil, fmt.Errorf("读取引擎响应失败：%w", err)
		}
		if result != nil || terminalErr != nil {
			return nil, errors.New("引擎在结束后返回了额外事件")
		}
		switch e.Type {
		case "progress":
			if onProgress != nil {
				onProgress(e.Stage, e.Message)
			}
		case "result":
			if e.Data == nil || string(e.Data) == "null" {
				return nil, errors.New("引擎结果缺少数据")
			}
			result = e.Data
		case "error":
			if e.Message == "" {
				e.Message = "引擎报告了未知错误"
			}
			terminalErr = errors.New(e.Message)
		default:
			return nil, fmt.Errorf("无法识别的引擎事件 %q", e.Type)
		}
	}
	if terminalErr != nil {
		return nil, terminalErr
	}
	if result == nil {
		return nil, errors.New("引擎退出前未返回结果")
	}
	return result, nil
}
func (c cli) call(req request, dest any, progress progressFunc) error {
	b, err := json.Marshal(req)
	if err != nil {
		return err
	}
	cmd := exec.Command(c.program, "--data-dir", c.dir, "desktop")
	hideConsole(cmd)
	cmd.Stdin = bytes.NewReader(b)
	var stderr bytes.Buffer
	cmd.Stderr = &stderr
	out, err := cmd.StdoutPipe()
	if err != nil {
		return err
	}
	if err = cmd.Start(); err != nil {
		return fmt.Errorf("无法启动迁移引擎 %s：%w", c.program, err)
	}
	result, streamErr := readEvents(out, progress)
	if streamErr != nil {
		_, _ = io.Copy(io.Discard, out)
	}
	exitErr := cmd.Wait()
	if streamErr != nil {
		// Structured failures carry the specific engine message.
		if exitErr != nil && strings.TrimSpace(stderr.String()) != "" {
			return fmt.Errorf("%w\n%s", streamErr, strings.TrimSpace(stderr.String()))
		}
		return streamErr
	}
	if exitErr != nil {
		return fmt.Errorf("迁移引擎退出：%w %s", exitErr, strings.TrimSpace(stderr.String()))
	}
	if dest == nil {
		return nil
	}
	return json.Unmarshal(result, dest)
}

type browseResult struct {
	URL      string   `json:"url"`
	UUID     string   `json:"uuid"`
	Revision int64    `json:"revision"`
	Dirs     []string `json:"dirs"`
}
type verifiedRef struct {
	Full      string `json:"full_ref"`
	OID       string `json:"oid"`
	Commit    string `json:"commit_oid"`
	Files     int    `json:"files"`
	Revisions int    `json:"revisions"`
}
type job struct {
	AuthorOverrides    []author        `json:"author_overrides"`
	Logs               []string        `json:"logs"`
	ID                 string          `json:"id"`
	Title              string          `json:"title"`
	UpdatedAt          string          `json:"updated_at"`
	Manifest           manifest        `json:"manifest"`
	Stage              string          `json:"stage"`
	Message            string          `json:"message"`
	FailedStage        string          `json:"failed_stage"`
	PublicationStarted bool            `json:"publication_started"`
	Verified           []verifiedRef   `json:"verified"`
	Extra              []string        `json:"extra_refs"`
	CommitCount        int             `json:"commit_count"`
	Workdir            string          `json:"workdir"`
	Legacy             json.RawMessage `json:"legacy"`
}

func (j job) complete() bool { return j.Stage == "completed" && len(j.Legacy) == 0 }
func stageTitle(stage string) string {
	switch stage {
	case "created":
		return "已创建任务"
	case "convert":
		return "正在转换 SVN 历史"
	case "verify":
		return "正在核验历史与文件"
	case "push":
		return "正在发布 Git 引用"
	case "remote_verify":
		return "已推送，等待核对"
	case "completed":
		return "迁移完成"
	case "failed":
		return "迁移已暂停"
	case "":
		return "准备开始"
	default:
		return stage
	}
}
func (j job) retryLabel() string {
	stage := j.FailedStage
	if stage == "" {
		stage = j.Stage
	}
	switch stage {
	case "remote_verify":
		return "重新核对远端"
	case "push":
		return "重试推送"
	case "verify":
		return "重新转换并校验"
	default:
		return "重新转换"
	}
}
