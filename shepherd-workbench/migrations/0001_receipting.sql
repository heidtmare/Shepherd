-- Receipting work queue. One active (open or in_progress) task per document.

CREATE TABLE workbench.tasks (
    id                  uuid PRIMARY KEY,
    document_id         uuid        NOT NULL,
    filename            text,
    -- Template id; NULL until chosen when recognition matched no template.
    form_type           text,
    kind                text        NOT NULL CHECK (kind IN ('flagged', 'manual')),
    status              text        NOT NULL CHECK (status IN ('open', 'in_progress', 'completed', 'cancelled')),
    source_artifact_id  uuid        NOT NULL,
    -- intake.form.v1 artifact under review; NULL for manual tasks without recognition.
    form_artifact_id    uuid,
    flagged_fields      int         NOT NULL DEFAULT 0,
    total_fields        int         NOT NULL DEFAULT 0,
    assignee            text,
    claimed_at          timestamptz,
    receipt_artifact_id uuid,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    completed_at        timestamptz
);
CREATE UNIQUE INDEX tasks_one_active_per_document ON workbench.tasks (document_id)
    WHERE status IN ('open', 'in_progress');
CREATE INDEX tasks_queue_idx ON workbench.tasks (status, created_at);

-- Autosaved, unsubmitted work.
CREATE TABLE workbench.drafts (
    task_id    uuid PRIMARY KEY REFERENCES workbench.tasks (id) ON DELETE CASCADE,
    mode       text        NOT NULL CHECK (mode IN ('review', 'full')),
    "values"   jsonb       NOT NULL,
    updated_by text        NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE workbench.task_events (
    id      bigserial PRIMARY KEY,
    task_id uuid        NOT NULL REFERENCES workbench.tasks (id),
    at      timestamptz NOT NULL DEFAULT now(),
    actor   text,
    action  text        NOT NULL,
    details jsonb       NOT NULL DEFAULT '{}'
);
CREATE INDEX task_events_task_idx ON workbench.task_events (task_id, id);
