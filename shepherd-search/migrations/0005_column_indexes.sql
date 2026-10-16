-- Card columns are declared in mapping.toml and their indexes are managed by `columns::ensure_indexes`
-- under the names `cards_col_<name>[_asc|_desc]_idx`. Rename the indexes created for the former
-- fixed columns so they are recognized instead of rebuilt.

ALTER INDEX IF EXISTS search.cards_status_idx        RENAME TO cards_col_status_idx;
ALTER INDEX IF EXISTS search.cards_amount_idx        RENAME TO cards_col_amount_idx;
ALTER INDEX IF EXISTS search.cards_received_idx      RENAME TO cards_col_received_at_idx;
ALTER INDEX IF EXISTS search.cards_amount_desc_idx   RENAME TO cards_col_amount_desc_idx;
ALTER INDEX IF EXISTS search.cards_amount_asc_idx    RENAME TO cards_col_amount_asc_idx;
ALTER INDEX IF EXISTS search.cards_received_desc_idx RENAME TO cards_col_received_at_desc_idx;
ALTER INDEX IF EXISTS search.cards_received_asc_idx  RENAME TO cards_col_received_at_asc_idx;
