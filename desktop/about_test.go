package main

import (
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"testing"

	"github.com/egoist/mygo/ui"
)

func TestReleaseCheck(t *testing.T) {
	for _, tc := range []struct {
		name           string
		status         int
		body, latest   string
		newer, wantErr bool
	}{
		{"numeric ordering", 200, `{"tag_name":"v0.10.0"}`, "v0.10.0", true, false},
		{"equal without prefix", 200, `{"tag_name":"0.2.0"}`, "v0.2.0", false, false},
		{"older", 200, `{"tag_name":"v0.1.9"}`, "v0.1.9", false, false},
		{"build metadata", 200, `{"tag_name":"v0.2.0+build.2"}`, "v0.2.0+build.2", false, false},
		{"prerelease", 200, `{"tag_name":"v1.0.0-rc.1","prerelease":true}`, "", false, false},
		{"no public release", 404, "", "", false, false},
		{"rate limited", 403, "", "", false, true},
		{"server error", 503, "", "", false, true},
		{"invalid tag", 200, `{"tag_name":"nightly"}`, "", false, true},
		{"invalid response", 200, "bad json", "", false, true},
	} {
		t.Run(tc.name, func(t *testing.T) {
			server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				if r.Method == http.MethodGet && (r.Header.Get("User-Agent") == "" || r.Header.Get("Accept") != "application/vnd.github+json") {
					t.Error("missing release request headers")
				}
				w.WriteHeader(tc.status)
				_, _ = w.Write([]byte(tc.body))
			}))
			defer server.Close()
			result, err := checkRelease(server.Client(), server.URL, server.URL+"/releases", "0.2.0")
			if (err != nil) != tc.wantErr || result.latest != tc.latest || result.newer != tc.newer {
				t.Fatalf("got %+v, err %v", result, err)
			}
		})
	}
}

func TestRateLimitedAPIFollowsOfficialReleaseLink(t *testing.T) {
	mux := http.NewServeMux()
	mux.HandleFunc("/api", func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusTooManyRequests)
	})
	mux.HandleFunc("/releases/latest", func(w http.ResponseWriter, r *http.Request) {
		http.Redirect(w, r, "/releases/tag/v0.10.0", http.StatusFound)
	})
	mux.HandleFunc("/releases/tag/v0.10.0", func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusOK)
	})
	server := httptest.NewServer(mux)
	defer server.Close()
	got, err := checkRelease(server.Client(), server.URL+"/api", server.URL+"/releases", "0.2.0")
	if err != nil || !got.newer || got.latest != "v0.10.0" {
		t.Fatal(got, err)
	}
}

func TestAboutDialogShowsVersionAndLinks(t *testing.T) {
	a := testApp(t)
	tt := ui.NewTester(a.view, 480, 440)
	if err := tt.Click("帮助"); err != nil {
		t.Fatal(err)
	}
	if err := tt.Click("关于 SVNHistoryToGit"); err != nil {
		t.Fatal(err)
	}
	for _, label := range []string{"当前版本 v" + product.Version, "作者 edisonluc", "检查更新"} {
		if !tt.HasText(label) {
			t.Fatalf("missing %s in %v", label, tt.Texts())
		}
	}
	if err := tt.Click(repositoryURL); err != nil || len(tt.OpenedURLs()) != 1 || tt.OpenedURLs()[0] != repositoryURL {
		t.Fatal(err, tt.OpenedURLs())
	}
}

func TestDesktopDataDirectoryAcrossRename(t *testing.T) {
	base := t.TempDir()
	t.Setenv("APPDATA", base)
	t.Setenv("SVN_HISTORY_TO_GIT_DATA_DIR", "")
	t.Setenv("SVN2GIT_DATA_DIR", "")
	check := func(want string) {
		t.Helper()
		client, err := defaultClient()
		if err != nil || client.dir != want || filepath.Base(client.program) != "svn-history-to-git-cli.exe" {
			t.Fatalf("got %+v, %v; want directory %s", client, err, want)
		}
	}
	current, legacy := filepath.Join(base, "SVNHistoryToGit"), filepath.Join(base, "SVN2Git")
	check(current)
	if err := os.Mkdir(legacy, 0700); err != nil {
		t.Fatal(err)
	}
	check(legacy)
	if err := os.Mkdir(current, 0700); err != nil {
		t.Fatal(err)
	}
	check(current)
	t.Setenv("SVN2GIT_DATA_DIR", filepath.Join(base, "old-custom"))
	check(filepath.Join(base, "old-custom"))
	t.Setenv("SVN_HISTORY_TO_GIT_DATA_DIR", filepath.Join(base, "custom"))
	check(filepath.Join(base, "custom"))
}
