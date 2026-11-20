//! Code blocks' colors, by syntect (Sublime Text's grammars for some 75
//! languages, the language found by a fence's name or extension): what it
//! makes of each piece of a line, narrowed to the few kinds the app colors
//! in its own palette (not a theme's).

use std::ops::Range;
use std::sync::OnceLock;

use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Keyword,
    String,
    Comment,
    Number,
    Type,
    Call,
    Added,
    Removed,
    Hunk,
}

/// The grammars, loaded once, when first needed.
fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// What a scope (as `string.quoted.double.rust`) is colored as, if
/// anything; the first that matches, of the scopes it's in, wins.
fn kind_of(scope: &str) -> Option<Kind> {
    const KINDS: &[(&str, Kind)] = &[
        ("markup.inserted", Kind::Added),
        ("markup.deleted", Kind::Removed),
        ("meta.diff.range", Kind::Hunk),
        ("meta.diff.header", Kind::Hunk),
        ("meta.mapping.key", Kind::Type),
        ("meta.structure.dictionary.key", Kind::Type),
        ("support.type.property-name", Kind::Type),
        ("comment", Kind::Comment),
        ("string", Kind::String),
        ("constant.numeric", Kind::Number),
        ("constant.character", Kind::String),
        ("constant.language", Kind::Keyword),
        ("keyword.operator", Kind::Keyword),
        ("keyword", Kind::Keyword),
        ("storage", Kind::Keyword),
        ("entity.name.function", Kind::Call),
        ("support.function", Kind::Call),
        ("variable.function", Kind::Call),
        ("entity.name.type", Kind::Type),
        ("entity.name.class", Kind::Type),
        ("entity.name.struct", Kind::Type),
        ("support.type", Kind::Type),
        ("support.class", Kind::Type),
    ];
    KINDS.iter().find(|(prefix, _)| scope.starts_with(prefix)).map(|(_, kind)| *kind)
}

/// The kind of what the scopes `stack` say: the outermost that's a diff's
/// or a key's (they color whole lines, whole keys), else the innermost
/// that's colored.
fn kind_of_stack(stack: &[Scope]) -> Option<Kind> {
    let kinds: Vec<Kind> = stack.iter().filter_map(|s| kind_of(&s.build_string())).collect();
    let whole = |k: &Kind| matches!(k, Kind::Added | Kind::Removed | Kind::Hunk | Kind::Type);
    kinds.iter().find(|k| whole(k)).or(kinds.last()).copied()
}

/// A code block's lines' colored ranges (bytes in each line), by its
/// language (a fence's info: `rust`, `py`, `ts`, `diff`, ...); none for a
/// language not known.
pub fn highlight(lang: &str, code: &str) -> Vec<Vec<(Range<usize>, Kind)>> {
    let lines = code.lines();
    let token = lang.split_whitespace().next().unwrap_or_default();
    let set = syntaxes();
    let syntax = (!token.is_empty()).then(|| set.find_syntax_by_token(token)).flatten();
    let Some(syntax) = syntax else { return lines.map(|_| Vec::new()).collect() };
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    lines
        .map(|line| {
            let with_end = format!("{line}\n");
            let ops = state.parse_line(&with_end, set).unwrap_or_default();
            let mut tokens: Vec<(Range<usize>, Kind)> = Vec::new();
            let mut push = |range: Range<usize>, stack: &ScopeStack| {
                let range = range.start.min(line.len())..range.end.min(line.len());
                if range.is_empty() {
                    return;
                }
                let Some(kind) = kind_of_stack(stack.as_slice()) else { return };
                match tokens.last_mut() {
                    Some((last, k)) if *k == kind && last.end == range.start => {
                        last.end = range.end
                    },
                    _ => tokens.push((range, kind)),
                }
            };
            let mut at = 0;
            for (index, op) in ops {
                if index > at {
                    push(at..index, &stack);
                    at = index;
                }
                // (A grammar's error: the rest as it was.)
                let _ = stack.apply(&op);
            }
            push(at..line.len(), &stack);
            tokens
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each line's (text, kind) pieces.
    fn kinds(lang: &str, code: &str) -> Vec<Vec<(String, Kind)>> {
        highlight(lang, code)
            .into_iter()
            .zip(code.lines())
            .map(|(tokens, line)| {
                tokens.into_iter().map(|(r, k)| (line[r].to_owned(), k)).collect()
            })
            .collect()
    }

    fn has(line: &[(String, Kind)], text: &str, kind: Kind) -> bool {
        line.iter().any(|(t, k)| t.trim() == text && *k == kind)
    }

    #[test]
    fn rust() {
        let found =
            kinds("rust", "fn next_url(p: &str) -> String { // the next\n    let n = 42; \"s\"\n}");
        assert!(has(&found[0], "fn", Kind::Keyword), "{found:?}");
        assert!(has(&found[0], "next_url", Kind::Call), "{found:?}");
        assert!(has(&found[0], "// the next", Kind::Comment), "{found:?}");
        assert!(has(&found[1], "let", Kind::Keyword), "{found:?}");
        assert!(has(&found[1], "42", Kind::Number), "{found:?}");
        assert!(has(&found[1], "\"s\"", Kind::String), "{found:?}");
    }

    #[test]
    fn lines_carried_over() {
        // A block comment's later line is still a comment.
        let found = kinds("js", "/* one\ntwo */ const x = 1");
        assert_eq!(
            found[1].first().map(|(t, k)| (t.as_str(), *k)),
            Some(("two */", Kind::Comment))
        );
        assert!(has(&found[1], "const", Kind::Keyword), "{found:?}");
        // (A docstring is a comment, to Python's grammar.)
        let py = kinds("py", "def f():\n    \"\"\"doc\n    more\"\"\"\n    return None");
        assert!(py[2].iter().all(|(_, k)| *k == Kind::Comment), "{py:?}");
        assert!(has(&py[3], "return", Kind::Keyword), "{py:?}");
    }

    #[test]
    fn diffs_keys_and_the_unknown() {
        let diff = kinds("diff", "@@ -1 +1 @@\n-old\n+new\n same");
        let shown: Vec<Option<Kind>> = diff.iter().map(|l| l.first().map(|(_, k)| *k)).collect();
        assert_eq!(shown, [Some(Kind::Hunk), Some(Kind::Removed), Some(Kind::Added), None]);
        let json = kinds("json", "{\"name\": \"artty\"}");
        assert!(
            has(&json[0], "\"name\"", Kind::Type) || has(&json[0], "name", Kind::Type),
            "{json:?}"
        );
        assert!(json[0].iter().any(|(t, k)| t.contains("artty") && *k == Kind::String), "{json:?}");
        // Not known, or none given: no colors.
        assert_eq!(kinds("nolang", "x = 1"), [Vec::new()]);
        assert_eq!(kinds("", "x = 1"), [Vec::new()]);
    }
}
