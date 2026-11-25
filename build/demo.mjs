import { mkdir, writeFile } from 'node:fs/promises';
import { build } from 'esbuild';

const { outputFiles } = await build({
  entryPoints: ['src/demo.tsx'],
  outdir: 'demo',
  write: false,
  bundle: true,
  minify: true,
  legalComments: 'inline',
  define: { 'process.env.NODE_ENV': '"production"' },
});
const js = outputFiles.find(file => file.path.endsWith('.js')).text.replace(/<\/script/gi, '<\\/script');
const css = outputFiles.find(file => file.path.endsWith('.css')).text;

await mkdir('release', { recursive: true });
await writeFile('release/session-desk-demo.html', `<!doctype html>
<html lang="ko">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'">
  <title>Offtab · 웹 데모</title>
  <style>${css}</style>
</head>
<body class="demo">
  <div id="session-desk-demo"></div>
  <script>${js}</script>
</body>
</html>
`);
