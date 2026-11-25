package main

import (
	_ "embed"

	"github.com/egoist/mygo/ui"
)

//go:embed assets/icon.png
var appIconPNG []byte

var appIconImage, _ = ui.DecodeImage(appIconPNG)
