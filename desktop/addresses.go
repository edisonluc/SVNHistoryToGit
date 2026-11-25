package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"net/url"
	"path/filepath"
	"regexp"
	"strings"
	"unicode"
)

var urlInText = regexp.MustCompile(`(?i)(?:https?|svn(?:\+ssh)?|ssh|file)://\S+`)

// A legacy message may include command output with the full endpoint. Hide the
// line as a unit because a malformed URL may contain spaces inside a password.
func safeDiagnostic(text string) string {
	lines := strings.Split(text, "\n")
	for n, line := range lines {
		for _, raw := range urlInText.FindAllString(line, -1) {
			if !validAddress(raw, svnAddress) && !validAddress(raw, gitAddress) && !validAddress(raw, pageAddress) {
				lines[n] = "记录中的地址已隐藏"
				break
			}
		}
	}
	return strings.Join(lines, "\n")
}

func legacyJSON(data json.RawMessage) string {
	dec := json.NewDecoder(bytes.NewReader(data))
	dec.UseNumber()
	var root any
	if dec.Decode(&root) != nil {
		return ""
	}
	var clean func(key string, v any) any
	clean = func(key string, v any) any {
		switch val := v.(type) {
		case map[string]any:
			for k, child := range val {
				val[k] = clean(k, child)
			}
		case []any:
			for idx, child := range val {
				val[idx] = clean(key, child)
			}
		case string:
			switch key {
			case "definition", "password", "secret", "token":
				return "已隐藏"
			case "svn_url":
				if !validOptionalAddress(val, svnAddress) {
					return "地址已隐藏"
				}
			case "git_remote_url":
				if !validOptionalAddress(val, gitAddress) {
					return "地址已隐藏"
				}
			case "web_url":
				if !validOptionalAddress(val, pageAddress) {
					return "地址已隐藏"
				}
			}
			return safeDiagnostic(val)
		}
		return v
	}
	b, err := json.MarshalIndent(clean("", root), "", "  ")
	if err != nil {
		return ""
	}
	return string(b)
}

type addressType int

const (
	svnAddress addressType = iota
	gitAddress
	pageAddress
)

const addressHint = "请输入有效的仓库地址，移除查询参数与片段，并通过凭据表单填写认证信息。"

