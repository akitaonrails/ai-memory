//! Document-markdown → outline projection.
//!
//! ai-memory pages are *document* markdown (headings, multi-line
//! paragraphs, fenced code). Outl's own parser (`outl-md`) is an
//! outline-dialect parser: feeding it a document fragments paragraphs
//! line-by-line and loses blank lines inside fences (verified by
//! spike). So the backend projects documents into blocks itself, with
//! a deterministic, information-preserving grouping:
//!
//! - a fenced code block (``` … ```) becomes ONE block (fence kept);
//! - a heading line becomes one block;
//! - consecutive non-blank lines become one paragraph block
//!   (newlines kept inside the block text);
//! - blank lines only separate blocks.
//!
//! The reverse ([`specs_to_body`]) joins block texts with blank lines.
//! `specs_to_body(body_to_specs(body))` is byte-identical for bodies
//! already in "blocks separated by single blank lines" form and
//! semantically equivalent otherwise (runs of blank lines collapse).
//! The index keeps the ORIGINAL body regardless — this projection only
//! feeds the Outl view and the external-edit reconcile path.

use outl_actions::BlockTreeSpec;

fn block(text: String) -> BlockTreeSpec {
    BlockTreeSpec {
        text,
        children: Vec::new(),
    }
}

fn fence_block(opener: String, lines: Vec<&str>) -> BlockTreeSpec {
    let mut text = opener;
    for l in lines {
        text.push('\n');
        text.push_str(l);
    }
    block(text)
}

fn flush_paragraph(paragraph: &mut Vec<&str>, specs: &mut Vec<BlockTreeSpec>) {
    if !paragraph.is_empty() {
        specs.push(block(paragraph.join("\n")));
        paragraph.clear();
    }
}

/// Project a markdown document body into a flat forest of Outl blocks.
pub fn body_to_specs(body: &str) -> Vec<BlockTreeSpec> {
    let mut specs: Vec<BlockTreeSpec> = Vec::new();
    let mut paragraph: Vec<&str> = Vec::new();
    let mut fence: Option<(String, Vec<&str>)> = None;

    for line in body.lines() {
        if let Some((_, fence_lines)) = &mut fence {
            fence_lines.push(line);
            if is_fence_close(line) {
                let (opener, lines) = fence.take().expect("fence is Some in this branch");
                specs.push(fence_block(opener, lines));
            }
            continue;
        }

        if is_fence_open(line) {
            flush_paragraph(&mut paragraph, &mut specs);
            fence = Some((line.to_string(), Vec::new()));
            continue;
        }

        if line.trim().is_empty() {
            flush_paragraph(&mut paragraph, &mut specs);
            continue;
        }

        if line.starts_with('#') {
            flush_paragraph(&mut paragraph, &mut specs);
            specs.push(block(line.to_string()));
            continue;
        }

        paragraph.push(line);
    }

    // Unterminated fence: keep whatever we collected as one block so
    // no content is dropped.
    if let Some((opener, lines)) = fence.take() {
        specs.push(fence_block(opener, lines));
    }
    flush_paragraph(&mut paragraph, &mut specs);
    specs
}

/// Rebuild a document body from block texts (depth-first, children
/// after their parent). Used by the reconcile path when the user
/// edited the page inside Outl.
pub fn texts_to_body(texts: &[String]) -> String {
    texts.join("\n\n")
}

fn is_fence_open(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("```") || trimmed.starts_with("~~~")
}

fn is_fence_close(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed == "```" || trimmed == "~~~"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(specs: &[BlockTreeSpec]) -> Vec<String> {
        specs.iter().map(|s| s.text.clone()).collect()
    }

    #[test]
    fn groups_paragraphs_headings_and_fences() {
        let body =
            "# Title\n\nline one\nline two\n\n```rust\nfn x() {}\n\n    indented\n```\n\ntail";
        let specs = body_to_specs(body);
        assert_eq!(
            texts(&specs),
            vec![
                "# Title",
                "line one\nline two",
                "```rust\nfn x() {}\n\n    indented\n```",
                "tail",
            ]
        );
    }

    #[test]
    fn round_trip_is_stable_for_canonical_bodies() {
        let body = "# Title\n\npara one\nstill para one\n\n```py\nprint(1)\n```\n\nlast";
        let specs = body_to_specs(body);
        let rebuilt = texts_to_body(&texts(&specs));
        assert_eq!(rebuilt, body);
        // And projecting the rebuilt body again is a fixpoint.
        assert_eq!(texts(&body_to_specs(&rebuilt)), texts(&specs));
    }

    #[test]
    fn unterminated_fence_keeps_content() {
        let body = "```sh\necho hi";
        let specs = body_to_specs(body);
        assert_eq!(texts(&specs), vec!["```sh\necho hi"]);
    }

    #[test]
    fn blank_body_yields_no_blocks() {
        assert!(body_to_specs("").is_empty());
        assert!(body_to_specs("\n\n\n").is_empty());
    }
}
