# src/ui — the widgets

## Overview

Everything plv draws, as ratatui widgets. The layer renders and computes
layout; it owns no state that outlives a frame and does no I/O. What is on
screen comes in from the app layer ([`../README.md`](../README.md)): a page
`DataFrame` from the `Store`, the cursor, the selection, the search, the sort
keys, the hand-set widths and pins. Where the app layer has to agree with the
renderer about layout — which column is the last one to fit, how wide a column
is drawn, whether a pin leaves room to scroll, how a cell's value wraps in its
window — the answer is a function in this layer that both call, rather than
arithmetic repeated on the other side.

## Interface

- `mod.rs` — `SelectionMode { Row, Column, Cell }` and `cycle()` (the `Tab`
  mode); re-exports the widgets below. `cell`, `json`, `markup`, `markdown`,
  `syntax` and `theme` are public modules; the rest are private behind the
  re-exports.
- `table.rs` — `DataTable` (the table widget); `Widths` (hand-set widths,
  keyed by source column) and `Pinned` (pinned columns, as display positions);
  `col_offset_showing` (the horizontal offset that brings a column into view),
  `drawn_width` (what a column is drawn at now), `natural_width` (what a page's
  values need), `pin_fits` (whether a scrolling region would remain),
  `MIN_COLUMN`.
