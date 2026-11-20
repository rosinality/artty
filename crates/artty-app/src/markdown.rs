//! Markdown, as agents write it, shown: paragraphs, headings, emphasis,
//! inline code and code blocks, lists (tasks too), quotes, rules, tables,
//! links (opened with a click). Parsed into blocks first (see [`parse`]),
//! then drawn with the app's text.

use std::ops::Range;

use gpui::{
    AnyElement, App, Font, FontStyle, FontWeight, Hsla, InteractiveElement, IntoElement,
    ParentElement, SharedString, StatefulInteractiveElement, StrikethroughStyle, Styled,
    StyledText, TextRun, UnderlineStyle, Window, div, px,
};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

/// A run of text and how it's set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub strike: bool,
    pub link: Option<String>,
}

impl Span {
    fn same_style(&self, other: &Span) -> bool {
        (self.bold, self.italic, self.code, self.strike, &self.link)
            == (other.bold, other.italic, other.code, other.strike, &other.link)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    Paragraph(Vec<Span>),
    Heading(u8, Vec<Span>),
    Code(String),
    /// `start`: an ordered list's first number.
    List {
        start: Option<u64>,
        items: Vec<Item>,
    },
    Quote(Vec<Block>),
    Rule,
    Table {
        head: Vec<Vec<Span>>,
        rows: Vec<Vec<Vec<Span>>>,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Item {
    /// A task list's box: ticked or not.
    pub task: Option<bool>,
    pub blocks: Vec<Block>,
}

/// What blocks are being put together in.
enum Frame {
    Blocks(Vec<Block>),
    Quote(Vec<Block>),
    List { start: Option<u64>, items: Vec<Item> },
    Item(Item),
    Table { head: Vec<Vec<Span>>, rows: Vec<Vec<Vec<Span>>>, in_head: bool },
}

/// Text being put together: a paragraph's (maybe one a tight list item
/// implies), a heading's, a table cell's.
enum Inline {
    Paragraph,
    Heading(u8),
    Cell,
}

#[derive(Default)]
struct Style {
    bold: usize,
    italic: usize,
    strike: usize,
    link: Option<String>,
}

struct Builder {
    frames: Vec<Frame>,
    inline: Option<(Inline, Vec<Span>)>,
    code: Option<String>,
    style: Style,
}

impl Builder {
    fn push_block(&mut self, block: Block) {
        match self.frames.last_mut() {
            Some(Frame::Blocks(blocks) | Frame::Quote(blocks)) => blocks.push(block),
            Some(Frame::Item(item)) => item.blocks.push(block),
            // (Nowhere else takes blocks: a list takes items.)
            _ => {},
        }
    }

    /// Close the text being put together, where it goes.
    fn end_inline(&mut self) {
        let Some((kind, spans)) = self.inline.take() else { return };
        match kind {
            Inline::Paragraph if spans.iter().all(|s| s.text.trim().is_empty()) => {},
            Inline::Paragraph => self.push_block(Block::Paragraph(spans)),
            Inline::Heading(level) => self.push_block(Block::Heading(level, spans)),
            Inline::Cell => {
                if let Some(Frame::Table { head, rows, in_head }) = self.frames.last_mut() {
                    match (*in_head, rows.last_mut()) {
                        (true, _) => head.push(spans),
                        (false, Some(row)) => row.push(spans),
                        (false, None) => rows.push(vec![spans]),
                    }
                }
            },
        }
    }

    fn text(&mut self, text: &str, code: bool) {
        if let Some(block) = self.code.as_mut() {
            block.push_str(text);
            return;
        }
        // Text straight in a list item (a tight list): a paragraph.
        let (_, spans) = self.inline.get_or_insert_with(|| (Inline::Paragraph, Vec::new()));
        let span = Span {
            text: text.to_owned(),
            bold: self.style.bold > 0,
            italic: self.style.italic > 0,
            code,
            strike: self.style.strike > 0,
            link: self.style.link.clone(),
        };
        match spans.last_mut() {
            Some(last) if last.same_style(&span) => last.text.push_str(&span.text),
            _ => spans.push(span),
        }
    }

    fn pop_into_parent(&mut self) {
        let Some(frame) = self.frames.pop() else { return };
        match frame {
            Frame::Quote(blocks) => self.push_block(Block::Quote(blocks)),
            Frame::List { start, items } => self.push_block(Block::List { start, items }),
            Frame::Item(item) => {
                if let Some(Frame::List { items, .. }) = self.frames.last_mut() {
                    items.push(item);
                }
            },
            Frame::Table { head, rows, .. } => self.push_block(Block::Table { head, rows }),
            Frame::Blocks(blocks) => self.frames.push(Frame::Blocks(blocks)),
        }
    }
}

/// `text`'s blocks.
pub fn parse(text: &str) -> Vec<Block> {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut b = Builder {
        frames: vec![Frame::Blocks(Vec::new())],
        inline: None,
        code: None,
        style: Style::default(),
    };
    for event in Parser::new_ext(text, options) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    b.end_inline();
                    b.inline = Some((Inline::Paragraph, Vec::new()));
                },
                Tag::Heading { level, .. } => {
                    b.end_inline();
                    b.inline = Some((Inline::Heading(level as u8), Vec::new()));
                },
                Tag::CodeBlock(_) => {
                    b.end_inline();
                    b.code = Some(String::new());
                },
                Tag::BlockQuote(_) => {
                    b.end_inline();
                    b.frames.push(Frame::Quote(Vec::new()));
                },
                Tag::List(start) => {
                    b.end_inline();
                    b.frames.push(Frame::List { start, items: Vec::new() });
                },
                Tag::Item => b.frames.push(Frame::Item(Item::default())),
                Tag::Table(_) => {
                    b.end_inline();
                    b.frames.push(Frame::Table {
                        head: Vec::new(),
                        rows: Vec::new(),
                        in_head: false,
                    });
                },
                Tag::TableHead => {
                    if let Some(Frame::Table { in_head, .. }) = b.frames.last_mut() {
                        *in_head = true;
                    }
                },
                Tag::TableRow => {
                    if let Some(Frame::Table { rows, .. }) = b.frames.last_mut() {
                        rows.push(Vec::new());
                    }
                },
                Tag::TableCell => b.inline = Some((Inline::Cell, Vec::new())),
                Tag::Strong => b.style.bold += 1,
                Tag::Emphasis => b.style.italic += 1,
                Tag::Strikethrough => b.style.strike += 1,
                Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                    b.style.link = Some(dest_url.to_string())
                },
                _ => {},
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::TableCell => b.end_inline(),
                TagEnd::CodeBlock => {
                    let code = b.code.take().unwrap_or_default();
                    b.push_block(Block::Code(code.trim_end_matches('\n').to_owned()));
                },
                TagEnd::BlockQuote(_) | TagEnd::List(_) | TagEnd::Table => {
                    b.end_inline();
                    b.pop_into_parent();
                },
                TagEnd::Item => {
                    b.end_inline();
                    b.pop_into_parent();
                },
                TagEnd::TableHead => {
                    if let Some(Frame::Table { in_head, .. }) = b.frames.last_mut() {
                        *in_head = false;
                    }
                },
                TagEnd::Strong => b.style.bold = b.style.bold.saturating_sub(1),
                TagEnd::Emphasis => b.style.italic = b.style.italic.saturating_sub(1),
                TagEnd::Strikethrough => b.style.strike = b.style.strike.saturating_sub(1),
                TagEnd::Link | TagEnd::Image => b.style.link = None,
                _ => {},
            },
            Event::Text(text) => b.text(&text, false),
            Event::Code(text) => b.text(&text, true),
            Event::SoftBreak => b.text(" ", false),
            Event::HardBreak => b.text("\n", false),
            Event::Html(html) | Event::InlineHtml(html) => b.text(&html, false),
            Event::TaskListMarker(done) => {
                if let Some(Frame::Item(item)) = b.frames.last_mut() {
                    item.task = Some(done);
                }
            },
            Event::Rule => {
                b.end_inline();
                b.push_block(Block::Rule);
            },
            _ => {},
        }
    }
    b.end_inline();
    if let Some(code) = b.code.take() {
        b.push_block(Block::Code(code.trim_end_matches('\n').to_owned()));
    }
    // (Frames left open by text that ends early.)
    while b.frames.len() > 1 {
        b.pop_into_parent();
    }
    match b.frames.pop() {
        Some(Frame::Blocks(blocks)) => blocks,
        _ => Vec::new(),
    }
}

