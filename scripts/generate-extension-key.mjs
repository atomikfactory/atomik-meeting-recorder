// Generates a stable Chrome/Edge extension keypair so the extension ID never
// changes across "Load unpacked" reloads or CI checkouts. Chromium derives an
// extension's ID from the SHA-256 hash of the DER-encoded SubjectPublicKeyInfo
// (the `key` field in manifest.json): the first 16 bytes of that hash, with
// each nibble mapped 0-15 -> 'a'-'p'. This script performs the same
// derivation so the printed ID matches what Chrome/Edge will assign.
//
// Run once; commit the resulting public key into
// browser-extension/manifest.json ("key") and the derived ID into the native
// messaging host manifests' allowed_origins. The private key is written to
// browser-extension/key/extension-key.pem, is NOT committed (see .gitignore),
// and is only needed again if you want to re-sign a .crx (not required for
// unpacked development loading or for this project's Firefox-style
// allowlisting, which uses a gecko ID instead).
import { generateKeyPairSync, createHash } from "node:crypto";
import { writeFileSync, mkdirSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

const here = path.dirname(fileURLToPath(import.meta.url));
const keyDir = path.join(here, "..", "browser-extension", "key");
mkdirSync(keyDir, { recursive: true });

const { publicKey, privateKey } = generateKeyPairSync("rsa", {
  modulusLength: 2048,
  publicKeyEncoding: { type: "spki", format: "der" },
  privateKeyEncoding: { type: "pkcs8", format: "pem" },
});

const publicKeyBase64 = publicKey.toString("base64");
const hash = createHash("sha256").update(publicKey).digest();
const id = Array.from(hash.subarray(0, 16))
  .map((byte) => {
    const nibbleHigh = (byte >> 4) & 0x0f;
    const nibbleLow = byte & 0x0f;
    return String.fromCharCode(97 + nibbleHigh) + String.fromCharCode(97 + nibbleLow);
  })
  .join("");

writeFileSync(path.join(keyDir, "extension-key.pem"), privateKey);
writeFileSync(path.join(keyDir, "extension-id.txt"), id + "\n");
writeFileSync(path.join(keyDir, "extension-key.base64.txt"), publicKeyBase64 + "\n");

console.log("Chromium extension ID:", id);
console.log("Public key (manifest.json 'key' field) written to browser-extension/key/extension-key.base64.txt");
console.log("Private key (DO NOT COMMIT) written to browser-extension/key/extension-key.pem");