- `cell.rs` — `CellWindow` (the `K` window); `Content` (a value parsed once
  into a `Format`: `Text`, `Json`, `Markup(Flavour)` or `Markdown`, with
  `switchable()` for `r`); `window` and its `Layout { rect, lines, origins }`;
  `wrap`/`wrapped` (the word-aware wrap); `Origin`, `Hit`, `find`, `matches`,
  `match_lines` (the window's search); `title`, `text_rows`, `max_scroll`.
- `json.rs` — `reindent(&str) -> Option<Vec<Vec<Piece>>>`; claims objects and
  arrays only.
- `markup.rs` — `reindent(&str) -> Option<(Vec<Vec<Piece>>, Flavour)>`,
  `Flavour { Xml, Html }`.
- `markdown.rs` — `format(&str) -> Option<Vec<Vec<Piece>>>`.
- `syntax.rs` — `Kind`, `Piece`, and `lines`, the one place pieces become
  styled `Line`s.
- `panel.rs` — `Panel` (completion candidates) and `height`; `CellView` (the
  `zk` strip), `cell_height` and its hard `wrap`.
- `browser.rs` — `Browser` and `BrowserState`, the DuckLake catalog lists.
- `help.rs` — `Help` and `Section`, the `?` overlay.
- `statusbar.rs` — `StatusBar`.
- `prompt.rs` — `Prompt`, the `:` and `/` line with its caret.
- `theme.rs` — `Theme` (every colour, in one place) and
  `Theme::catppuccin_mocha()`.

## Invariants

- The formatters (`json.rs`, `markup.rs`, `markdown.rs`) emit only slices of
  the value plus line breaks and spaces, and hold no ratatui and no theme; a
  format is claimed only when its parse (or, for markdown, its judgement)
  covers the whole value. The markdown pieces reproduce the value character for
  character.
- A cell window's search matches the value, not the wrapped screen: the count
  does not depend on the window's width.
- Layout questions the app layer needs answered (`col_offset_showing`,
  `drawn_width`, `pin_fits`, `cell::window`) are answered here and nowhere else.
- An empty or absent field is drawn as `·`, never as the word `null`.
- Colours come from `Theme`; no widget names a colour of its own.

## Side Effects

- None beyond drawing into the ratatui `Buffer`: no files, no threads, no clock.
  Animation is driven by tick counters passed in (`spinner_tick`, `sort_tick`).
- The one write-back: `DataTable` stores the last fully visible column into a
  `Cell<usize>` the app passes in (`last_vis_col_out`), so the app can scroll
  when the cursor reaches the edge.

## Design

### The table (`table.rs`)

`DataTable`: computes per-column display widths from the current view, determines which columns fit given the terminal width (starting from `col_offset`), then renders a ratatui `Table` with a row-number column on the left. Rows are styled only by the cursor, the column cursor and the visual selection; there is no alternating background.

Column widths set by hand (`z>`/`z<`/`z_`/`z=`) live in `App::widths`, keyed by **source** column so they follow a column through `:select`. They are display state, not view state — how wide a column is drawn is a fact about this screen, not about which rows and columns are being asked for — so they are deliberately not in the `View`, and `u` does not take them back: `u` is the edit buffer's undo, and mixing display noise into the data history would make it mean two things. `z=` is the reset. A set width is exempt from the default cap, and pushes later columns off the right edge rather than squeezing them, which is what a spreadsheet does. `ui::drawn_width` answers what a column is currently drawn at so the app layer does not re-derive the layout arithmetic — it did once, and the two drifted.

Columns are **pinned** to the left edge with `zp` (`z|` unpins the lot), so a key or a label stays in sight while `h`/`l` walk the columns it is being read against. `App::pinned` is keyed by source column for the same reason `widths` is, and is display state for the same reason: which column stays in sight while you move sideways is a fact about this screen. `App::display_pins` is the crossing point — `Store` counts source columns and the widget counts display positions — so a pin on a column `:select` hides is not forgotten, it simply has nowhere to be drawn. In the renderer the pinned columns lead `vis_cols` and are then *skipped* by the scrolling walk, or a column both pinned and scrolled to would be drawn twice; `ui::col_offset_showing` pays for the block out of its budget and steps over pins on the way left, since they are already drawn and already paid for. A second `│` closes the block: the gutter already ends in one, and without it a pinned column reads as sitting beside the column next to it, which is the one thing it is not. Past what `ui::pin_fits` allows plv **refuses the pin** rather than drawing a table with no scrolling region — a view that silently stops answering `h` and `l` is worse than being told why. Where the last scrolling column still does not fit it is squeezed into what is left rather than overflowing, because an overflow is paid for out of the pinned columns, undoing the one thing they were set to do. The second key of a `z` command is lowercased (a shift held for `z<` lands on the `z` too), so unpinning everything is `z|` and not `zP` — and a stray shift on `zp` toggles rather than clearing.

The row-number gutter counts from the cursor by default, nvim's hybrid `number` + `relativenumber`: each row shows its distance and the cursor row shows its own number, so `{n}j` and `{n}G` can be read off rather than worked out. The current line is left-aligned where the distances are right-aligned, which is what makes it read as outdented — and both fill the same width, so the column does not shift as the cursor moves. `#` switches to plain absolute numbering. `row_num_width(row_offset, viewport_rows)` still sizes the column ahead of where you are — for the larger of three screens past `row_offset` and 99 — and only grows at powers of ten.

The sort indicator gives way to the column name, never the other way round. A sorted column asks for ` [▲1]` **on top of** its natural width, so `redistribute` hands it the slack first — counted into what the column asks for and not into what it is granted, so the set of visible columns does not change when a sort starts, which would shift the table sideways and disagree with `col_offset_showing`, which cannot see the sort. A width set by hand asks for exactly what was set: `z>` is the user saying how wide, and a sort must not talk them out of it. A lone key is drawn as just ` ▲`: the number is a priority, and a priority among one thing says nothing, so five characters of header said what two say. The brackets exist to bind the number to the arrow and go with it — they come back for a second key, which is the first time there is anything to tell apart. Where there is no slack, `header_parts` walks a ladder from whichever full form applies down to the bare arrow, and then truncates the name but never below what is left beside it. A six-wide `region` used to render as a bare `…`, all six characters spent naming the sort and losing the column.

Cells are rendered with `AnyValue::str_value()` rather than its `Display`, which quotes strings for debugging. An empty or absent field shows a recessive · rather than the word `null`: the two are the same thing to plv — both are written back as an empty field — and `null` as text would collide with a field whose value really is "null".

### A cell too big for its column: the strip and the window (`panel.rs`, `cell.rs`)

A cell too big for its column has two views, because reading one value and scanning a column of them are different things to want. `zk` shows it in the panel **strip** above the status bar and stays on while the cursor moves — a column of long values is read by walking down it. It shares that strip with `:` completion; the two cannot both be wanted, since one belongs to the command line and the other to the table. Capped at half the screen, with the last line saying how much was left out rather than stopping in silence. It sits in the `z` family because it is display state of exactly that kind: it stays on, it takes its rows out of the table, and it says nothing about which rows and columns are being asked for.

`K` opens the same cell in a **window** over the table (`ui/cell.rs`) — vim's `K` opens a pager in a window of its own, and that is what a value with its own line breaks, or one day its own JSON structure, needs. It **covers** rather than displaces, like the `?` overlay and unlike the strip, so nothing about the layout underneath changes and closing it puts the screen back exactly as it was. It is **modal**: it holds every key while it is up, because each key that would move the table is one it wants for moving through the value — so `q` closes the window rather than quitting plv, which is the picker's rule and for the picker's reason. `ui::cell::window` answers where the window sits *and* how the value wrapped inside it, from one place, so the renderer and the scroll clamp cannot disagree about how many lines there are; the clamp is done in `draw` (as the picker's is) because that is the only point that knows both, which is what lets `G` simply ask for further than there is. The wrap is **word-aware**, unlike the strip's: the strip shows a few lines of a value that is mostly off screen anyway, where a hard break is honest about the truncation, but this view claims to show the whole thing, and prose broken mid-word down a wide window is unreadable. A run with no space in it is still cut on the width, having nowhere else to break. The window sizes to its title as well as to its value, since a border too narrow to name the column cannot say what it is showing, and the title walks a ladder down to the bare name where even that does not fit. The footer says the position only when there is something to scroll, and gives up its keys before its position — the position is the part that cannot be guessed from the screen.