/// How markdown is drawn: colors and fonts.
#[derive(Clone)]
pub struct Look {
    pub text: Hsla,
    pub dim: Hsla,
    pub link: Hsla,
    pub code_bg: Hsla,
    pub rule: Hsla,
    pub font: SharedString,
    pub mono: SharedString,
    /// What's selected, behind it.
    pub selection: Hsla,
}

/// A piece of a message's text as drawn (a paragraph, a heading, a code
/// block's line, a table's cell): where it starts in the message's text
/// (the pieces' texts, a line each), what it says, and its layout, to find
/// the character under the pointer.
pub struct Piece {
    pub offset: usize,
    pub text: String,
    pub layout: gpui::TextLayout,
}

/// A message's pieces, as last drawn (shared with its pointer's handlers).
#[derive(Clone, Default)]
pub struct Pieces(std::rc::Rc<std::cell::RefCell<Vec<Piece>>>);

impl Pieces {
    /// The character (as an offset in the message's text) nearest `point`,
    /// once drawn.
    pub fn offset_at(&self, point: gpui::Point<gpui::Pixels>) -> Option<usize> {
        let pieces = self.0.borrow();
        let bounds: Vec<gpui::Bounds<gpui::Pixels>> =
            pieces.iter().map(|p| p.layout.bounds()).collect();
        // The first line not above the point; of those side by side (a
        // table's row), the one it's over, or nearest.
        let row = bounds.iter().position(|b| point.y <= b.bottom())?;
        let top = bounds[row].top();
        let mut best = row;
        for (i, b) in bounds.iter().enumerate().skip(row) {
            if b.top() > top + px(1.) {
                break;
            }
            if b.left() <= point.x {
                best = i;
            }
        }
        let piece = &pieces[best];
        if point.y < bounds[best].top() {
            return Some(piece.offset);
        }
        let index = match piece.layout.index_for_position(point) {
            Ok(index) | Err(index) => index.min(piece.text.len()),
        };
        Some(piece.offset + index)
    }

