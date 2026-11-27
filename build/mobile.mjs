import { createHash } from 'node:crypto';
import { cp, mkdir, readFile, writeFile } from 'node:fs/promises';
import { build } from 'esbuild';

await mkdir('dist/mobile', { recursive: true });
await Promise.all([
  build({ entryPoints: ['src/mobile/app.tsx'], outdir: 'dist/mobile', bundle: true, minify: true, target: 'safari17' }),
  cp('docs/assets/offtab.png', 'dist/mobile/offtab.png'),
  cp('src/mobile/offtab-sw.js', 'dist/mobile/offtab-sw.js'),
  cp('src/mobile/manifest.webmanifest', 'dist/mobile/manifest.webmanifest'),
]);

const files = ['app.js', 'app.css'];
const [template, ...assets] = await Promise.all([
  readFile('src/mobile/index.html', 'utf8'),
  ...files.map(file => readFile(`dist/mobile/${file}`)),
]);
const index = files.reduce((html, file, offset) => {
  const version = createHash('sha256').update(assets[offset]).digest('hex').slice(0, 12);
  return html.replace(`/${file}"`, `/${file}?v=${version}"`);
}, template);
await writeFile('dist/mobile/index.html', index);
