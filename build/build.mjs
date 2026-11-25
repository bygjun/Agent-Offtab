import { cp } from 'node:fs/promises';
import { build } from 'esbuild';

await Promise.all([
  build({
    entryPoints: ['src/renderer.tsx'],
    outdir: 'dist-tauri',
    bundle: true,
    minify: true,
    define: { 'process.env.NODE_ENV': '"production"' },
  }),
  cp('src/index.html', 'dist-tauri/index.html'),
]);