    /// The pieces' text in `range` (the message's offsets), pieces a line each.
    pub fn text(&self, range: Range<usize>) -> String {
        let mut out = String::new();
        for piece in self.0.borrow().iter() {
            let end = piece.offset + piece.text.len();
            if end < range.start || piece.offset > range.end {
                continue;
            }
            let from = range.start.saturating_sub(piece.offset).min(piece.text.len());
            let to = (range.end - piece.offset.min(range.end)).min(piece.text.len());
            let (from, to) = (floor_char(&piece.text, from), floor_char(&piece.text, to));
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&piece.text[from..to.max(from)]);
        }
        out
    }

    /// The end of the last piece: the whole message's text.
    pub fn len(&self) -> usize {
        self.0.borrow().last().map_or(0, |p| p.offset + p.text.len())
    }
}

fn floor_char(text: &str, mut at: usize) -> usize {
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// What drawing a message needs: its look, where its pieces go, what's
/// selected (offsets in its text), and the next piece's offset.
struct Draw<'a> {
    look: &'a Look,
    pieces: Pieces,
    selected: Option<Range<usize>>,
    next: std::cell::Cell<usize>,
}

impl Draw<'_> {
    /// A piece of `len` bytes is drawn: its offset (the next starts a line
    /// on), and what of it is selected.
    fn piece(&self, text: &str, layout: gpui::TextLayout) -> Option<Range<usize>> {
        let offset = self.next.get();
        self.next.set(offset + text.len() + 1);
        self.pieces.0.borrow_mut().push(Piece { offset, text: text.to_owned(), layout });
        let selected = self.selected.clone()?;
        let start = selected.start.max(offset);
        let end = selected.end.min(offset + text.len());
        // (A line break selected through: its piece's end shows it.)
        (start < end || (start == end && selected.start < offset && selected.end > end))
            .then(|| start - offset..end - offset)
    }
}