// Only complete, credential-free endpoints may cross the process or persistence boundary.
func validAddress(raw string, kind addressType) bool {
	if raw == "" || raw != strings.TrimSpace(raw) || strings.IndexFunc(raw, unicode.IsControl) >= 0 {
		return false
	}
	if kind == gitAddress && filepath.IsAbs(raw) {
		// A schemeless network URL can also look like a Windows UNC path.
		if strings.HasPrefix(raw, "//") && strings.ContainsAny(raw, "?#@") {
			return false
		}
		if strings.HasPrefix(raw, `\\`) {
			server, _, _ := strings.Cut(raw[2:], `\`)
			if strings.ContainsAny(server, ":@") {
				return false
			}
		}
		return true
	}
	if kind != pageAddress && strings.ContainsAny(raw, "?#") {
		return false
	}
	if kind == gitAddress && !strings.Contains(raw, "://") {
		end := strings.Index(raw, "]:")
		if end >= 0 {
			end++
		} else {
			end = strings.IndexByte(raw, ':')
		}
		if end <= 0 || end+1 >= len(raw) {
			return false
		}
		authority, repo := raw[:end], raw[end+1:]
		switch strings.ToLower(authority) {
		case "http", "https", "ssh", "svn", "svn+ssh", "file":
			return false
		}
		if strings.ContainsAny(authority, "/\\% \t") || strings.HasPrefix(repo, ":") {
			return false
		}
		// A password pasted without a scheme can otherwise resemble host:path.
		first, _, _ := strings.Cut(repo, "/")
		if !strings.Contains(authority, "@") && strings.Contains(first, "@") {
			return false
		}
		raw = "ssh://" + authority + "/repository"
	}
	u, err := url.Parse(raw)
	if err != nil || u.Opaque != "" || !strings.Contains(raw, "://") {
		return false
	}
	if kind == pageAddress && (sensitivePageComponent(u.RawQuery) || sensitivePageComponent(u.EscapedFragment())) {
		return false
	}
	if u.User != nil {
		_, password := u.User.Password()
		if password || (u.Scheme != "ssh" && u.Scheme != "svn+ssh" && u.Scheme != "svn") {
			return false
		}
	}
	switch u.Scheme {
	case "file":
		return kind != pageAddress && strings.HasPrefix(u.Path, "/")
	case "https", "http":
		return u.Hostname() != ""
	case "ssh":
		return kind == gitAddress && u.Hostname() != "" && !strings.HasPrefix(u.Hostname(), "-")
	case "svn", "svn+ssh":
		return kind == svnAddress && u.Hostname() != ""
	default:
		return false
	}
}
func sensitivePageComponent(raw string) bool {
	decoded, err := url.QueryUnescape(raw)
	if err != nil {
		return true
	}
	for _, param := range strings.FieldsFunc(decoded, func(r rune) bool { return strings.ContainsRune("&;#?", r) }) {
		key, _, _ := strings.Cut(param, "=")
		key = strings.ToLower(key)
		for _, marker := range []string{"token", "password", "passwd", "pwd", "auth", "sig", "key", "credential", "session", "code", "secret", "bearer", "jwt", "ticket"} {
			if strings.Contains(key, marker) {
				return true
			}
		}
	}
	return false
}
func validOptionalAddress(raw string, kind addressType) bool {
	return raw == "" || validAddress(raw, kind)
}

// Previous drafts included raw svn:externals definitions. The next inspection
// supplies their declaring directories again before the user confirms the scope.
func (d *draft) scrubSavedAddresses() bool {
	changed := false
	clearInvalid := func(s *string, k addressType) bool {
		if validOptionalAddress(*s, k) {
			return false
		}
		*s = ""
		changed = true
		return true
	}
	badSource := clearInvalid(&d.Address, svnAddress)
	if clearInvalid(&d.Project, svnAddress) {
		badSource = true
	}
	if d.Source != nil {
		// Copy before dropping cached discovery data so the live UI retains its results.
		i := *d.Source
		if !validAddress(i.URL, svnAddress) {
			badSource = true
			changed = true
		}
		if len(i.Externals) > 0 || i.ExternalWarning != "" {
			changed = true
		}
		i.Externals = nil
		i.ExternalWarning = ""
		d.Source = &i
	}
	if badSource {
		d.clearSource()
		d.Project = ""
		d.SVNID = ""
	}
	if clearInvalid(&d.Remote, gitAddress) {
		d.GitID = ""
		d.RemoteOK = false
	}
	if strings.ContainsAny(d.Web, "?#") {
		// Links containing UI state can be used live; drafts keep only simple addresses.
		d.Web = ""
		d.WebCustom = false
		changed = true
	} else if clearInvalid(&d.Web, pageAddress) {
		d.WebCustom = false
	}
	return changed
}

func (m manifest) cloneCommand() string {
	if !validAddress(m.Remote, gitAddress) {
		return ""
	}
	// PowerShell recognizes typographic single quotation marks as delimiters too.
	quote := func(s string) string {
		var b strings.Builder
		b.WriteByte('\'')
		for _, r := range s {
			b.WriteRune(r)
			if strings.ContainsRune("'‘’‚‛", r) {
				b.WriteRune(r)
			}
		}
		b.WriteByte('\'')
		return b.String()
	}
	for _, r := range m.Refs {
		if r.Kind == "trunk" && validRefName(r.Name) {
			return fmt.Sprintf("git clone --branch %s -- %s", quote(r.Name), quote(m.Remote))
		}
	}
	return ""
}
