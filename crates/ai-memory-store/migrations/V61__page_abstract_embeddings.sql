-- L0 abstract embeddings (opt-in `[retrieval] abstract_vectors`).
--
-- A page whose frontmatter carries `abstract:` — the one-line summary the
-- consolidator writes, or a curator adds — gets that line embedded on its
-- own, separately from the body. A short abstract embeds far more sharply
-- than a multi-thousand-character body, so this table backs a fifth RRF
-- stream in hybrid search. Mirrors `page_embeddings`: one row per page,
-- keyed on the same `(provider, model, dim)` triple so the same
-- refuse-on-mismatch and stale-row rules apply.
CREATE TABLE page_abstract_embeddings (
    page_id     BLOB PRIMARY KEY NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    vector      BLOB NOT NULL,
    provider    TEXT NOT NULL,
    model       TEXT NOT NULL,
    dim         INTEGER NOT NULL CHECK (dim > 0),
    created_at  INTEGER NOT NULL
);

CREATE INDEX idx_page_abstract_embeddings_provider
    ON page_abstract_embeddings(provider, model, dim);
