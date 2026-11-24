CREATE SCHEMA connecto_works;
REVOKE ALL ON SCHEMA connecto_works FROM PUBLIC;
CREATE TABLE connecto_works.project (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    project_id text NOT NULL,
    queue_id text NOT NULL
);
CREATE TABLE connecto_works.connecto_jobs (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    project_name text NOT NULL,
    work_type text NOT NULL,
    prompt text NOT NULL,
    session_cli text NOT NULL DEFAULT 'agy',
    model text,
    effort text,
    status text NOT NULL DEFAULT 'to_do' CHECK (status IN
        ('to_do','doing','done_from_agent','failed','timeout')),
    attempt text,
    claimed_at timestamptz,
    result jsonb,
    reason text
);
CREATE INDEX doing_work ON connecto_works.connecto_jobs (claimed_at) WHERE status = 'doing';
CREATE INDEX pending_work ON connecto_works.connecto_jobs (created_at DESC, id DESC) WHERE status = 'to_do';
REVOKE ALL ON ALL TABLES IN SCHEMA connecto_works FROM PUBLIC;
REVOKE ALL ON ALL SEQUENCES IN SCHEMA connecto_works FROM PUBLIC;
