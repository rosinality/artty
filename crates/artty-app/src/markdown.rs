//! Markdown, as agents write it, shown: paragraphs, headings, emphasis,
//! inline code and code blocks (colored by language), lists (tasks too),
//! quotes, rules, tables, links (opened with a click). Parsed into blocks
//! first (see [`parse`]), then drawn with the app's text.
//!
//! Each run of text remembers where it came from in the markdown, and how
//! far the markup around it reaches (`**`, a link's `[...](...)`, a list
//! item's `1. `, a code block's fences): what's selected can be copied as the
//! markdown it was written as, markup and all for what's selected whole.

use std::ops::Range;

use gpui::{
    AnyElement, App, Font, FontStyle, FontWeight, Hsla, InteractiveElement, IntoElement,
    ParentElement, SharedString, StatefulInteractiveElement, StrikethroughStyle, Styled,
    StyledText, TextRun, UnderlineStyle, Window, div, px,
};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

use crate::highlight::{self, Kind};

/// A run of text, how it's set, and where it came from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub strike: bool,
    pub link: Option<String>,
    /// A code block's color for it.
    pub kind: Option<Kind>,
    /// Its text in the markdown (bytes).
    pub src: Range<usize>,
    /// Which run it is (for the markup around runs: see [`Group`]).
    pub id: usize,
}

/// Markup around runs (`**…**`, `[…](…)`, a list item's `- …`, a code
/// block's fences): where it is in the markdown, from its start to its end,
/// and the first and last runs inside it. What's selected copies with it
/// only when all that's inside is selected (so it's never half there).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub src: Range<usize>,
    pub first: usize,
    pub last: usize,
}

/// A message's blocks, and its markup's groups.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Doc {
    pub blocks: Vec<Block>,
    pub groups: Vec<Group>,
}

impl Span {
    fn same_style(&self, other: &Span) -> bool {
        (self.bold, self.italic, self.code, self.strike, &self.link, self.kind)
            == (other.bold, other.italic, other.code, other.strike, &other.link, other.kind)
    }
}

/// A code block's line: its text, and where it starts in the markdown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeLine {
    pub text: String,
    pub src: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    Paragraph(Vec<Span>),
    Heading(u8, Vec<Span>),
    /// Its language (the fence's), lines, and where it starts and ends in
    /// the markdown (fences and all).
    Code {
        lang: String,
        lines: Vec<CodeLine>,
        src: Range<usize>,
    },
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

/// A code block being read: its language, text, where its text was in the
/// markdown (pieces of it, as the parser gave them), and where it started.
struct Code {
    lang: String,
    text: String,
    from: Vec<(usize, usize)>,
    start: usize,
}

struct Builder<'a> {
    source: &'a str,
    frames: Vec<Frame>,
    inline: Option<(Inline, Vec<Span>)>,
    code: Option<Code>,
    style: Style,
    /// Markup open: where it starts, and its first run (once there's one).
    open: Vec<(usize, Option<usize>)>,
    groups: Vec<Group>,
    /// The last run's id, and the next one's.
    last: Option<usize>,
    next_id: usize,
}

