-- A regenerated draft revised from one of the drafts it was shown: the
-- UI can say "revised from #N".
ALTER TABLE drafts ADD COLUMN based_on INTEGER REFERENCES drafts (id);
