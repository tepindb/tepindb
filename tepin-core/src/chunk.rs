//! Built-in chunking: long embed text is split into overlapping chunks and
//! every chunk gets its own vector, so users never implement chunking
//! themselves and long documents stop losing meaning to truncation.
//!
//! The chunker is deterministic and produces verbatim substrings of the
//! input (modulo trimming), which is what makes search snippets possible:
//! re-chunking the same text at query time yields exactly the chunk that
//! was embedded at write time. Cuts prefer paragraph, then sentence, then
//! word boundaries; a boundary-free run is hard-cut at the target size.

/// Target chunk size in characters. Sized well under bge-small's 512-token
/// window for typical English (~4 chars/token); dense scripts (CJK) may
/// still truncate inside a chunk, which the per-chunk `truncated` flag
/// reports loudly.
pub const CHUNK_TARGET_CHARS: usize = 1500;

/// Overlap between consecutive chunks, so meaning that straddles a cut is
/// fully present in at least one chunk.
pub const CHUNK_OVERLAP_CHARS: usize = 200;

/// Hard cap on chunks per document (~380 KB of text). The embed worker
/// marks the final kept chunk truncated when a document exceeds it.
pub const MAX_CHUNKS: usize = 256;

const PARA_LOOKBACK: usize = 300;
const SENTENCE_LOOKBACK: usize = 200;
const WORD_LOOKBACK: usize = 120;

/// Split text into embedding-sized chunks. Deterministic: same input,
/// same chunks — always. Short text comes back as a single chunk.
pub fn chunk_text(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    if n <= CHUNK_TARGET_CHARS {
        let t = text.trim();
        return if t.is_empty() {
            Vec::new()
        } else {
            vec![t.to_string()]
        };
    }

    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < n {
        let end_limit = start + CHUNK_TARGET_CHARS;
        if end_limit >= n {
            push_trimmed(&mut chunks, &chars[start..n]);
            break;
        }
        let cut = find_cut(&chars, start, end_limit);
        push_trimmed(&mut chunks, &chars[start..cut]);

        // Overlap: step back from the cut, then forward to a word start so
        // the next chunk never begins mid-word. No boundary → no overlap.
        let mut next = cut.saturating_sub(CHUNK_OVERLAP_CHARS).max(start + 1);
        while next < cut && !chars[next - 1].is_whitespace() {
            next += 1;
        }
        start = next.max(start + 1);
    }
    chunks
}

fn push_trimmed(chunks: &mut Vec<String>, chars: &[char]) {
    let s: String = chars.iter().collect();
    let t = s.trim();
    if !t.is_empty() {
        chunks.push(t.to_string());
    }
}

/// Best cut position in `(start, end_limit]`, searching backward from the
/// limit: paragraph break, then sentence end, then any whitespace, then a
/// hard cut at the limit.
fn find_cut(chars: &[char], start: usize, end_limit: usize) -> usize {
    let floor = |lookback: usize| end_limit.saturating_sub(lookback).max(start + 1);

    // Paragraph: cut after a newline that follows another newline.
    let para_floor = floor(PARA_LOOKBACK);
    for i in (para_floor..end_limit).rev() {
        if chars[i] == '\n' && i > 0 && chars[i - 1] == '\n' {
            return i + 1;
        }
    }
    // Sentence: cut after ./!/? followed by whitespace.
    let sent_floor = floor(SENTENCE_LOOKBACK);
    for i in (sent_floor..end_limit).rev() {
        if matches!(chars[i], '.' | '!' | '?')
            && chars.get(i + 1).is_some_and(|c| c.is_whitespace())
        {
            return i + 1;
        }
    }
    // Word: cut at whitespace so no word is split.
    let word_floor = floor(WORD_LOOKBACK);
    for i in (word_floor..end_limit).rev() {
        if chars[i].is_whitespace() {
            return i + 1;
        }
    }
    end_limit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_one_chunk_verbatim() {
        assert_eq!(chunk_text("hello world"), vec!["hello world"]);
        assert!(chunk_text("").is_empty());
        assert!(chunk_text("   \n  ").is_empty());
    }

    #[test]
    fn chunks_are_deterministic_and_sized() {
        let text = "The quick brown fox jumps over the lazy dog. ".repeat(200);
        let a = chunk_text(&text);
        let b = chunk_text(&text);
        assert_eq!(a, b);
        assert!(a.len() > 1);
        for c in &a {
            assert!(c.chars().count() <= CHUNK_TARGET_CHARS);
        }
    }

    #[test]
    fn every_chunk_is_a_substring_of_the_input() {
        let text =
            "Sentence one is here. Sentence two follows!\n\nA new paragraph starts. ".repeat(100);
        for c in chunk_text(&text) {
            assert!(text.contains(&c), "chunk must be verbatim input text");
        }
    }

    #[test]
    fn consecutive_chunks_overlap() {
        let text = "word ".repeat(2000);
        let chunks = chunk_text(&text);
        assert!(chunks.len() > 1);
        for pair in chunks.windows(2) {
            let tail: String = pair[0]
                .chars()
                .skip(pair[0].chars().count().saturating_sub(40))
                .collect();
            assert!(
                pair[1].starts_with(tail.split_whitespace().next().unwrap_or("")),
                "next chunk should re-cover the previous tail"
            );
        }
    }

    #[test]
    fn prefers_paragraph_boundaries() {
        // Paragraphs of ~700 chars: cuts should land between them, never inside.
        let para = "x".repeat(699);
        let text = [para.as_str(); 6].join("\n\n");
        let chunks = chunk_text(&text);
        for c in &chunks {
            assert!(
                c.split("\n\n").all(|p| p.len() == 699),
                "no partial paragraphs when paragraph cuts are available"
            );
        }
    }

    #[test]
    fn boundary_free_text_hard_cuts_and_covers_everything() {
        let text = "€".repeat(4000); // multibyte, no whitespace at all
        let chunks = chunk_text(&text);
        assert!(chunks.len() >= 3);
        let total: usize = chunks.iter().map(|c| c.chars().count()).sum();
        assert!(total >= 4000, "hard cuts must not lose text");
        for c in &chunks {
            assert!(c.chars().count() <= CHUNK_TARGET_CHARS);
        }
    }

    #[test]
    fn utf8_multibyte_never_panics() {
        let text = "héllo wörld — カタカナと漢字のテキスト。".repeat(300);
        let chunks = chunk_text(&text);
        assert!(!chunks.is_empty());
        let rejoined: String = chunks.concat();
        assert!(rejoined.contains("カタカナ"));
    }
}