impl Builder<'_> {
    fn push_block(&mut self, block: Block) {
        match self.frames.last_mut() {
            Some(Frame::Blocks(blocks) | Frame::Quote(blocks)) => blocks.push(block),
            Some(Frame::Item(item)) => item.blocks.push(block),
            // (Nowhere else takes blocks: a list takes items.)
            _ => {},
        }
    }

    /// Markup opens at `at`.
    fn open(&mut self, at: usize) {
        self.open.push((at, None));
    }

    /// The markup opened last closes at `at`: a group, if runs are in it.
    fn close(&mut self, at: usize) {
        if let Some((start, Some(first))) = self.open.pop()
            && let Some(last) = self.last
        {
            self.groups.push(Group { src: start..at, first, last });
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

    /// Text that came from `src` in the markdown.
    fn text(&mut self, text: &str, src: Range<usize>, code: bool) {
        if let Some(block) = self.code.as_mut() {
            block.from.push((block.text.len(), src.start));
            block.text.push_str(text);
            return;
        }
        // Markup opened since the last run: this one starts it.
        let opened = self.open.iter().any(|(_, first)| first.is_none());
        // Text straight in a list item (a tight list): a paragraph.
        let (_, spans) = self.inline.get_or_insert_with(|| (Inline::Paragraph, Vec::new()));
        let span = Span {
            text: text.to_owned(),
            bold: self.style.bold > 0,
            italic: self.style.italic > 0,
            code,
            strike: self.style.strike > 0,
            link: self.style.link.clone(),
            kind: None,
            src,
            id: self.next_id,
        };
        // (One run with the last, if it's set alike, no markup opened
        // between, and its text runs on from it in the markdown.)
        match spans.last_mut() {
            Some(last)
                if last.same_style(&span)
                    && !opened
                    && self.last == Some(last.id)
                    && last.src.end == span.src.start =>
            {
                last.text.push_str(&span.text);
                last.src.end = span.src.end;
            },
            _ => {
                let id = span.id;
                spans.push(span);
                self.last = Some(id);
                self.next_id += 1;
                for (_, first) in self.open.iter_mut().filter(|(_, f)| f.is_none()) {
                    *first = Some(id);
                }
            },
        }
    }

    fn end_code(&mut self, end: usize) {
        let Some(code) = self.code.take() else { return };
        let text = code.text.strip_suffix('\n').unwrap_or(&code.text);
        // Each line where it is in the markdown (from the piece of text it's in).
        let mut lines = Vec::new();
        let mut at = 0;
        for line in text.split('\n') {
            let (piece_at, piece_src) =
                code.from.iter().rev().find(|(t, _)| *t <= at).copied().unwrap_or((0, code.start));
            lines.push(CodeLine { text: line.to_owned(), src: piece_src + (at - piece_at) });
            at += line.len() + 1;
        }
        if text.is_empty() {
            lines.clear();
        }
        self.push_block(Block::Code { lang: code.lang, lines, src: code.start..end });
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
#[cfg(test)]
pub fn parse(text: &str) -> Vec<Block> {
    parse_doc(text).blocks
}

/// `text`'s blocks, and its markup's groups.
pub fn parse_doc(text: &str) -> Doc {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut b = Builder {
        source: text,
        frames: vec![Frame::Blocks(Vec::new())],
        inline: None,
        code: None,
        style: Style::default(),
        open: Vec::new(),
        groups: Vec::new(),
        last: None,
        next_id: 1,
    };
    for (event, range) in Parser::new_ext(text, options).into_offset_iter() {
        match event {
            Event::Start(tag) => {
                match &tag {
                    Tag::Paragraph
                    | Tag::Heading { .. }
                    | Tag::Item
                    | Tag::BlockQuote(_)
                    | Tag::TableCell
                    | Tag::Strong
                    | Tag::Emphasis
                    | Tag::Strikethrough
                    | Tag::Link { .. }
                    | Tag::Image { .. } => b.open(range.start),
                    _ => {},
                }
                match tag {
                    Tag::Paragraph => {
                        b.end_inline();
                        b.inline = Some((Inline::Paragraph, Vec::new()));
                    },
                    Tag::Heading { level, .. } => {
                        b.end_inline();
                        b.inline = Some((Inline::Heading(level as u8), Vec::new()));
                    },
                    Tag::CodeBlock(kind) => {
                        b.end_inline();
                        let lang = match kind {
                            CodeBlockKind::Fenced(info) => info.to_string(),
                            CodeBlockKind::Indented => String::new(),
                        };
                        b.code = Some(Code {
                            lang,
                            text: String::new(),
                            from: Vec::new(),
                            start: range.start,
                        });
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
                }
            },
            Event::End(tag) => {
                match tag {
                    TagEnd::Paragraph
                    | TagEnd::Heading(_)
                    | TagEnd::Item
                    | TagEnd::BlockQuote(_)
                    | TagEnd::TableCell
                    | TagEnd::Strong
                    | TagEnd::Emphasis
                    | TagEnd::Strikethrough
                    | TagEnd::Link
                    | TagEnd::Image => {
                        // (Not the line break after it.)
                        let end = range.start + text[range.clone()].trim_end().len();
                        b.close(end);
                    },
                    _ => {},
                }
                match tag {
                    TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::TableCell => b.end_inline(),
                    TagEnd::CodeBlock => {
                        let end = range.start + text[range.clone()].trim_end().len();
                        b.end_code(end);
                    },
                    TagEnd::BlockQuote(_) | TagEnd::List(_) | TagEnd::Table | TagEnd::Item => {
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
                }
            },
            Event::Text(t) => b.text(&t, range, false),
            Event::Code(t) => {
                // Its text inside its backticks, which are its markup.
                let inner =
                    b.source[range.clone()].find(&*t).map_or(range.start, |at| range.start + at);
                b.open(range.start);
                b.text(&t, inner..inner + t.len(), true);
                b.close(range.end);
            },
            Event::SoftBreak => b.text(" ", range, false),
            Event::HardBreak => b.text("\n", range, false),
            Event::Html(html) | Event::InlineHtml(html) => b.text(&html, range, false),
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
    if b.code.is_some() {
        b.end_code(text.len());
    }
    // (Frames left open by text that ends early.)
    while b.frames.len() > 1 {
        b.pop_into_parent();
    }
    let blocks = match b.frames.pop() {
        Some(Frame::Blocks(blocks)) => blocks,
        _ => Vec::new(),
    };
    // Code blocks' runs get ids as they're drawn: from past these.
    Doc { blocks, groups: b.groups }
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
    /// Code's colors, by what it is.
    pub colors: fn(Kind) -> Hsla,
}

/// A run of a piece, for copying: its bytes in the piece's text, where it
/// came from, and which run it is.
#[derive(Clone, Debug)]
struct Run {
    shown: Range<usize>,
    src: Range<usize>,
    id: usize,
}

/// A piece of a message's text as drawn (a paragraph, a heading, a code
/// block's line, a table's cell): where it starts in the message's text
/// (the pieces' texts, a line each), what it says, its layout (to find the
/// character under the pointer), and its runs.
pub struct Piece {
    pub offset: usize,
    pub text: String,
    pub layout: gpui::TextLayout,
    runs: Vec<Run>,
}

/// A message's pieces, as last drawn (shared with its pointer's handlers),
/// its markdown, and its markup's groups.
#[derive(Clone, Default)]
pub struct Pieces(std::rc::Rc<std::cell::RefCell<Drawn>>);

/// What `Pieces` holds: the pieces, the markdown, the groups.
type Drawn = (Vec<Piece>, String, Vec<Group>);

impl Pieces {
    /// The character (as an offset in the message's text) nearest `point`,
    /// once drawn.
    pub fn offset_at(&self, point: gpui::Point<gpui::Pixels>) -> Option<usize> {
        let pieces = &self.0.borrow().0;
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

    /// The pieces' text in `range` (the message's offsets), pieces a line
    /// each: what's selected, as it reads.
    pub fn text(&self, range: Range<usize>) -> String {
        let mut out = String::new();
        for piece in self.0.borrow().0.iter() {
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

    /// What's selected (`range`, the message's offsets) as written: the
    /// markdown from its start to its end, with the markup around what's
    /// selected whole.
    pub fn markdown(&self, range: Range<usize>) -> String {
        let inner = self.0.borrow();
        let (pieces, source, groups) = (&inner.0, &inner.1, &inner.2);
        let (Some(mut from), Some(mut to)) =
            (source_start(pieces, range.start), source_end(pieces, range.end))
        else {
            return String::new();
        };
        // Where each run is shown, in the message's text.
        let mut shown: std::collections::HashMap<usize, Range<usize>> = Default::default();
        for piece in pieces {
            for run in &piece.runs {
                shown.insert(run.id, piece.offset + run.shown.start..piece.offset + run.shown.end);
            }
        }
        for group in groups {
            let (Some(first), Some(last)) = (shown.get(&group.first), shown.get(&group.last))
            else {
                continue;
            };
            if range.start <= first.start && range.end >= last.end {
                from = from.min(group.src.start);
                to = to.max(group.src.end);
            }
        }
        let (from, to) =
            (floor_char(source, from.min(source.len())), floor_char(source, to.min(source.len())));
        source[from..to.max(from)].trim_end_matches('\n').to_owned()
    }

    /// The end of the last piece: the whole message's text.
    pub fn len(&self) -> usize {
        self.0.borrow().0.last().map_or(0, |p| p.offset + p.text.len())
    }
}

/// Where the markdown of what's selected from `at` starts.
fn source_start(pieces: &[Piece], at: usize) -> Option<usize> {
    let piece = pieces.iter().find(|p| at <= p.offset + p.text.len())?;
    let local = at.saturating_sub(piece.offset);
    let run = piece.runs.iter().find(|r| local < r.shown.end).or(piece.runs.last())?;
    if local <= run.shown.start {
        return Some(run.src.start);
    }
    Some(mapped(run, local))
}

/// Where the markdown of what's selected up to `at` ends.
fn source_end(pieces: &[Piece], at: usize) -> Option<usize> {
    // The piece it's in, or (at the start of one) the one before's end.
    let piece = pieces.iter().rev().find(|p| p.offset < at || (p.offset == at && at == 0))?;
    let local = (at - piece.offset).min(piece.text.len());
    let run = piece.runs.iter().rev().find(|r| local > r.shown.start).or(piece.runs.first())?;
    if local >= run.shown.end {
        return Some(run.src.end);
    }
    Some(mapped(run, local))
}

/// A byte of a run as shown, where it is in the markdown (one for one; a
/// run whose markdown is longer, as an escape's, as far in as it can).
fn mapped(run: &Run, local: usize) -> usize {
    (run.src.start + (local - run.shown.start)).min(run.src.end)
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
    /// The next id for a code block's run (past the parsed runs').
    next_id: std::cell::Cell<usize>,
}

impl Draw<'_> {
    /// A piece is drawn: its offset (the next starts a line on), and what
    /// of it is selected.
    fn piece(&self, text: &str, runs: Vec<Run>, layout: gpui::TextLayout) -> Option<Range<usize>> {
        let offset = self.next.get();
        self.next.set(offset + text.len() + 1);
        let piece = Piece { offset, text: text.to_owned(), layout, runs };
        self.pieces.0.borrow_mut().0.push(piece);
        let selected = self.selected.clone()?;
        let start = selected.start.max(offset);
        let end = selected.end.min(offset + text.len());
        // (A line break selected through: its piece's end shows it.)
        (start < end || (start == end && selected.start < offset && selected.end > end))
            .then(|| start - offset..end - offset)
    }
}

/// `blocks` drawn, a column of them; `id`: unique among what's drawn (for
/// links' clicks). The pieces drawn go to `pieces`, with `source` (the
/// markdown); `selected`: what's selected, as offsets in the message's text.
pub fn view(
    doc: &Doc,
    source: &str,
    look: &Look,
    id: &str,
    pieces: &Pieces,
    selected: Option<Range<usize>>,
) -> gpui::Div {
    {
        let mut inner = pieces.0.borrow_mut();
        inner.0.clear();
        if inner.1 != source {
            inner.1 = source.to_owned();
        }
        inner.2.clone_from(&doc.groups);
    }
    let draw = Draw {
        look,
        pieces: pieces.clone(),
        selected,
        next: std::cell::Cell::new(0),
        next_id: std::cell::Cell::new(usize::MAX / 2),
    };
    blocks_view(&doc.blocks, &draw, id)
}

fn blocks_view(blocks: &[Block], draw: &Draw, id: &str) -> gpui::Div {
    div().flex().flex_col().gap(px(6.)).children(
        blocks.iter().enumerate().map(|(i, block)| block_view(block, draw, &format!("{id}-{i}"))),
    )
}

/// A code block's line as runs, colored as its language has it; their
/// ids from `next_id`.
fn code_spans(line: &CodeLine, tokens: &[(Range<usize>, Kind)], next_id: &mut usize) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut at = 0;
    let push = |range: Range<usize>, kind: Option<Kind>, spans: &mut Vec<Span>| {
        if range.is_empty() {
            return;
        }
        spans.push(Span {
            text: line.text[range.clone()].to_owned(),
            code: true,
            kind,
            src: line.src + range.start..line.src + range.end,
            ..Span::default()
        });
    };
    for (range, kind) in tokens {
        push(at..range.start, None, &mut spans);
        push(range.clone(), Some(*kind), &mut spans);
        at = range.end;
    }
    push(at..line.text.len(), None, &mut spans);
    if spans.is_empty() {
        // (An empty line: still a run, for where it is.)
        spans.push(Span { code: true, src: line.src..line.src, ..Span::default() });
    }
    for span in &mut spans {
        span.id = *next_id;
        *next_id += 1;
    }
    spans
}

/// A code block's lines as runs, and the group of its fences around them.
fn code_lines(
    lines: &[CodeLine],
    src: &Range<usize>,
    colors: &[Vec<(Range<usize>, Kind)>],
    next_id: &mut usize,
) -> (Vec<Vec<Span>>, Option<Group>) {
    let spans: Vec<Vec<Span>> = lines
        .iter()
        .enumerate()
        .map(|(n, line)| code_spans(line, colors.get(n).map_or(&[][..], Vec::as_slice), next_id))
        .collect();
    let first = spans.first().and_then(|l| l.first()).map(|s| s.id);
    let last = spans.last().and_then(|l| l.last()).map(|s| s.id);
    let group = first.zip(last).map(|(first, last)| Group { src: src.clone(), first, last });
    (spans, group)
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
        Block::Code { lang, lines, src } => {
            let group = SharedString::from(format!("{id}-code"));
            let copied: String =
                lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n");
            let code: String = copied.clone();
            let colors = highlight::highlight(lang, &code);
            let mut next_id = draw.next_id.get();
            let (line_spans, fences) = code_lines(lines, src, &colors, &mut next_id);
            draw.next_id.set(next_id);
            // Copied whole, its fences with it.
            if let Some(fences) = fences {
                draw.pieces.0.borrow_mut().2.push(fences);
            }
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
                .children(line_spans.iter().enumerate().map(|(n, spans)| {
                    // (An empty line keeps its height.)
                    div().min_h(px(16.)).child(text_view(spans, draw, &format!("{id}-{n}"), true))
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
    let mut copied = Vec::new();
    let mut links: Vec<(Range<usize>, String)> = Vec::new();
    for span in spans {
        let start = text.len();
        text.push_str(&span.text);
        copied.push(Run { shown: start..text.len(), src: span.src.clone(), id: span.id });
        if span.text.is_empty() {
            continue;
        }
        let mut font = gpui::font(if span.code { look.mono.clone() } else { look.font.clone() });
        if span.bold {
            font.weight = FontWeight::BOLD;
        }
        if span.italic {
            font.style = FontStyle::Italic;
        }
        let color = match (&span.link, span.kind) {
            (Some(_), _) => look.link,
            (None, Some(kind)) => (look.colors)(kind),
            (None, None) => look.text,
        };
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
    let selected = draw.piece(&text, copied, layout);
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
        let Block::Code { lang, lines, src } = &blocks[1] else { panic!("{:?}", blocks[1]) };
        assert_eq!(lang, "rust");
        let shown: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(shown, ["fn main() {}", "", "let x = 1;"]);
        assert_eq!(&text[src.clone()], "```rust\nfn main() {}\n\nlet x = 1;\n```");
        assert_eq!(&text[lines[2].src..lines[2].src + 10], "let x = 1;");
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
    fn plain_text_and_what_ends_early() {
        let blocks = parse("just words");
        let [Block::Paragraph(spans)] = blocks.as_slice() else { panic!("{blocks:?}") };
        assert_eq!(plain(spans), "just words");
        // A message cut off inside a code block still shows what it has.
        let blocks = parse("```\nhalf");
        let [Block::Code { lines, .. }] = blocks.as_slice() else { panic!("{blocks:?}") };
        assert_eq!(lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), ["half"]);
        assert!(parse("").is_empty());
    }

    /// `text`'s pieces as drawing makes them (without drawing: no layouts).
    fn pieces_of(text: &str) -> Pieces {
        let pieces = Pieces::default();
        let doc = parse_doc(text);
        pieces.0.borrow_mut().1 = text.to_owned();
        pieces.0.borrow_mut().2 = doc.groups.clone();
        let mut offset = 0;
        let mut add = |spans: &[Span]| {
            let mut shown = String::new();
            let mut runs = Vec::new();
            for span in spans {
                let start = shown.len();
                shown.push_str(&span.text);
                runs.push(Run { shown: start..shown.len(), src: span.src.clone(), id: span.id });
            }
            let len = shown.len();
            pieces.0.borrow_mut().0.push(Piece {
                offset,
                text: shown,
                layout: gpui::TextLayout::default(),
                runs,
            });
            offset += len + 1;
        };
        let mut next_id = usize::MAX / 2;
        let mut code_groups = Vec::new();
        fn walk(
            blocks: &[Block],
            add: &mut dyn FnMut(&[Span]),
            next_id: &mut usize,
            groups: &mut Vec<Group>,
        ) {
            for block in blocks {
                match block {
                    Block::Paragraph(s) | Block::Heading(_, s) => add(s),
                    Block::Code { lines, src, .. } => {
                        let (spans, group) = code_lines(lines, src, &[], next_id);
                        spans.iter().for_each(|s| add(s));
                        groups.extend(group);
                    },
                    Block::List { items, .. } => {
                        items.iter().for_each(|i| walk(&i.blocks, add, next_id, groups))
                    },
                    Block::Quote(blocks) => walk(blocks, add, next_id, groups),
                    Block::Table { head, rows } => {
                        head.iter().for_each(|c| add(c));
                        rows.iter().flatten().for_each(|c| add(c));
                    },
                    Block::Rule => {},
                }
            }
        }
        walk(&doc.blocks, &mut add, &mut next_id, &mut code_groups);
        pieces.0.borrow_mut().2.extend(code_groups);
        pieces
    }

    #[test]
    fn selected_text_across_pieces() {
        let pieces = pieces_of("Fixed the bug.\n\ncargo test\n\n한글 too");
        assert_eq!(pieces.len(), 26 + "한글 too".len());
        assert_eq!(pieces.text(6..9), "the");
        // Over a piece's end: its line ends, the next goes on.
        assert_eq!(pieces.text(10..20), "bug.\ncargo");
        assert_eq!(pieces.text(0..pieces.len()), "Fixed the bug.\ncargo test\n한글 too");
        // (Inside a character: from its start.)
        assert_eq!(pieces.text(27..32), "한글");
    }

    #[test]
    fn copied_as_written() {
        let text = "Fix **the login** and `next_url()`, see [docs](https://x.io).\n\n- one\n- two\n\n```rust\nlet x = 1;\n```";
        let pieces = pieces_of(text);
        let shown = pieces.text(0..pieces.len());
        let at = |s: &str| shown.find(s).unwrap();
        let md = |from: &str, to: &str| pieces.markdown(at(from)..at(to) + to.len());
        // Part of a bold run: its text alone; all of it: its markup too.
        assert_eq!(md("login", "login"), "login");
        assert_eq!(md("the login", "the login"), "**the login**");
        assert_eq!(md("Fix", "next_url()"), "Fix **the login** and `next_url()`");
        assert_eq!(md("docs", "docs"), "[docs](https://x.io)");
        // Whole list items: their markers; a whole code block: its fences.
        assert_eq!(md("one", "two"), "- one\n- two");
        assert_eq!(md("let x", "= 1;"), "```rust\nlet x = 1;\n```");
        // Part of a code line: as it is.
        assert_eq!(md("x = 1", "x = 1"), "x = 1");
        // Everything: the whole message.
        assert_eq!(pieces.markdown(0..pieces.len()), text);
    }
}
