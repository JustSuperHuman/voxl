import { generateKeyPairSync, createPrivateKey, createPublicKey } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { execFileSync } from 'node:child_process';

const keyPath = process.argv[2] ?? join(process.env.USERPROFILE ?? process.env.HOME, '.bro', 'rmm-release-signing.pem');
mkdirSync(dirname(keyPath), { recursive: true });
if (!existsSync(keyPath)) {
  const { privateKey } = generateKeyPairSync('ed25519');
  writeFileSync(keyPath, privateKey.export({ type: 'pkcs8', format: 'pem' }), { mode: 0o600, flag: 'wx' });
}
if (process.platform === 'win32') {
  const account = process.env.USERDOMAIN ? `${process.env.USERDOMAIN}\\${process.env.USERNAME}` : process.env.USERNAME;
  execFileSync('icacls.exe', [keyPath, '/inheritance:r', '/grant:r', `${account}:(F)`, '*S-1-5-18:(F)'], { stdio: 'ignore' });
}
const key = createPrivateKey(readFileSync(keyPath));
if (key.asymmetricKeyType !== 'ed25519') throw new Error('The existing signing key is not Ed25519.');
const publicDer = createPublicKey(key).export({ type: 'spki', format: 'der' });
console.log(publicDer.subarray(-32).toString('hex'));
