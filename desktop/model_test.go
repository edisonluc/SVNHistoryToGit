package main

import (
	"reflect"
	"strings"
	"testing"
)

func TestSelectionAndRefresh(t *testing.T) {
	d := newDraft()
	i := inspection{URL: "https://svn.test/ERP", UUID: "uuid", Revision: 4, Trunk: "trunk", Branches: []string{"branches/fix"}, Tags: []string{"tags/v1"}}
	d.apply(i)
	refs := func() []scopeRef { return d.manifest().Refs }
	if len(refs()) != 1 || refs()[0].Name != "main" {
		t.Fatal(refs())
	}
	d.Options[0].Selected, d.Options[0].Name = true, "stable"
	d.Options[1].Selected = true
	if want := []scopeRef{{"trunk", "trunk", "main"}, {"branches/fix", "branch", "stable"}, {"tags/v1", "tag", "v1"}}; !reflect.DeepEqual(refs(), want) {
		t.Fatal(refs())
	}
	d.Options[1].Selected = false
	i.Revision++
	d.apply(i)
	if len(refs()) != 2 || refs()[1].Name != "stable" {
		t.Fatal(refs())
	}
	i.URL += "2"
	d.apply(i)
	if len(refs()) != 1 {
		t.Fatal(refs())
	}
}

func TestEventStreamRequiresTerminalResult(t *testing.T) {
	for _, s := range []string{"", `{"type":"progress","stage":"verify"}`, `{"type":"error","message":"push denied"}`, `{"type":"result"}`, `{"type":"result","data":{}} {"type":"result","data":{}}`} {
		if _, err := readEvents(strings.NewReader(s), nil); err == nil {
			t.Fatalf("accepted %q", s)
		}
	}
	var progress string
	r, err := readEvents(strings.NewReader("{\"type\":\"progress\",\"stage\":\"push\"}\n{\"type\":\"result\",\"data\":{\"id\":\"abc\"}}"), func(stage, message string) { progress = stage })
	if err != nil || string(r) != `{"id":"abc"}` || progress != "push" {
		t.Fatal(string(r), err, progress)
	}
}
