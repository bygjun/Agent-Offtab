import { execFileSync, spawnSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, renameSync, rmdirSync, statSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';

const signingDirectory = join(homedir(), 'Library', 'Application Support', 'Offtab Release');
const identifier = 'local.sessiondesk.app';
const target = 'aarch64-apple-darwin';
const app = `src-tauri/target/${target}/release/bundle/macos/Offtab.app`;
const toolsPath = `/usr/bin:/bin:/usr/sbin:/sbin:${process.env.PATH ?? ''}`;

function buildApp(bundleOptions, args, env) {
  const info = join(app, 'Contents', 'Info.plist');
  const previousBundle = statSync(info, { throwIfNoEntry: false })?.ino;
  const build = spawnSync('tauri', ['build', '--target', target, ...bundleOptions, ...args], { env, stdio: 'inherit' });
  if (build.error) throw build.error;
  if (build.status !== 0) throw new Error(`Tauri build failed (${build.status ?? build.signal}).`);
  const bundled = statSync(info, { throwIfNoEntry: false });
  if (!bundled || bundled.ino === previousBundle) throw new Error('Tauri did not produce a new Offtab release app.');
}

function packageMac(args) {
  let savedTeam;
  try {
    savedTeam = readFileSync(join(signingDirectory, 'signing-team'), 'utf8').trim();
  } catch (error) {
    if (error.code !== 'ENOENT') throw error;
  }
  const requestedTeam = process.env.OFFTAB_SIGNING_TEAM?.trim();
  if (savedTeam && requestedTeam && savedTeam !== requestedTeam) {
    throw new Error('OFFTAB_SIGNING_TEAM does not match the configured Offtab signing team.');
  }
  const team = savedTeam || requestedTeam;
  if (!team || !/^[A-Z0-9]{10}$/.test(team)) {
    throw new Error('Set OFFTAB_SIGNING_TEAM to the Apple Developer team ID assigned to Offtab.');
  }

  const keychain = execFileSync('/usr/bin/security', ['find-identity', '-v', '-p', 'codesigning'], { encoding: 'utf8' });
  const identities = [...keychain.matchAll(/^\s*\d+\) ([A-F0-9]{40}) "(Developer ID Application: [^"\n]+ \(([A-Z0-9]{10})\))"$/gm)];
  const requestedIdentity = process.env.APPLE_SIGNING_IDENTITY?.trim();
  const candidates = identities.filter(([, hash, name, candidateTeam]) => candidateTeam === team
    && (!requestedIdentity || requestedIdentity === hash || requestedIdentity === name));
  if (candidates.length !== 1) {
    throw new Error(candidates.length === 0
      ? `No matching Developer ID Application identity is available for Offtab team ${team}.`
      : 'Multiple Developer ID identities match. Set APPLE_SIGNING_IDENTITY to the certificate fingerprint.');
  }

  const env = {
    ...process.env,
    PATH: toolsPath,
    APPLE_SIGNING_IDENTITY: candidates[0][1],
  };
  buildApp(['--bundles', 'app,dmg'], args, env);

  const requirement = `identifier "${identifier}" and anchor apple generic and certificate leaf[field.1.2.840.113635.100.6.1.13] exists and certificate leaf[subject.OU] = "${team}"`;
  execFileSync('/usr/bin/codesign', ['--verify', '--deep', '--strict', `-R=${requirement}`, app], { stdio: 'inherit' });
  console.log(`Verified Offtab's Developer ID signature for team ${team}.`);
}

function packageLocal(args) {
  const fingerprintFile = join(signingDirectory, 'local-signing-cert');
  let fingerprint;
  try {
    fingerprint = readFileSync(fingerprintFile, 'utf8').trim();
  } catch (error) {
    if (error.code === 'ENOENT') throw new Error(`Set up the Offtab local signing certificate in ${fingerprintFile}.`);
    throw error;
  }
  if (!/^[A-F0-9]{40}$/.test(fingerprint)) throw new Error('Invalid Offtab local certificate fingerprint.');
  const keychain = execFileSync('/usr/bin/security', ['find-identity', '-v', '-p', 'codesigning'], { encoding: 'utf8' });
  if (!keychain.split('\n').some(line => line.trim().split(/\s+/)[1] === fingerprint)) {
    throw new Error('The configured Offtab local signing identity is unavailable in Keychain.');
  }

  buildApp(['--bundles', 'app', '--no-sign'], args, { ...process.env, PATH: toolsPath });
  const version = execFileSync('/usr/libexec/PlistBuddy', ['-c', 'Print CFBundleShortVersionString', join(app, 'Contents', 'Info.plist')], { encoding: 'utf8' }).trim();
  if (!/^\d+\.\d+\.\d+$/.test(version)) throw new Error('Set an explicit local build version such as 0.5.9.');
  const local = 'release/local';
  mkdirSync(local, { recursive: true });
  const output = mkdtempSync(join(local, `Offtab-${version}-`));
  const destination = join(output, 'Offtab.app');
  const requirement = `identifier "${identifier}" and certificate leaf = H"${fingerprint}"`;
  try {
    execFileSync('/usr/bin/codesign', ['--force', '--sign', fingerprint, '--timestamp=none', '--options', 'runtime', `-r=designated => ${requirement}`, app], { stdio: 'inherit' });
    execFileSync('/usr/bin/codesign', ['--verify', '--deep', '--strict', `-R=${requirement}`, app], { stdio: 'inherit' });
    renameSync(app, destination);
  } catch (error) {
    rmdirSync(output);
    throw error;
  }
  console.log(`Verified locally signed Offtab: ${destination}`);
}

try {
  const args = process.argv.slice(2);
  if (args[0] === '--local') packageLocal(args.slice(1));
  else packageMac(args);
} catch (error) {
  console.error(error.message);
  process.exitCode = 1;
}
