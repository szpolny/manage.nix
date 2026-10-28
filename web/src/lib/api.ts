// A small client for the mnix-server API. The session cookie authenticates
// every request; requests that change something also send the CSRF header
// the server requires.

import { goto } from '$app/navigation';
import type {
	ActivationMode,
	ClosureDiff,
	CreatedMachine,
	Deployment,
	EnrollmentToken,
	GenerationView,
	Machine,
	MachineDeployment,
	Repository,
	Session
} from './types.ts';

export class ApiError extends Error {
	constructor(
		readonly status: number,
		message: string
	) {
		super(message);
	}
}

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
	const headers: Record<string, string> = {};
	if (body !== undefined) headers['content-type'] = 'application/json';
	if (method !== 'GET') headers['x-mnix-csrf'] = '1';
	const response = await fetch(path, {
		method,
		headers,
		body: body === undefined ? undefined : JSON.stringify(body),
		credentials: 'same-origin'
	});
	if (!response.ok) {
		let message = response.statusText;
		try {
			message = (await response.json()).error ?? message;
		} catch {
			// Not JSON; keep the status text.
		}
		// A failed login is not a reason to go to the login page.
		if (response.status === 401 && !(method === 'POST' && path === '/api/session')) {
			const next = location.pathname + location.search;
			await goto(`/login?next=${encodeURIComponent(next)}`);
		}
		throw new ApiError(response.status, message);
	}
	if (response.status === 204) return undefined as T;
	return response.json();
}

const enc = encodeURIComponent;

export const api = {
	session: () => request<Session>('GET', '/api/session'),
	login: (username: string, password: string) =>
		request<Session>('POST', '/api/session', { username, password }),
	logout: () => request<void>('DELETE', '/api/session'),
	changePassword: (current_password: string, new_password: string) =>
		request<void>('PUT', '/api/session/password', { current_password, new_password }),

	machines: () => request<Machine[]>('GET', '/api/machines'),
	machine: (name: string) => request<Machine>('GET', `/api/machines/${enc(name)}`),
	addMachine: (name: string) => request<CreatedMachine>('POST', '/api/machines', { name }),
	enrollmentToken: (name: string) =>
		request<EnrollmentToken>('POST', `/api/machines/${enc(name)}/enrollment-tokens`, {}),
	configure: (name: string, repository: string, configuration?: string) =>
		request<Machine>('PUT', `/api/machines/${enc(name)}/configuration`, {
			repository,
			configuration: configuration || undefined
		}),
	setTags: (name: string, tags: string[]) =>
		request<Machine>('PUT', `/api/machines/${enc(name)}/tags`, { tags }),
	machineDeployments: (name: string, limit = 10) =>
		request<MachineDeployment[]>('GET', `/api/machines/${enc(name)}/deployments?limit=${limit}`),
	generations: (name: string) =>
		request<GenerationView[]>('GET', `/api/machines/${enc(name)}/generations`),
	diff: (name: string, from: number, to: number) =>
		request<ClosureDiff>('GET', `/api/machines/${enc(name)}/diff?from=${from}&to=${to}`),
	rollback: (name: string, mode: ActivationMode = 'switch') =>
		request<Deployment>('POST', `/api/machines/${enc(name)}/rollback`, { mode }),

	repositories: () => request<Repository[]>('GET', '/api/repositories'),
	addRepository: (name: string, url: string) =>
		request<Repository>('POST', '/api/repositories', { name, url }),

	deployments: (limit = 50) => request<Deployment[]>('GET', `/api/deployments?limit=${limit}`),
	deployment: (id: string) => request<Deployment>('GET', `/api/deployments/${enc(id)}`),
	deploy: (targets: string[], rev: string | undefined, mode: ActivationMode) =>
		request<Deployment>('POST', '/api/deployments', { targets, rev, mode }),
	deploymentEvents: (id: string) => `/api/deployments/${enc(id)}/events`
};

/** The message of an error thrown by the API (or anything else). */
export function errorMessage(e: unknown): string {
	return e instanceof Error ? e.message : String(e);
}
