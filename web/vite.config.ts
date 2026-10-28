import adapter from '@sveltejs/adapter-static';
import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

export default defineConfig({
	plugins: [
		sveltekit({
			// A single-page app: mnix-server serves `index.html` for every path
			// that is not a file or under /api/.
			adapter: adapter({ fallback: 'index.html' })
		})
	],
	server: {
		// `npm run dev` against a local `mnix-server serve --insecure-cookies`.
		proxy: { '/api': 'http://127.0.0.1:8080' }
	}
});
