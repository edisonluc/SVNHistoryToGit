package main

import "github.com/egoist/mygo/ui"

// Toolbar and workspace glyphs share a 24-unit grid and inherit the text color.
func lineGlyph(shapes string) *ui.SVG {
	return ui.MustParseSVG([]byte(`<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round">` + shapes + `</svg>`))
}

var (
	glyphBranch  = lineGlyph(`<circle cx="6.5" cy="5" r="2.5"/><circle cx="6.5" cy="19" r="2.5"/><circle cx="18" cy="6.5" r="2.5"/><path d="M6.5 7.5v9M7 15c6 0 11-1.8 11-6"/>`)
	glyphUser    = lineGlyph(`<circle cx="10" cy="8" r="3.5"/><path d="M3.5 20v-1.5a6.5 6.5 0 0 1 13 0V20M16 4.8a3.4 3.4 0 0 1 0 6.5M18.5 14.7a5.5 5.5 0 0 1 2.5 4.6"/>`)
	glyphKey     = lineGlyph(`<circle cx="8" cy="8" r="4.7"/><path d="m11.5 11.5 8 8h2v-3h-2v-2h-2l-3.4-3.4"/><circle cx="7" cy="7" r=".8"/>`)
	glyphHistory = lineGlyph(`<path d="M3.5 10a8.5 8.5 0 1 1 .8 6M3 4.5v5.6h5.6M12 6.7v5.7l3.6 2.1"/>`)
	glyphFolder  = lineGlyph(`<path d="M3 7V5a1.5 1.5 0 0 1 1.5-1.5h5l2.4 3h7.6A1.5 1.5 0 0 1 21 8v10.5a1.5 1.5 0 0 1-1.5 1.5h-15A1.5 1.5 0 0 1 3 18.5z"/>`)
	glyphList    = lineGlyph(`<rect x="4" y="3.5" width="16" height="17" rx="2"/><path d="m7 9 1 1 2-2M12.5 9h4M7 15l1 1 2-2M12.5 15h4"/>`)
	glyphSidebar = lineGlyph(`<rect x="2.8" y="4" width="18.4" height="16" rx="2.4"/><path d="M9 4v16"/>`)
	glyphPlus    = lineGlyph(`<path d="M12 4v16M4 12h16"/>`)
	glyphBack    = lineGlyph(`<path d="m10 5-7 7 7 7M4 12h16"/>`)
	glyphCheck   = lineGlyph(`<path d="m4.5 12 5 5 10-11"/>`)
	glyphSearch  = lineGlyph(`<circle cx="10" cy="10" r="6"/><path d="m15 15 5.5 5.5"/>`)
	glyphImport  = lineGlyph(`<path d="M12 3v11m-4-4 4 4 4-4M4 15.5V20h16v-4.5"/>`)
	glyphTools   = lineGlyph(`<path d="m15.5 3.3-3.1 3.1 1 3.3 3.4 1 3.2-3.1a5.7 5.7 0 0 1-7.4 7.1l-6.5 6.1a2 2 0 0 1-2.8-2.8l6.1-6.5a5.8 5.8 0 0 1 6.1-8.2z"/>`)
	glyphTag     = lineGlyph(`<path d="M3.5 4h7L21 14.4 14.5 21 3.5 10z"/><circle cx="7.8" cy="8" r="1"/>`)
	glyphRun     = lineGlyph(`<circle cx="12" cy="12" r="9"/><path d="m10 8 6 4-6 4z"/>`)
)