/// `blocks` drawn, a column of them; `id`: unique among what's drawn (for
/// links' clicks). The pieces drawn go to `pieces`; `selected`: what's
/// selected, as offsets in the message's text.
pub fn view(
    blocks: &[Block],
    look: &Look,
    id: &str,
    pieces: &Pieces,
    selected: Option<Range<usize>>,
) -> gpui::Div {
    pieces.0.borrow_mut().clear();
    let draw = Draw { look, pieces: pieces.clone(), selected, next: std::cell::Cell::new(0) };
    blocks_view(blocks, &draw, id)
}

fn blocks_view(blocks: &[Block], draw: &Draw, id: &str) -> gpui::Div {
    div().flex().flex_col().gap(px(6.)).children(
        blocks.iter().enumerate().map(|(i, block)| block_view(block, draw, &format!("{id}-{i}"))),
    )
}

fn block_view(block: &Block, draw: &Draw, id: &str) -> AnyElement {
    let look = draw.look;
    match block {
        Block::Paragraph(spans) => text_view(spans, draw, id, false),
        Block::Heading(level, spans) => {
            let size = match level {
                1 => 16.,
                2 => 15.,
                _ => 14.,
            };
            let bold: Vec<Span> = spans.iter().map(|s| Span { bold: true, ..s.clone() }).collect();
            div()
                .pt(px(2.))
                .text_size(px(size))
                .child(text_view(&bold, draw, id, false))
                .into_any_element()
        },
        Block::Code(code) => {
            let group = SharedString::from(format!("{id}-code"));
            let copied = code.clone();
            div()
                .group(group.clone())
                .relative()
                .flex()
                .flex_col()
                .px(px(8.))
                .py(px(6.))
                .rounded(px(6.))
                .bg(look.code_bg)
                .text_size(px(12.))
                .children(code.lines().enumerate().map(|(n, line)| {
                    let span = Span { text: line.to_owned(), code: true, ..Span::default() };
                    // (An empty line keeps its height.)
                    div().min_h(px(16.)).child(text_view(&[span], draw, &format!("{id}-{n}"), true))
                }))
                // On hover: Copy, the code alone.
                .child(
                    div()
                        .id(SharedString::from(format!("{id}-copy")))
                        .absolute()
                        .top(px(4.))
                        .right(px(4.))
                        .px(px(6.))
                        .rounded(px(4.))
                        .text_size(px(11.))
                        .text_color(look.dim)
                        .bg(look.code_bg)
                        .opacity(0.)
                        .group_hover(group, |s| s.opacity(1.))
                        .hover(|d| d.text_color(look.text))
                        .child("Copy")
                        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(copied.clone()))
                        }),
                )
                .into_any_element()
        },
        Block::List { start, items } => div()
            .flex()
            .flex_col()
            .gap(px(3.))
            .children(items.iter().enumerate().map(|(n, item)| {
                let marker = match (item.task, start) {
                    (Some(true), _) => "☑".to_owned(),
                    (Some(false), _) => "☐".to_owned(),
                    (None, Some(first)) => format!("{}.", first + n as u64),
                    (None, None) => "•".to_owned(),
                };
                div()
                    .flex()
                    .gap(px(6.))
                    .child(div().flex_none().min_w(px(12.)).text_color(look.dim).child(marker))
                    .child(blocks_view(&item.blocks, draw, &format!("{id}-{n}")).flex_1().min_w_0())
            }))
            .into_any_element(),
        Block::Quote(blocks) => div()
            .pl(px(10.))
            .border_l_2()
            .border_color(look.rule)
            .child(blocks_view(blocks, draw, id))
            .into_any_element(),
        Block::Rule => div().h(px(1.)).my(px(4.)).bg(look.rule).into_any_element(),
        Block::Table { head, rows } => {
            let row = |cells: &[Vec<Span>], header: bool, r: usize| {
                div().flex().border_b_1().border_color(look.rule).children(
                    cells.iter().enumerate().map(|(c, cell)| {
                        let cell: Vec<Span> = match header {
                            true => cell.iter().map(|s| Span { bold: true, ..s.clone() }).collect(),
                            false => cell.clone(),
                        };
                        div().flex_1().min_w_0().px(px(6.)).py(px(3.)).child(text_view(
                            &cell,
                            draw,
                            &format!("{id}-{r}-{c}"),
                            false,
                        ))
                    }),
                )
            };
            div()
                .flex()
                .flex_col()
                .rounded(px(6.))
                .border_1()
                .border_color(look.rule)
                .text_size(px(12.5))
                .when_not_empty(head, |d| d.child(row(head, true, 0)))
                .children(rows.iter().enumerate().map(|(r, cells)| row(cells, false, r + 1)))
                .into_any_element()
        },
    }
}

