ALTER TABLE sessions ADD COLUMN display_name TEXT;
ALTER TABLE sessions ADD COLUMN display_name_updated_ns INTEGER;

-- A hook move and an administrative move share this boundary. A label
-- captured in the old project may contain information private to it.
CREATE TRIGGER sessions_clear_display_name_on_scope_change
AFTER UPDATE OF workspace_id, project_id ON sessions
WHEN OLD.workspace_id != NEW.workspace_id OR OLD.project_id != NEW.project_id
BEGIN
    UPDATE sessions SET display_name = NULL, display_name_updated_ns = NULL WHERE id = NEW.id;
END;
