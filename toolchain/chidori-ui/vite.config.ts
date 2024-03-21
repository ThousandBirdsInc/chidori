import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

export default defineConfig({
	server: {
		fs: {
			allow: ["./sample/profiles"]
		}
	},
	optimizeDeps: {
		include: ["codemirror", "@codemirror/view"]
	},
	plugins: [sveltekit()]
});
