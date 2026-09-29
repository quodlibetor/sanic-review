-- Why a resumed push review dismissed the draft as obsolete. A pending
-- draft it dismisses is `dismissed` until you restore it; one you
-- accepted or edited keeps its status, flagged, until you clear this.
ALTER TABLE drafts ADD COLUMN obsolete TEXT;
