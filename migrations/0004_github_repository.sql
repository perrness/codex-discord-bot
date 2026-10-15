-- Optional explicit mapping; otherwise discovery validates the current origin.
ALTER TABLE repositories ADD COLUMN github_repository text;
