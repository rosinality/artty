//! Fuzzy matching for the palette: each word of a query must appear in the
//! text in order (not necessarily together), ignoring case; matches at the
//! start of words, and runs of letters, score higher.

/// How well `query` matches `text`, or `None` if it doesn't.
pub fn score(query: &str, text: &str) -> Option<i32> {
    let text: Vec<char> = text.chars().collect();
    let lower: Vec<char> = text.iter().flat_map(|c| c.to_lowercase()).collect();
    if lower.len() != text.len() {
        // (Letters whose lowercase is longer: compare as they are.)
        return words_score(query, &text, &text);
    }
    words_score(query, &text, &lower)
}

fn words_score(query: &str, text: &[char], lower: &[char]) -> Option<i32> {
    let mut total = 0;
    for word in query.split_whitespace() {
        total += word_score(&word.to_lowercase().chars().collect::<Vec<_>>(), text, lower)?;
    }
    // Shorter texts match more closely.
    Some(total - text.len() as i32 / 8)
}

fn word_score(word: &[char], text: &[char], lower: &[char]) -> Option<i32> {
    if word.is_empty() {
        return Some(0);
    }
    // Each possible start, the best kept: greedy from there.
    let mut best = None;
    for start in (0..lower.len()).filter(|&i| lower[i] == word[0]) {
        let mut score = 0;
        let mut at = start;
        let mut previous: Option<usize> = None;
        let mut matched = true;
        for &c in word {
            let Some(found) = (at..lower.len()).find(|&i| lower[i] == c) else {
                matched = false;
                break;
            };
            score += 10;
            if word_start(text, found) {
                score += 15;
            }
            match previous {
                Some(p) if found == p + 1 => score += 8,
                Some(p) => score -= (found - p - 1).min(10) as i32,
                None => {},
            }
            previous = Some(found);
            at = found + 1;
        }
        if matched {
            best = best.max(Some(score));
        }
    }
    best
}

/// Whether `text[i]` begins a word: the first letter, one after a
/// separator, or a capital after a small letter (camelCase).
fn word_start(text: &[char], i: usize) -> bool {
    let Some(&before) = i.checked_sub(1).and_then(|b| text.get(b)) else { return true };
    !before.is_alphanumeric() || before.is_lowercase() && text[i].is_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_in_order_ignoring_case() {
        assert!(score("srv", "server").is_some());
        assert!(score("SRV", "server").is_some());
        assert!(score("rvs", "server").is_none());
        assert!(score("", "anything").is_some());
    }

    #[test]
    fn every_word_somewhere() {
        assert!(score("api claude", "claude · api-server · ~/src/api").is_some());
        assert!(score("api codex", "claude · api-server").is_none());
    }

    #[test]
    fn word_starts_and_runs_rank_higher() {
        let starts = score("ns", "New Split").unwrap();
        let inside = score("ns", "lines").unwrap();
        assert!(starts > inside, "{starts} vs {inside}");
        let run = score("test", "cargo test").unwrap();
        let scattered = score("test", "the extra sentinel").unwrap();
        assert!(run > scattered);
        // camelCase counts as words.
        assert!(score("tf", "ToggleFiles").unwrap() > score("tf", "outfit").unwrap());
    }
}
