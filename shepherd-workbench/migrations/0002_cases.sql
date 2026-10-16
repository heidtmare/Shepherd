-- Benefit request cases. One case per receipt number; a document belongs to at most one case.

CREATE SEQUENCE workbench.receipt_seq;

-- Amendments of stored form values are recorded as completed tasks.
ALTER TABLE workbench.tasks DROP CONSTRAINT tasks_kind_check;
ALTER TABLE workbench.tasks ADD CONSTRAINT tasks_kind_check CHECK (kind IN ('flagged', 'manual', 'amend'));

CREATE TABLE workbench.cases (
    id                   uuid PRIMARY KEY,
    receipt_number       text        NOT NULL UNIQUE,
    -- Benefit form type (template id listed in benefits.toml).
    form_type            text        NOT NULL,
    primary_document_id  uuid        NOT NULL,
    status               text        NOT NULL CHECK (status IN ('open', 'in_review', 'awaiting_response', 'decided')),
    assignee             text,
    assigned_at          timestamptz,
    -- Fact key -> value (see benefits::FACTS).
    facts                jsonb       NOT NULL DEFAULT '{}',
    -- Fact keys edited by an officer; not refreshed from form values.
    overridden           text[]      NOT NULL DEFAULT '{}',
    received_date        date        NOT NULL,
    -- Due date of the outstanding notice while awaiting_response.
    response_due         date,
    -- Criterion id -> {result, note, by, at}.
    criteria             jsonb       NOT NULL DEFAULT '{}',
    decision             text CHECK (decision IN ('approved', 'denied', 'withdrawn')),
    decided_by           text,
    decided_at           timestamptz,
    decision_artifact_id uuid,
    -- Latest workbench.case.v1 projection written to storage.
    case_artifact_id     uuid,
    -- Bumped on every change to projected state; indexed_revision trails it until written.
    revision             int         NOT NULL DEFAULT 1,
    indexed_revision     int         NOT NULL DEFAULT 0,
    created_at           timestamptz NOT NULL DEFAULT now(),
    updated_at           timestamptz NOT NULL DEFAULT now(),
    CHECK ((status = 'decided') = (decision IS NOT NULL))
);
CREATE INDEX cases_worklist_idx ON workbench.cases (status, received_date);
CREATE INDEX cases_assignee_idx ON workbench.cases (assignee) WHERE assignee IS NOT NULL;
CREATE INDEX cases_a_number_idx ON workbench.cases ((facts ->> 'a_number'));
CREATE INDEX cases_unindexed_idx ON workbench.cases (id) WHERE indexed_revision < revision;

CREATE TABLE workbench.case_documents (
    case_id     uuid        NOT NULL REFERENCES workbench.cases (id),
    document_id uuid        NOT NULL UNIQUE,
    role        text        NOT NULL CHECK (role IN ('primary', 'supporting', 'response')),
    filename    text,
    added_by    text,
    added_at    timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (case_id, document_id)
);

-- Requests for evidence and notices of intent to deny.
CREATE TABLE workbench.case_notices (
    id            uuid PRIMARY KEY,
    case_id       uuid        NOT NULL REFERENCES workbench.cases (id),
    kind          text        NOT NULL CHECK (kind IN ('rfe', 'noid')),
    items         jsonb       NOT NULL,
    message       text,
    issued_by     text        NOT NULL,
    issued_at     timestamptz NOT NULL DEFAULT now(),
    due_date      date        NOT NULL,
    artifact_id   uuid        NOT NULL,
    responded_at  timestamptz,
    response_note text,
    recorded_by   text
);
CREATE INDEX case_notices_case_idx ON workbench.case_notices (case_id, issued_at);

CREATE TABLE workbench.case_events (
    id      bigserial PRIMARY KEY,
    case_id uuid        NOT NULL REFERENCES workbench.cases (id),
    at      timestamptz NOT NULL DEFAULT now(),
    actor   text,
    action  text        NOT NULL,
    details jsonb       NOT NULL DEFAULT '{}'
);
CREATE INDEX case_events_case_idx ON workbench.case_events (case_id, id);
