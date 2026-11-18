//! Source-preserving windows with explicit session and turn provenance.

use std::collections::HashSet;
use std::ops::Range;

use anyhow::{Result, bail};
use tokenizers::Tokenizer;

use super::super::dataset::Question;
use super::super::ingest::stored_session_uuid;

pub(super) struct Source {
    pub id: uuid::Uuid,
    pub title: String,
    pub text: String,
    turns: Vec<Range<usize>>,
}

pub(super) struct Chunk {
    pub source: usize,
    pub tokens: Range<usize>,
    pub bytes: Range<usize>,
}

/// Only haystack fields enter the index; gold answers and evidence flags
/// are used later by the existing scorer, never by retrieval.
pub(super) fn sources(question: &Question) -> Result<Vec<Source>> {
    if question.haystack_session_ids.len() != question.haystack_sessions.len()
        || question.haystack_dates.len() != question.haystack_sessions.len()
    {
        bail!("haystack session ids, dates, and turns have different lengths");
    }
    let mut seen = HashSet::new();
    let mut sources = Vec::new();
    for (index, id) in question.haystack_session_ids.iter().enumerate() {
        if !seen.insert(id) {
            continue;
        }
        let date = &question.haystack_dates[index];
        let mut text = String::new();
        let mut turns = Vec::new();
        for turn in &question.haystack_sessions[index] {
            let start = text.len();
            text.push_str(&format!("[Source session date: {date}]\n[{}] ", turn.role));
            text.push_str(&turn.content);
            text.push_str("\n\n");
            turns.push(start..text.len());
        }
        sources.push(Source {
            id: stored_session_uuid(question, id),
            title: format!("session {id}, {date}"),
            text,
            turns,
        });
    }
    Ok(sources)
}

pub(super) fn windows(
    sources: &[Source],
    tokenizer: &Tokenizer,
    size: usize,
    overlap: usize,
) -> Result<Vec<Chunk>> {
    if size == 0 || overlap >= size {
        bail!("chunk tokens must be positive and overlap must be smaller than the chunk");
    }
    let mut chunks = Vec::new();
    for (source, document) in sources.iter().enumerate() {
        let encoding = tokenizer
            .encode(document.text.as_str(), false)
            .map_err(|e| anyhow::anyhow!("tokenizing source: {e}"))?;
        let offsets = encoding.get_offsets();
        let mut start = 0;
        while start < offsets.len() {
            let end = (start + size).min(offsets.len());
            let bytes = offsets[start].0..offsets[end - 1].1;
            if bytes.is_empty() || document.text.get(bytes.clone()).is_none() {
                bail!("tokenizer returned invalid UTF-8 source offsets");
            }
            chunks.push(Chunk {
                source,
                tokens: start..end,
                bytes,
            });
            if end == offsets.len() {
                break;
            }
            start += size - overlap;
        }
    }
    Ok(chunks)
}

impl Chunk {
    pub(super) fn text<'a>(&self, sources: &'a [Source]) -> &'a str {
        &sources[self.source].text[self.bytes.clone()]
    }

    pub(super) fn evidence<'a>(&self, sources: &'a [Source], expand: bool) -> &'a str {
        let source = &sources[self.source];
        if !expand {
            return self.text(sources);
        }
        let first = source.turns.iter().find(|turn| turn.end > self.bytes.start);
        let last = source
            .turns
            .iter()
            .rev()
            .find(|turn| turn.start < self.bytes.end);
        match (first, last) {
            (Some(first), Some(last)) => &source.text[first.start..last.end],
            _ => self.text(sources),
        }
    }
}