The window has **its own `/`**, with `n`/`N` to step. Not the table's: that one moves the cursor between rows, and borrowing it would mean closing the window changed which row you were on, so the search is created with the window and dies with it. It shares the `:`/`/` prompt, because there is one line that takes typed input and drawing a second would be two answers to the same question — `handle_search_key` looks at whether the window is up to decide which of the two searches a pattern is for.

**The pattern is matched against the value, not against the screen.** The wrap breaks lines at spaces, so searching what is drawn would mean `line 137` is found or not found depending on how wide the window happens to be, and a resize would silently change the answer — a test pins the count identical across six widths. So a match is found in the value's own line and then *placed* on the wrapped lines that draw it, which is what `ui::cell::Origin` is for: it records, per drawn line, which source line it came from, how far into it it begins, and how many leading characters are the hanging indent the wrap *added* rather than the value's own — miscount that last one and every highlight on a continuation line lands a few columns to the left. The consequence is that a `Hit` is a **piece** of a match rather than a match: a run the wrap broke is drawn in two places and counted once, so the renderer colours fragments while the footer counts matches and `n` steps between the lines they start on.

Matches draw in `Theme::match_*`, the colours the table's search uses, and the one `n` is on takes the cursor's instead — with one to step to there has to be a way to see which one you are on. The highlight replaces only the background and foreground, so the weight and slant a document gave a run survive being found: a highlight says *this is what you asked for*, not *this is a different kind of thing*. An empty match is skipped rather than counted, since `x*` matches between every pair of characters and `1/4212` that never moves has answered nothing. A pattern that found nothing says `none` rather than falling silent, or "found nothing" and "never searched" would look the same and the reader would assume the second.

`n` steps from the match you are on while it is still on screen, and from the top of the view once you have scrolled away — being sent back up the value to resume from somewhere off screen is the surprising answer. It is `/`'s rule out in the table, with the viewport playing the part the cursor plays there. The hits are found in `draw` for the reason the scroll is clamped there — it is the only place that knows how the value wrapped — and found again only when the width or the raw/formatted view changed, never per keypress.

### Documents in a cell (`json.rs`, `markup.rs`, `markdown.rs`, `syntax.rs`)