trait WhenNotEmpty: Sized {
    fn when_not_empty<T>(self, list: &[T], then: impl FnOnce(Self) -> Self) -> Self {
        if list.is_empty() { self } else { then(self) }
    }
}

impl WhenNotEmpty for gpui::Div {}

/// `runs` with `selected` (bytes) marked: split where it starts and ends.
fn with_selection(runs: Vec<TextRun>, selected: Range<usize>, color: Hsla) -> Vec<TextRun> {
    let mut out = Vec::new();
    let mut at = 0;
    for run in runs {
        let (start, end) = (at, at + run.len);
        at = end;
        let cuts = [start, selected.start.clamp(start, end), selected.end.clamp(start, end), end];
        for pair in cuts.windows(2) {
            let (from, to) = (pair[0], pair[1]);
            if from == to {
                continue;
            }
            let inside = from >= selected.start && to <= selected.end;
            out.push(TextRun {
                len: to - from,
                background_color: if inside { Some(color) } else { run.background_color },
                ..run.clone()
            });
        }
    }
    out
}

/// Spans as one piece of text, each set as it is; links open with a click.
/// `block_code`: a code block's line (its block has the background).
fn text_view(spans: &[Span], draw: &Draw, id: &str, block_code: bool) -> AnyElement {
    let look = draw.look;
    let mut text = String::new();
    let mut runs = Vec::new();
    let mut links: Vec<(Range<usize>, String)> = Vec::new();
    for span in spans {
        if span.text.is_empty() {
            continue;
        }
        let start = text.len();
        text.push_str(&span.text);
        let mut font = gpui::font(if span.code { look.mono.clone() } else { look.font.clone() });
        if span.bold {
            font.weight = FontWeight::BOLD;
        }
        if span.italic {
            font.style = FontStyle::Italic;
        }
        let color = if span.link.is_some() { look.link } else { look.text };
        runs.push(TextRun {
            len: span.text.len(),
            font: Font { ..font },
            color,
            background_color: (span.code && !block_code).then_some(look.code_bg),
            underline: span.link.as_ref().map(|_| UnderlineStyle {
                thickness: px(1.),
                color: Some(look.link),
                wavy: false,
            }),
            strikethrough: span
                .strike
                .then_some(StrikethroughStyle { thickness: px(1.), color: Some(look.text) }),
        });
        if let Some(url) = &span.link {
            links.push((start..text.len(), url.clone()));
        }
    }
    let probe = StyledText::new(text.clone());
    let layout = probe.layout().clone();
    let selected = draw.piece(&text, layout.clone());
    let runs = match selected {
        Some(range) if !text.is_empty() => with_selection(runs, range, look.selection),
        _ => runs,
    };
    // (Its layout stays the one recorded.)
    let styled = probe.with_runs(runs);
    if links.is_empty() {
        return styled.into_any_element();
    }
    let (ranges, urls): (Vec<_>, Vec<_>) = links.into_iter().unzip();
    gpui::InteractiveText::new(SharedString::from(id.to_owned()), styled)
        .on_click(ranges, move |i, _: &mut Window, cx: &mut App| {
            if let Some(url) = urls.get(i).filter(|u| u.contains(':')) {
                cx.open_url(url);
            }
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(spans: &[Span]) -> String {
        spans.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn paragraphs_and_inline_styles() {
        let blocks = parse("Fixed **the** `login`\nredirect, see [docs](https://x.io).\n\nDone.");
        let [Block::Paragraph(first), Block::Paragraph(second)] = blocks.as_slice() else {
            panic!("{blocks:?}")
        };
        assert_eq!(plain(first), "Fixed the login redirect, see docs.");
        assert!(first.iter().any(|s| s.bold && s.text == "the"));
        assert!(first.iter().any(|s| s.code && s.text == "login"));
        assert!(
            first.iter().any(|s| s.link.as_deref() == Some("https://x.io") && s.text == "docs")
        );
        assert_eq!(plain(second), "Done.");
    }

    #[test]
    fn headings_code_lists_quotes_tables() {
        let text = "# Plan\n\n```rust\nfn main() {}\n\nlet x = 1;\n```\n\n- one\n- **two**\n  1. a\n  2. b\n\n* [x] done\n* [ ] not\n\n> quoted\n\n---\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
        let blocks = parse(text);
        assert!(matches!(&blocks[0], Block::Heading(1, s) if plain(s) == "Plan"));
        assert_eq!(blocks[1], Block::Code("fn main() {}\n\nlet x = 1;".into()));
        let Block::List { start: None, items } = &blocks[2] else { panic!("{:?}", blocks[2]) };
        assert_eq!(items.len(), 2);
        assert!(matches!(
            &items[1].blocks[..],
            [Block::Paragraph(_), Block::List { start: Some(1), .. }]
        ));
        let Block::List { items: tasks, .. } = &blocks[3] else { panic!("{:?}", blocks[3]) };
        assert_eq!(tasks.iter().map(|i| i.task).collect::<Vec<_>>(), [Some(true), Some(false)]);
        assert!(matches!(&blocks[4], Block::Quote(inner) if inner.len() == 1));
        assert_eq!(blocks[5], Block::Rule);
        let Block::Table { head, rows } = &blocks[6] else { panic!("{:?}", blocks[6]) };
        assert_eq!(head.iter().map(|c| plain(c)).collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].iter().map(|c| plain(c)).collect::<Vec<_>>(), ["1", "2"]);
    }

    #[test]
    fn selected_text_across_pieces() {
        let pieces = Pieces::default();
        for (offset, text) in [(0, "Fixed the bug."), (15, "cargo test"), (26, "한글 too")] {
            let layout = gpui::TextLayout::default();
            pieces.0.borrow_mut().push(Piece { offset, text: text.into(), layout });
        }
        assert_eq!(pieces.len(), 26 + "한글 too".len());
        assert_eq!(pieces.text(6..9), "the");
        // Over a piece's end: its line ends, the next goes on.
        assert_eq!(pieces.text(10..20), "bug.\ncargo");
        assert_eq!(pieces.text(0..pieces.len()), "Fixed the bug.\ncargo test\n한글 too");
        // (Inside a character: from its start.)
        assert_eq!(pieces.text(27..32), "한글");
    }

    #[test]
    fn plain_text_and_what_ends_early() {
        assert_eq!(
            parse("just words"),
            [Block::Paragraph(vec![Span { text: "just words".into(), ..Span::default() }])]
        );
        // A message cut off inside a code block still shows what it has.
        assert_eq!(parse("```\nhalf"), [Block::Code("half".into())]);
        assert!(parse("").is_empty());
    }
}
