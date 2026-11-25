package main

import "github.com/egoist/mygo/ui"

func recordRow(c *ui.Context, key, label, title, subtitle string, icon *ui.SVG, active bool, onClick func()) {
	t := c.Theme()
	row := ui.ButtonBase(c.Key(key)).Label(label).AccessSelected(active).Cursor(ui.CursorPointer).
		FillWidth().Height(43).Padding(0, 9).Gap(8).Justify(ui.Start).AlignItems(ui.Center).OnClick(onClick)
	if subtitle != "" {
		row.Tooltip(title + "\n" + subtitle)
	}
	color := t.TextMuted
	if active {
		row.Background(ui.RGBA(0, 0, 0, 0.06))
		color = t.Text
	} else if row.Hovered() {
		row.Background(t.SurfaceHover)
	}
	row.Children(func() {
		ui.Icon(c, icon).Size(16, 16).Shrink(0).TextColor(color)
		ui.Column(c).Grow(1).MinWidth(0).Gap(1).Children(func() {
			ui.Text(c, title).MinWidth(0).FillWidth().SingleLine().FontSize(13).FontWeight(600).TextColor(t.Text)
			if subtitle != "" {
				ui.Text(c, subtitle).MinWidth(0).FillWidth().SingleLine().FontSize(12).TextColor(t.TextMuted)
			}
		})
	})
}

func libraryDetailHeader(c *ui.Context, heading, caption string) {
	ui.Column(c).FillWidth().Gap(4).Children(func() {
		ui.Text(c, heading).FontSize(18).Bold().TextColor(c.Theme().Text)
		if caption != "" {
			ui.Text(c, caption).FontSize(13).TextColor(c.Theme().TextMuted).Selectable()
		}
	})
}

func libraryGroup(c *ui.Context, name string, icon *ui.SVG, build func()) {
	t := c.Theme()
	ui.Column(c).FillWidth().Gap(9).Children(func() {
		ui.Row(c).Gap(7).AlignItems(ui.Center).Children(func() {
			ui.Icon(c, icon).Size(17, 17).TextColor(t.Accent)
			ui.Text(c, name).FontSize(15).Bold().TextColor(t.Text)
		})
		ui.Column(c).FillWidth().Background(t.Surface).Border(1, t.Border).Radius(12).Clip().Children(build)
	})
}

func librarySetting(c *ui.Context, name, description string, last bool, control func()) {
	t := c.Theme()
	w, _ := c.Size()
	var row ui.Element
	stacked := w < 620
	if stacked {
		row = ui.Column(c).Gap(8)
	} else {
		row = ui.Row(c).Gap(16).AlignItems(ui.Center)
	}
	row.FillWidth().Padding(13, 16)
	if !last {
		row.BorderWidth(0, 0, 1, 0).BorderColor(t.Border)
	}
	row.Children(func() {
		labels := ui.Column(c).MinWidth(0).Gap(2)
		if stacked {
			labels.FillWidth()
		} else {
			labels.Basis(0).Grow(1)
		}
		labels.Children(func() {
			ui.Text(c, name).FontSize(13).Bold().TextColor(t.Text).Selectable()
			if description != "" {
				ui.Text(c, description).FillWidth().FontSize(12).TextColor(t.TextMuted).Selectable()
			}
		})
		if control != nil {
			controls := ui.Row(c).MinWidth(0).AlignItems(ui.Center)
			if stacked {
				controls.FillWidth()
			} else {
				controls.Width(240).Justify(ui.End)
			}
			controls.Children(control)
		}
	})
}

func libraryInput(c *ui.Context, label string, value *string, placeholder string) ui.Element {
	return ui.TextInput(c.Key(label), value).Label(label).Placeholder(placeholder).
		FillWidth().MinWidth(0).FontSize(13).Background(c.Theme().Background)
}

func libraryEmpty(c *ui.Context, icon *ui.SVG, title, message string) {
	ui.Column(c).FillWidth().Padding(14, 0).Gap(8).AlignItems(ui.Start).Children(func() {
		ui.Icon(c, icon).Size(25, 25).TextColor(c.Theme().Accent)
		libraryDetailHeader(c, title, message)
	})
}

func libraryTool(c *ui.Context, name string, icon *ui.SVG, onClick func()) {
	t := c.Theme()
	b := ui.ButtonBase(c.Key(name)).Label(name).Tooltip(name).Cursor(ui.CursorPointer).
		Size(28, 28).Radius(5).OnClick(onClick)
	if b.Hovered() {
		b.Background(t.SurfaceHover)
	}
	b.Children(func() { ui.Icon(c, icon).Size(16, 16).TextColor(t.Text) })
}
