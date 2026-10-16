-- Rule evaluation over benefit cases: the latest case projection and its parties, and findings.

-- Latest workbench.case.v1 seen per case.
CREATE TABLE inference.cases (
    case_id              uuid PRIMARY KEY,
    receipt_number       text        NOT NULL,
    form_type            text        NOT NULL,
    status               text        NOT NULL,
    -- Primary document; inference.findings.v1 is stored on it.
    document_id          uuid        NOT NULL,
    case_artifact_id     uuid        NOT NULL,
    case_revision        int         NOT NULL,
    -- Bumped when the case's findings change; written_revision trails it until
    -- inference.findings.v1 is stored.
    findings_revision    int         NOT NULL DEFAULT 0,
    written_revision     int         NOT NULL DEFAULT 0,
    findings_artifact_id uuid,
    evaluated_at         timestamptz,
    updated_at           timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX cases_unwritten_idx ON inference.cases (case_id) WHERE written_revision < findings_revision;

-- Parties of each case from its latest projection; the cross-case identity index.
CREATE TABLE inference.parties (
    case_id       uuid NOT NULL REFERENCES inference.cases (case_id) ON DELETE CASCADE,
    role          text NOT NULL CHECK (role IN ('applicant', 'beneficiary')),
    family_name   text,
    given_name    text,
    -- 9 digits, as normalized in case facts.
    a_number      text,
    -- MM/DD/YYYY, as normalized in case facts.
    date_of_birth text,
    PRIMARY KEY (case_id, role)
);
CREATE INDEX parties_a_number_idx ON inference.parties (a_number) WHERE a_number IS NOT NULL;

-- One open finding per (rule, case, key). A finding that stops firing is resolved; if it fires
-- again a new row is raised.
CREATE TABLE inference.findings (
    id            uuid PRIMARY KEY,
    rule          text        NOT NULL,
    case_id       uuid        NOT NULL REFERENCES inference.cases (case_id),
    -- Rule-defined identity within the case, e.g. the A-Number.
    key           text        NOT NULL,
    severity      text        NOT NULL CHECK (severity IN ('low', 'medium', 'high')),
    status        text        NOT NULL CHECK (status IN ('open', 'resolved')),
    summary       text        NOT NULL,
    evidence      jsonb       NOT NULL,
    -- Other cases involved.
    related_cases uuid[]      NOT NULL DEFAULT '{}',
    -- Reviewer disposition; cleared when related_cases changes.
    review        text CHECK (review IN ('confirmed', 'dismissed')),
    reviewed_by   text,
    reviewed_at   timestamptz,
    review_note   text,
    raised_at     timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),
    resolved_at   timestamptz,
    CHECK ((status = 'resolved') = (resolved_at IS NOT NULL))
);
CREATE UNIQUE INDEX findings_one_open ON inference.findings (rule, case_id, key) WHERE status = 'open';
CREATE INDEX findings_case_idx ON inference.findings (case_id, raised_at);
CREATE INDEX findings_open_idx ON inference.findings (raised_at) WHERE status = 'open';
CREATE INDEX findings_related_idx ON inference.findings USING gin (related_cases) WHERE status = 'open';

-- Service state, e.g. the fingerprint of the rules last applied to every case.
CREATE TABLE inference.meta (
    key   text PRIMARY KEY,
    value text NOT NULL
);
