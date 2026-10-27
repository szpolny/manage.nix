-- M3: repositories, machine configurations and tags, deployments, and
-- their logs.
--
-- Statuses are text (see mnix_protocol::api), signatures and reports JSON
-- text.

CREATE TABLE repositories (
    id              TEXT PRIMARY KEY NOT NULL,
    organization_id TEXT NOT NULL REFERENCES organizations (id),
    name            TEXT NOT NULL,
    url             TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    UNIQUE (organization_id, name)
);

-- Where the machine's system is built from:
-- <repository>#nixosConfigurations.<configuration>.
ALTER TABLE machines ADD COLUMN repository_id TEXT REFERENCES repositories (id);
ALTER TABLE machines ADD COLUMN configuration TEXT;

CREATE TABLE machine_tags (
    machine_id TEXT NOT NULL REFERENCES machines (id) ON DELETE CASCADE,
    tag        TEXT NOT NULL,
    PRIMARY KEY (machine_id, tag)
);

CREATE TABLE deployments (
    id              TEXT PRIMARY KEY NOT NULL,
    organization_id TEXT NOT NULL REFERENCES organizations (id),
    -- deploy | rollback
    kind            TEXT NOT NULL,
    -- queued | building | activating | succeeded | failed
    status          TEXT NOT NULL,
    -- Why the deployment failed before reaching the machines.
    message         TEXT,
    repository_id   TEXT REFERENCES repositories (id),
    -- The revision as requested, and the commit it resolved to.
    rev             TEXT,
    commit_id       TEXT,
    -- switch | boot | test
    mode            TEXT NOT NULL,
    requested_by    TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    started_at      TEXT,
    finished_at     TEXT
);

CREATE INDEX deployments_status ON deployments (status, created_at);
CREATE INDEX deployments_created ON deployments (organization_id, created_at);

CREATE TABLE deployment_machines (
    deployment_id TEXT NOT NULL REFERENCES deployments (id) ON DELETE CASCADE,
    machine_id    TEXT NOT NULL REFERENCES machines (id) ON DELETE CASCADE,
    configuration TEXT,
    -- queued | building | activating | succeeded | rolled_back | failed
    status        TEXT NOT NULL,
    -- The phase the agent last reported.
    phase         TEXT,
    store_path    TEXT,
    -- JSON array of signatures on store_path, sent with the request.
    signatures    TEXT NOT NULL,
    message       TEXT,
    -- The agent's ActivationReport (JSON).
    report        TEXT,
    -- Bytes of the agent's activation log received so far.
    log_bytes     INTEGER NOT NULL DEFAULT 0,
    -- When the request was handed to the agent sessions, and when the
    -- agent first reported a phase for it.
    released_at   TEXT,
    accepted_at   TEXT,
    finished_at   TEXT,
    PRIMARY KEY (deployment_id, machine_id)
);

CREATE INDEX deployment_machines_activating ON deployment_machines (machine_id)
    WHERE status = 'activating';

CREATE TABLE deployment_logs (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    deployment_id TEXT NOT NULL REFERENCES deployments (id) ON DELETE CASCADE,
    machine_id    TEXT REFERENCES machines (id) ON DELETE SET NULL,
    -- server (fetch, build, push) | agent (activation)
    source        TEXT NOT NULL,
    text          TEXT NOT NULL,
    created_at    TEXT NOT NULL
);

CREATE INDEX deployment_logs_deployment ON deployment_logs (deployment_id, id);
