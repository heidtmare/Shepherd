-- Assignment workloads: workers and their skills, automatic assignment rules, workload requests.

-- Officers who can be assigned cases. A worker's load is their undecided assigned cases.
CREATE TABLE workbench.workers (
    worker      text PRIMARY KEY,
    -- Load at which automatic assignment stops. Managers may exceed it with `force`.
    max_cases   int         NOT NULL CHECK (max_cases >= 0),
    -- Receives cases from assignment rules, including a replacement when a case is decided.
    auto_assign boolean     NOT NULL DEFAULT true,
    updated_by  text,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
);

-- Benefit form types a worker may adjudicate. Every assignment requires the skill.
CREATE TABLE workbench.worker_skills (
    worker    text NOT NULL REFERENCES workbench.workers (worker) ON DELETE CASCADE,
    form_type text NOT NULL,
    PRIMARY KEY (worker, form_type)
);
CREATE INDEX worker_skills_form_idx ON workbench.worker_skills (form_type);

-- Unassigned cases of `form_type` go to the least-loaded eligible worker of the first matching
-- enabled rule (lowest priority first).
CREATE TABLE workbench.assignment_rules (
    id         uuid PRIMARY KEY,
    name       text        NOT NULL,
    form_type  text        NOT NULL,
    -- Eligible workers; empty = every worker skilled for form_type.
    workers    text[]      NOT NULL DEFAULT '{}',
    priority   int         NOT NULL DEFAULT 100,
    enabled    boolean     NOT NULL DEFAULT true,
    created_by text        NOT NULL,
    updated_by text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX assignment_rules_form_idx ON workbench.assignment_rules (form_type, priority) WHERE enabled;

-- Worker requests to add a case to, or remove one from, their workload; resolved by a manager.
CREATE TABLE workbench.workload_requests (
    id              uuid PRIMARY KEY,
    case_id         uuid        NOT NULL REFERENCES workbench.cases (id),
    worker          text        NOT NULL,
    kind            text        NOT NULL CHECK (kind IN ('add', 'remove')),
    reason          text,
    status          text        NOT NULL CHECK (status IN ('pending', 'approved', 'denied', 'withdrawn', 'cancelled')),
    created_at      timestamptz NOT NULL DEFAULT now(),
    resolved_by     text,
    resolved_at     timestamptz,
    resolution_note text
);
CREATE UNIQUE INDEX workload_requests_one_pending ON workbench.workload_requests (case_id, worker, kind)
    WHERE status = 'pending';
CREATE INDEX workload_requests_worker_idx ON workbench.workload_requests (worker, created_at);
CREATE INDEX workload_requests_pending_idx ON workbench.workload_requests (created_at) WHERE status = 'pending';

-- Workers a case was returned to the pool from (approved removal request or manager unassign).
-- Rules do not assign the case to them again; a manager still can.
CREATE TABLE workbench.assignment_exclusions (
    case_id    uuid        NOT NULL REFERENCES workbench.cases (id),
    worker     text        NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (case_id, worker)
);

CREATE INDEX cases_backlog_idx ON workbench.cases (form_type, received_date, created_at)
    WHERE status = 'open' AND assignee IS NULL;
