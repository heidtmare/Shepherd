-- Rescinded notices and due-date extensions. A notice is closed by a response or a rescission.

ALTER TABLE workbench.case_notices
    -- Due date at issue; set by the first extension.
    ADD COLUMN original_due_date date,
    ADD COLUMN rescinded_at      timestamptz,
    ADD COLUMN rescinded_by      text,
    ADD COLUMN rescind_reason    text,
    ADD CONSTRAINT case_notices_closed_once CHECK (responded_at IS NULL OR rescinded_at IS NULL);

-- Overdue worklist.
CREATE INDEX cases_response_due_idx ON workbench.cases (response_due) WHERE status = 'awaiting_response';
