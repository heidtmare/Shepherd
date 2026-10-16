-- Administrative closure: a terminal outcome without adjudication.
ALTER TABLE workbench.cases DROP CONSTRAINT cases_decision_check;
ALTER TABLE workbench.cases ADD CONSTRAINT cases_decision_check
    CHECK (decision IN ('approved', 'denied', 'withdrawn', 'closed'));

-- One primary document per case; the primary document can be replaced by another case document.
CREATE UNIQUE INDEX case_documents_one_primary ON workbench.case_documents (case_id) WHERE role = 'primary';
