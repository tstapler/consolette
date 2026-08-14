//! Semantic near-duplicate elimination.
//!
//! Unlike [`crate::compression::text_compressor`]'s `dedup_consecutive_lines`
//! (which only collapses byte-identical *consecutive* lines), this stage
//! finds non-adjacent blocks of text that are near-duplicates of an earlier
//! block — e.g. the same error reported with a different timestamp or ID —
//! using 3-word shingle fingerprinting and Jaccard similarity, and replaces
//! later occurrences with a compact reference to the first.

use std::collections::HashSet;

const SHINGLE_WORDS: usize = 3;
const JACCARD_THRESHOLD: f64 = 0.8;

/// The set of 3-word shingles in `text`. Empty if `text` has fewer than
/// [`SHINGLE_WORDS`] words, which also makes it ineligible for matching.
fn shingles(text: &str) -> HashSet<String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() < SHINGLE_WORDS {
        return HashSet::new();
    }
    words
        .windows(SHINGLE_WORDS)
        .map(|w| w.join(" "))
        .collect()
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let intersection = a.intersection(b).count();
    let union = a.union(b).count();
    // Shingle-set sizes are bounded by block word counts (tool output, not
    // multi-gigabyte files), so usize -> f64 precision loss is not a concern.
    #[allow(clippy::cast_precision_loss)]
    let ratio = intersection as f64 / union as f64;
    ratio
}

/// Split `text` into blank-line-separated blocks and replace any block that
/// is a near-duplicate (Jaccard similarity over [`JACCARD_THRESHOLD`] on
/// 3-word shingles) of an earlier kept block with a compact reference.
/// Blocks under the shingle threshold are always kept verbatim. Returns the
/// input unchanged if nothing is deduplicated, or if doing so wouldn't
/// shrink the text.
#[must_use]
pub fn collapse_semantic_duplicates(text: &str) -> String {
    let blocks: Vec<&str> = text.split("\n\n").collect();
    if blocks.len() < 2 {
        return text.to_string();
    }

    let block_shingles: Vec<HashSet<String>> = blocks.iter().map(|&b| shingles(b)).collect();
    let mut out: Vec<String> = Vec::with_capacity(blocks.len());
    let mut kept_indices: Vec<usize> = Vec::new();

    for (i, block) in blocks.iter().enumerate() {
        if block_shingles[i].is_empty() {
            out.push((*block).to_string());
            kept_indices.push(i);
            continue;
        }

        let mut best: Option<(usize, f64)> = None;
        for &j in &kept_indices {
            let sim = jaccard(&block_shingles[i], &block_shingles[j]);
            if sim > JACCARD_THRESHOLD && best.is_none_or(|(_, best_sim)| sim > best_sim) {
                best = Some((j, sim));
            }
        }

        if let Some((j, sim)) = best {
            out.push(format!(
                "[near-duplicate of block {} ({:.0}% similar)]",
                j + 1,
                sim * 100.0
            ));
        } else {
            out.push((*block).to_string());
            kept_indices.push(i);
        }
    }

    let result = out.join("\n\n");
    if result.len() < text.len() {
        result
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapses_near_duplicate_block() {
        let text = "Error: connection to host db-primary-01 timed out while waiting for a reply after 30s at 12:00:01\n\n\
                     Error: connection to host db-primary-01 timed out while waiting for a reply after 30s at 12:00:47";
        let result = collapse_semantic_duplicates(text);
        assert!(result.contains(
            "Error: connection to host db-primary-01 timed out while waiting for a reply after 30s at 12:00:01"
        ));
        assert!(result.contains("[near-duplicate of block 1"));
        assert!(!result.contains("12:00:47"));
    }

    #[test]
    fn keeps_first_occurrence_verbatim() {
        let text = "alpha beta gamma delta epsilon\n\nalpha beta gamma delta zeta";
        let result = collapse_semantic_duplicates(text);
        assert!(result.starts_with("alpha beta gamma delta epsilon"));
    }

    #[test]
    fn does_not_collapse_dissimilar_blocks() {
        let text = "the quick brown fox jumps over the lazy dog\n\n\
                     completely unrelated content about something else entirely";
        assert_eq!(collapse_semantic_duplicates(text), text);
    }

    #[test]
    fn does_not_collapse_short_blocks() {
        let text = "ok\n\nok\n\nok";
        assert_eq!(collapse_semantic_duplicates(text), text);
    }

    #[test]
    fn single_block_is_noop() {
        let text = "just one block of several words here";
        assert_eq!(collapse_semantic_duplicates(text), text);
    }

    #[test]
    fn references_first_kept_occurrence_not_a_removed_one() {
        let text = "warning: retrying request to upstream service alpha after a transient network blip now\n\n\
                     warning: retrying request to upstream service alpha after a transient network blip then\n\n\
                     warning: retrying request to upstream service alpha after a transient network blip again";
        let result = collapse_semantic_duplicates(text);
        assert_eq!(result.matches("[near-duplicate of block 1").count(), 2);
    }
}
