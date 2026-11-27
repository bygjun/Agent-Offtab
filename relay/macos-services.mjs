import { createHash } from 'node:crypto';
import { mkdirSync, realpathSync, writeFileSync } from 'node:fs';
import { isAbsolute, join } from 'node:path';
import { homedir } from 'node:os';
import { fileURLToPath } from 'node:url';

const [address, tunnelConfig, cloudflared, output] = process.argv.slice(2);
if (process.platform !== 'darwin' || ![tunnelConfig, cloudflared, output].every(path => path && isAbsolute(path))) {
  throw new Error('Usage: node relay/macos-services.mjs HTTPS_ORIGIN /path/config.yml /path/cloudflared /output/directory');
}
const origin = new URL(address);
if (origin.protocol !== 'https:' || origin.href !== origin.origin + '/') throw new Error('Provide an HTTPS origin.');

const relay = fileURLToPath(new URL('./server.mjs', import.meta.url));
const state = join(homedir(), 'Library', 'Application Support', 'Offtab Relay', createHash('sha256').update(origin.origin).digest('base64url') + '.json');
const logs = join(output, 'logs');
mkdirSync(logs, { recursive: true, mode: 0o700 });
const port = '63915';

function escape(value) {
  return value.replace(/[&<>"']/g, char => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&apos;' })[char]);
}
const str = value => '<string>' + escape(value) + '</string>';
const entries = values => Object.entries(values).map(([key, value]) => '<key>' + key + '</key>' + str(value)).join('\n');
function writeService(label, args, environment = {}) {
  const plist = `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
${entries({ Label: label, StandardOutPath: join(logs, label + '.log'), StandardErrorPath: join(logs, label + '.errors.log') })}
<key>ProgramArguments</key><array>${args.map(str).join('')}</array>
<key>EnvironmentVariables</key><dict>${entries(environment)}</dict>
<key>RunAtLoad</key><true/>
<key>KeepAlive</key><true/>
</dict></plist>
`;
  const path = join(output, label + '.plist');
  writeFileSync(path, plist, { mode: 0o600, flag: 'wx' });
  console.log(path);
}

writeService('local.offtab.relay', [process.execPath, relay], {
  OFFTAB_RELAY_ORIGIN: origin.origin,
  OFFTAB_RELAY_LOCAL_REGISTRATION: '1',
  OFFTAB_RELAY_STATE: state,
  HOST: '127.0.0.1',
  PORT: port,
});
writeService('local.offtab.tunnel', [realpathSync(cloudflared), 'tunnel', '--config', realpathSync(tunnelConfig), 'run']);
console.log('Mac 내부 중계  http://127.0.0.1:' + port + '/');
