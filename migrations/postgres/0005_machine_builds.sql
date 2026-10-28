-- 0.2: machines that build their own systems.
--
-- For a deployment the machine builds, `attribute` is the flake output it
-- builds at the deployment's commit, and store_path stays NULL until the
-- agent reports what it built.
ALTER TABLE deployment_machines ADD COLUMN attribute TEXT;
