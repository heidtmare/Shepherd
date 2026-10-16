-- Outbound correspondence: one letter per triggering event, delivered on one or more channels.

CREATE TABLE notification.letters (
    id                 uuid PRIMARY KEY,
    case_id            uuid        NOT NULL,
    receipt_number     text        NOT NULL,
    -- Case primary document; letter artifacts are stored on it.
    document_id        uuid        NOT NULL,
    kind               text        NOT NULL CHECK (kind IN ('receipt', 'rfe', 'noid', 'approval', 'denial', 'withdrawal')),
    -- Entity the letter answers: the case (receipt), the notice (rfe, noid) or the decision artifact.
    source_id          uuid        NOT NULL,
    -- workbench.notice.v1 or workbench.decision.v1 artifact.
    source_artifact_id uuid,
    status             text        NOT NULL CHECK (status IN ('held', 'generated')),
    hold_reason        text,
    attempts           int         NOT NULL DEFAULT 0,
    -- notification.letter.v1 and notification.letter.pdf artifacts.
    artifact_id        uuid,
    pdf_artifact_id    uuid,
    -- {name, lines} the letter was addressed to.
    recipient          jsonb,
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now(),
    generated_at       timestamptz,
    UNIQUE (kind, source_id),
    CHECK ((status = 'generated') = (pdf_artifact_id IS NOT NULL))
);
CREATE INDEX letters_case_idx ON notification.letters (case_id, created_at);
CREATE INDEX letters_held_idx ON notification.letters (updated_at) WHERE status = 'held';

CREATE TABLE notification.deliveries (
    id              uuid PRIMARY KEY,
    letter_id       uuid        NOT NULL REFERENCES notification.letters (id),
    channel         text        NOT NULL CHECK (channel IN ('mail', 'email')),
    -- Postal address (lines joined by newlines) or e-mail address.
    address         text        NOT NULL,
    -- E-mail content, fixed at generation.
    subject         text,
    body            text,
    status          text        NOT NULL CHECK (status IN ('queued', 'sent', 'failed')),
    attempts        int         NOT NULL DEFAULT 0,
    last_error      text,
    next_attempt_at timestamptz NOT NULL DEFAULT now(),
    sent_at         timestamptz,
    -- Mail room user who recorded the mailing.
    sent_by         text,
    -- SMTP message id, or the tracking reference recorded for a mailing.
    reference       text,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    UNIQUE (letter_id, channel)
);
CREATE INDEX deliveries_queue_idx ON notification.deliveries (channel, status, created_at);
CREATE INDEX deliveries_due_idx ON notification.deliveries (next_attempt_at) WHERE status = 'queued' AND channel = 'email';