/// Cosine over every window in this question's own index. Ties retain
/// source order, so identical scores do not acquire hash-map noise.
pub(super) fn dense_order(query: &[f32], vectors: &[Vec<f32>], limit: usize) -> Result<Vec<usize>> {
    let norm = |vector: &[f32]| -> Result<f64> {
        if vector.is_empty() || vector.iter().any(|value| !value.is_finite()) {
            bail!("empty or non-finite dense vector");
        }
        let norm = vector
            .iter()
            .map(|&v| f64::from(v).powi(2))
            .sum::<f64>()
            .sqrt();
        if norm == 0.0 {
            bail!("zero dense vector");
        }
        Ok(norm)
    };
    let query_norm = norm(query)?;
    let mut scores = Vec::new();
    for (index, vector) in vectors.iter().enumerate() {
        if query.len() != vector.len() {
            bail!("dense vector dimensions differ");
        }
        let denominator = query_norm * norm(vector)?;
        let dot = query
            .iter()
            .zip(vector)
            .map(|(&a, &b)| f64::from(a) * f64::from(b))
            .sum::<f64>();
        scores.push((index, dot / denominator));
    }
    scores.sort_by(|a, b| b.1.total_cmp(&a.1));
    scores.truncate(limit);
    Ok(scores.into_iter().map(|(index, _)| index).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokenizers::models::wordlevel::WordLevel;
    use tokenizers::pre_tokenizers::whitespace::WhitespaceSplit;

    fn tokenizer() -> Tokenizer {
        let model = WordLevel::builder()
            .vocab([("[UNK]".into(), 0)].into_iter().collect())
            .unk_token("[UNK]".into())
            .build()
            .unwrap();
        let mut tokenizer = Tokenizer::new(model);
        tokenizer.with_pre_tokenizer(Some(WhitespaceSplit));
        tokenizer
    }

    fn source(text: &str, turns: Vec<Range<usize>>) -> Source {
        Source {
            id: uuid::Uuid::nil(),
            title: "fixture".into(),
            text: text.into(),
            turns,
        }
    }

    #[test]
    fn windows_overlap_without_gaps_and_preserve_source_bytes() {
        let text = "Árvore  São\nPaulo C D E F G";
        let sources = [source(text, std::iter::once(0..text.len()).collect())];
        let chunks = windows(&sources, &tokenizer(), 3, 1).unwrap();
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.tokens.clone())
                .collect::<Vec<_>>(),
            [0..3, 2..5, 4..7, 6..8]
        );
        assert_eq!(chunks[0].text(&sources), "Árvore  São\nPaulo");
        assert_eq!(chunks[1].text(&sources), "Paulo C D");
        assert_eq!(chunks[3].text(&sources), "F G");
        for chunk in &chunks {
            assert_eq!(chunk.evidence(&sources, true), text);
        }
        assert!(windows(&sources, &tokenizer(), 0, 0).is_err());
        assert!(windows(&sources, &tokenizer(), 3, 3).is_err());
    }

    #[test]
    fn expansion_opens_intersecting_turns_and_never_crosses_sessions() {
        let sources = [
            source("A B\nC D", vec![0..4, 4..7]),
            source("E F", std::iter::once(0..3).collect()),
        ];
        let chunks = windows(&sources, &tokenizer(), 2, 1).unwrap();
        assert_eq!(chunks[1].text(&sources), "B\nC");
        assert_eq!(chunks[1].evidence(&sources, true), "A B\nC D");
        assert_eq!(chunks[1].evidence(&sources, false), "B\nC");
        assert_eq!(chunks[3].evidence(&sources, true), "E F");
    }

    #[test]
    fn cosine_is_normalized_stable_and_rejects_invalid_vectors() {
        let vectors = vec![vec![1.0, 0.0], vec![2.0, 0.0], vec![0.0, 3.0]];
        assert_eq!(dense_order(&[1.0, 0.0], &vectors, 2).unwrap(), [0, 1]);
        assert!(dense_order(&[1.0], &vectors, 2).is_err());
        assert!(dense_order(&[0.0, 0.0], &vectors, 2).is_err());
        assert!(dense_order(&[f32::NAN, 1.0], &vectors, 2).is_err());
        assert!(dense_order(&[1.0], &[vec![f32::INFINITY]], 2).is_err());
    }

    #[test]
    fn changing_gold_labels_does_not_change_the_index() {
        let bytes = include_bytes!("../../../fixtures/retrieval/coding-v1.json");
        let mut questions: Vec<Question> = serde_json::from_slice(bytes).unwrap();
        let before = sources(&questions[0]).unwrap();
        questions[0].answer = serde_json::json!("DO NOT INDEX THIS GOLD ANSWER");
        questions[0].answer_session_ids.clear();
        for session in &mut questions[0].haystack_sessions {
            for turn in session {
                turn.has_answer = Some(!turn.is_evidence());
            }
        }
        let after = sources(&questions[0]).unwrap();
        assert_eq!(
            before.iter().map(|s| (&s.id, &s.text)).collect::<Vec<_>>(),
            after.iter().map(|s| (&s.id, &s.text)).collect::<Vec<_>>()
        );
        questions[0].haystack_dates.clear();
        assert!(sources(&questions[0]).is_err());
    }
}
