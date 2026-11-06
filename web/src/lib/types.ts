// The HTTP API's bodies, mirroring `crates/protocol/src/api.rs`.

export type MachineStatus = 'pending' | 'online' | 'offline';
export type DeploymentKind = 'deploy' | 'rollback' | 'reboot' | 'update' | 'flatpak_update';
export type DeploymentStatus = 'queued' | 'building' | 'activating' | 'succeeded' | 'failed';
export type MachineDeploymentStatus =
	| 'queued'
	| 'building'
	| 'activating'
	| 'succeeded'
	| 'rolled_back'
	| 'failed';
export type ActivationMode = 'switch' | 'boot' | 'test';
export type ActivationPhase =
	| 'accepted'
	| 'building'
	| 'activating'
	| 'checking_health'
	| 'awaiting_confirmation'
	| 'rolling_back'
	| 'rebooting'
	| 'updating';

export interface Generation {
	number: number;
	store_path: string;
	created_at: string;
	current: boolean;
}

export type Os = 'nixos' | 'darwin';

export interface ServiceSnapshot {
 refreshed_at: string;
 error: string | null;
 services: { name: string; description: string; state: string; active_state: string; sub_state: string; load_state: string }[];
}

export interface Facts {
 services?: ServiceSnapshot | null;
	flatpak_updates?: string[] | null;
	hostname: string;
	os: Os;
	current_system: string;
	booted_system: string | null;
	/** `nixos-version` or `darwin-version`. */
	os_version: string | null;
	/** The commit the running configuration was built from, if the flake sets it. */
	configuration_revision: string | null;
	kernel: string;
	reboot_required: boolean;
	generations: Generation[];
}

export interface Revision {
	deployment_id: string;
	repository: string | null;
	rev: string | null;
	commit: string;
}

export interface MachineDeployment {
	deployment_id: string;
	kind: DeploymentKind;
	mode: ActivationMode;
	rev: string | null;
	commit: string | null;
	requested_by: string;
	status: MachineDeploymentStatus;
	store_path: string | null;
	message: string | null;
	created_at: string;
	finished_at: string | null;
}

export interface Machine {
	id: string;
	name: string;
	status: MachineStatus;
	created_at: string;
	enrolled_at: string | null;
	last_seen_at: string | null;
	agent_version: string | null;
	current_system: string | null;
	facts: Facts | null;
	repository: string | null;
	configuration: string | null;
	tags: string[];
	revision: Revision | null;
	last_deployment: MachineDeployment | null;
	auto_update: AutoUpdate;
}

/** Updates the server applies on its own, at most once a day each. */
export interface AutoUpdate {
	nixpkgs: boolean;
	flatpak: boolean;
}

export interface EnrollmentToken {
	token: string;
	expires_at: string;
}

export interface CreatedMachine {
	machine: Machine;
	enrollment_token: EnrollmentToken;
}

export interface GenerationView {
	number: number;
	store_path: string;
	created_at: string;
	current: boolean;
	running: boolean;
	booted: boolean;
	revision: Revision | null;
}

export interface ClosureDiff {
	from: string;
	to: string;
	diff: string;
}

export interface Repository {
	id: string;
	name: string;
	url: string;
	created_at: string;
}

export interface ActivationReport {
	store_path: string;
	mode: ActivationMode;
	status: 'succeeded' | 'failed' | 'rolled_back';
	system_changed: boolean;
	previous_system: string | null;
	message: string | null;
	failed_checks: string[];
}

export interface DeploymentMachine {
	machine_id: string;
	machine: string;
	configuration: string | null;
	status: MachineDeploymentStatus;
	phase: ActivationPhase | null;
	store_path: string | null;
	message: string | null;
	report: ActivationReport | null;
	finished_at: string | null;
}

export interface Deployment {
	id: string;
	kind: DeploymentKind;
	status: DeploymentStatus;
	message: string | null;
	repository: string | null;
	rev: string | null;
	commit: string | null;
	mode: ActivationMode;
	requested_by: string;
	created_at: string;
	started_at: string | null;
	finished_at: string | null;
	machines: DeploymentMachine[];
}

export interface LogEntry {
	id: number;
	machine: string | null;
	source: string;
	text: string;
	created_at: string;
}

export interface Commit {
	commit: string;
	summary: string;
	author: string;
	date: string;
}

export interface Updates {
	nixpkgs_available?: boolean | null;
	repository: string;
	/** The revision compared with. */
	rev: string;
	/** The commit `rev` names now. */
	commit: string;
	/** The commit the running system was built from, if known. */
	current_commit: string | null;
	/** Commits in `rev` the running system lacks; null if that is unknown. */
	behind: number | null;
	ahead: number | null;
	/** The newest of the commits behind, newest first. */
	commits: Commit[];
	fetched_at: string;
}

export interface UpdateSummary {
	nixpkgs_available?: boolean | null;
	machine: string;
	rev: string | null;
	commit: string | null;
	behind: number | null;
	error: string | null;
}

export type PreviewStatus = 'building' | 'succeeded' | 'failed';

export interface Preview {
	status: PreviewStatus;
	rev: string;
	commit: string | null;
	from: string;
	to: string | null;
	diff: string | null;
	message: string | null;
	started_at: string;
	finished_at: string | null;
}

export interface Session {
	username: string;
}
