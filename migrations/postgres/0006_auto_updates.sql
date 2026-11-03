-- Updates the server applies to a machine on its own
-- (`PUT /api/machines/:machine/auto-update`).
ALTER TABLE machines ADD COLUMN auto_update_nixpkgs BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE machines ADD COLUMN auto_update_flatpak BOOLEAN NOT NULL DEFAULT FALSE;