A cell the window recognises as a **document** is drawn as one (`ui/json.rs`): JSON is re-indented and coloured by what each run of it is. Two rules shape it. **Detection is a parse, not a sniff** — a cell shown as pretty JSON that was not JSON is a lie about the data, and a viewer that lies once cannot be trusted for the rest of the session, which is worse than plain text; so a format is claimed only when the parse walks the whole value, and everything else falls back to text. And **the original bytes are re-emitted, never re-serialised**: parsing into a model and printing it back loses key order, reformats `1.0` and `1e3`, drops duplicate keys and rewrites escapes — `writer.rs` splices bytes rather than re-serialising a frame for exactly these reasons, and a reader that quietly disagreed with it about what is in the file would be worse than one that shows nothing. So every piece of text the module emits is a slice of the value, and the only things added are line breaks and spaces. The parse is done once when the window opens rather than per keypress, and is held in `Inspect`; the cursor cannot move while the window is up, so the cell under it cannot change. The title names the format it recognised and `r` goes back to the bytes (saying `json, raw`), because a reformat the title did not own up to would leave the file's own line breaks indistinguishable from plv's, and the raw value is the one that gets edited and written. `r` is offered only where there is another view to switch to. Colours are `Theme::syntax_*`, named for what a run of text *is* rather than for JSON, and `ui/syntax.rs` holds the `Kind`/`Piece` pair every formatter hands back plus the one place that turns them into styled lines — the formatters themselves hold no ratatui and no theme.

**XML and HTML** go the same way (`ui/markup.rs`): tags laid out one element to a line, the original bytes re-emitted so attribute quoting and entities survive, and the only additions line breaks and indentation. It is **well-formed or nothing** — every tag closed, in the right order, reaching the end of the value — which leaves a good deal of real HTML as text, and deliberately: a half-parsed document drawn as a tree is a claim about where things nest, and a wrong one is worse than no indentation. Void elements are the exception, because closing themselves is their rule rather than a mistake and the list is finite; needing that rule, a bare attribute, an unquoted value or a doctype is what makes a document `html` rather than `xml`, which is one parser answering both. An element whose children are all elements is broken out; one with any prose in it stays on a single line, because breaking prose apart to show structure would be showing structure that is not there, and whitespace *between* elements is the formatter's while whitespace inside prose is the author's. `<script>` and `<style>` hold text and not markup, or the first `a < b` inside one would fail the parse.

**Markdown** is the odd one out (`ui/markdown.rs`): it was designed to be read as written, so there is no indentation to add and nothing to move. plv only says which parts are which — a heading bold, a fence in the code colour, a bullet's dash out of the way — and **every marker stays where it was**, drawn dim rather than hidden. A test holds the pieces to reproducing the value character for character: hiding `##` and `**` would be the first place plv drew something other than what a cell holds, and would take away the difference between a `*` in prose and a `*` that made a bullet. It is also the one **judgement** rather than a parse, since a paragraph is a valid markdown document — what is asked is whether there is structure worth drawing (a heading, a fence, a quote, or *more than one* list item, since a lone dash is a sentence as often as a bullet). That is why it is tried **last**, after the two parses: the loosest test must not claim what a stricter one would have. A false positive costs a dim dash, which is the whole reason keeping the markers is what makes the judgement affordable. `Kind::Strong` and `Kind::Emphasis` carry weight and slant rather than colour, since what a document marks as strong is not another kind of thing, it is the same words said louder. `Heading` is bold in the key colour and `Code` takes the string colour.

### The completion panel (`panel.rs`)

What the panel is filled with, and why completion is a prefix match, is in [`../README.md`](../README.md#completion-completers).

`Panel` draws the candidates above the status bar. Unlike the `?` overlay it
sits *in* the layout — `viewport_rows` subtracts its height, so the table gives
up exactly the rows the panel takes rather than being covered. It is capped at
four rows of candidates, plus one line saying `… n more` rather than truncating in silence, because a list
that quietly stops reads as the whole list.

### The help overlay and the status bar (`help.rs`, `statusbar.rs`)

`StatusBar`: a single line showing the file name and its `[+n]` edit count, the cursor's `Row r/total` and `Col c/total`, the view summary (with a spinner while a filter resolves), the size of a visual selection, a pending count or `z` prefix, the search state (`/pat [cur/total]`, `[no matches]`, or a spinner), an animated `Sorting`, and key help. A message replaces the whole bar.

`?` opens a key-binding overlay whose contents follow the current screen
(`App::help_sections`); any key dismisses it. The status bar only has room for a
few hints, so it degrades to `?:help` and then to nothing rather than truncating
the position readout — the overlay is the authoritative in-app reference.

## Verification

- Run: `cargo test` (the widget tests live beside the code, e.g.
  `cargo test the_window_width_does_not_change_what_was_found`,
  `cargo test every_character_survives_being_named`).
