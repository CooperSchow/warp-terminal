-- The Codex CLI conversation a pane was running at save time, so a restored tab
-- reopens it with `codex resume <id>` instead of coming back a blank shell.
--
-- A nullable column alongside `claude_session_id` rather than a widening of it:
-- a build that predates this column selects an explicit column list, so it
-- ignores this one, and inserts leave it NULL. That makes a rollback real — an
-- older build restores a Codex tab as a plain shell rather than mistaking a
-- Codex id for a Claude one and running the wrong agent in that tab.
ALTER TABLE terminal_panes ADD COLUMN codex_session_id TEXT;
