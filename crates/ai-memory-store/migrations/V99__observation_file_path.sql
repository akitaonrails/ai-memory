-- Opt-in per-file index (#1843): the checkout-relative path of the one file a
-- kept file-tool call named, set only when `[ingest] index_file_paths` is on.
-- Existing rows read NULL; the column follows the row through every lifecycle
-- operation, so prune, purge, move and merge need no new code.
ALTER TABLE observations ADD COLUMN file_path TEXT;

CREATE INDEX idx_observations_file_path
    ON observations(workspace_id, project_id, file_path, created_at)
    WHERE file_path IS NOT NULL;
