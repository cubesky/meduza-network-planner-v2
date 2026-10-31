// OpenVPN PKI on the manager (AGENT.md §6.3, §16: heavy key work happens here, not on 128 MiB nodes).
// Uses @peculiar/x509 on WebCrypto — no hand-written ASN.1. Certificates, CRLs and keys are exposed to node
// configurations as secret references (`pki-...`), delivered only to the nodes in scope.

import { randomBytes } from "node:crypto";
import * as x509 from "@peculiar/x509";
import type { Db } from "./db.js";
import { nowIso } from "./db.js";
import type { SecretStore } from "./secrets.js";
import type { Audit } from "./audit.js";

// Node 24 exposes WebCrypto as the global `crypto` (DOM-typed), which @peculiar/x509 uses.
x509.cryptoProvider.set(globalThis.crypto);

const subtle = globalThis.crypto.subtle;
export type KeyAlgorithm = "ecdsa-p256" | "rsa-2048" | "rsa-3072";

function algorithm(a: KeyAlgorithm): RsaHashedKeyGenParams | EcKeyGenParams {
  if (a === "ecdsa-p256") return { name: "ECDSA", namedCurve: "P-256" } as EcKeyGenParams;
  return { name: "RSASSA-PKCS1-v1_5", modulusLength: a === "rsa-2048" ? 2048 : 3072, publicExponent: new Uint8Array([1, 0, 1]), hash: "SHA-256" } as RsaHashedKeyGenParams;
}

function signing(a: KeyAlgorithm): Algorithm {
  return a === "ecdsa-p256" ? ({ name: "ECDSA", hash: "SHA-256" } as EcdsaParams) : ({ name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" } as Algorithm);
}

function pem(label: string, der: ArrayBuffer): string {
  const b64 = Buffer.from(der).toString("base64").replace(/(.{64})/g, "$1\n").trim();
  return `-----BEGIN ${label}-----\n${b64}\n-----END ${label}-----\n`;
}

function derFromPem(text: string): ArrayBuffer {
  const b = Buffer.from(text.replace(/-----[^-]+-----/g, "").replace(/\s+/g, ""), "base64");
  return b.buffer.slice(b.byteOffset, b.byteOffset + b.byteLength) as ArrayBuffer;
}

function serial(): string {
  const b = randomBytes(16);
  b[0] = b[0]! & 0x7f; // positive INTEGER
  return b.toString("hex");
}

/** DN component escaping (RFC 4514) for user-supplied common names. */
function cn(value: string): string {
  const v = value.trim();
  if (!v || v.length > 64) throw new Error("common name must be 1-64 characters");
  return `CN=${v.replace(/([,+"\\<>;=])/g, "\\$1").replace(/^([ #])/, "\\$1")}`;
}

/** OpenVPN static key (tls-crypt / tls-auth / secret): 2048 random bits in the V1 text format. */
export function openvpnStaticKey(): string {
  const hex = randomBytes(256).toString("hex");
  const lines = hex.match(/.{32}/g)!.join("\n");
  return `-----BEGIN OpenVPN Static key V1-----\n${lines}\n-----END OpenVPN Static key V1-----\n`;
}

export interface CaRecord {
  id: string;
  name: string;
  certPem: string;
  keySecretId: string;
  algorithm: KeyAlgorithm;
}

export class Pki {
  constructor(private readonly db: Db, private readonly secrets: SecretStore, private readonly audit: Audit) {}

  private ca(id: string): CaRecord {
    const r = this.db.get<{ id: string; name: string; cert_pem: string; key_secret_id: string }>("SELECT * FROM pki_cas WHERE id = ?", id);
    if (!r) throw new Error(`CA ${id} not found`);
    const cert = new x509.X509Certificate(r.cert_pem);
    const alg: KeyAlgorithm = cert.publicKey.algorithm.name === "ECDSA" ? "ecdsa-p256" : "rsa-2048";
    return { id: r.id, name: r.name, certPem: r.cert_pem, keySecretId: r.key_secret_id, algorithm: alg };
  }

  private async caKey(ca: CaRecord): Promise<CryptoKey> {
    const der = derFromPem(this.secrets.reveal(ca.keySecretId));
    const params = ca.algorithm === "ecdsa-p256" ? { name: "ECDSA", namedCurve: "P-256" } : { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" };
    return subtle.importKey("pkcs8", der, params, false, ["sign"]);
  }

  listCas() {
    return this.db.all<Record<string, unknown>>("SELECT id, name, cert_pem, crl_number, created_at FROM pki_cas ORDER BY id").map((r) => {
      const c = new x509.X509Certificate(String(r.cert_pem));
      return { id: r.id, name: r.name, subject: c.subject, notBefore: c.notBefore.toISOString(), notAfter: c.notAfter.toISOString(), crlNumber: r.crl_number, certRef: `pki-ca-${r.id}-cert`, crlRef: `pki-ca-${r.id}-crl` };
    });
  }

  listCerts(caId?: string) {
    const rows = caId
      ? this.db.all<Record<string, unknown>>("SELECT * FROM pki_certs WHERE ca_id = ? ORDER BY created_at DESC", caId)
      : this.db.all<Record<string, unknown>>("SELECT * FROM pki_certs ORDER BY created_at DESC");
    return rows.map((r) => ({
      serial: r.serial,
      caId: r.ca_id,
      subject: r.subject,
      usage: r.usage,
      notBefore: r.not_before,
      notAfter: r.not_after,
      revokedAt: r.revoked_at,
      revocationReason: r.revocation_reason,
      certRef: `pki-${r.serial}-cert`,
      keyRef: r.key_secret_id,
      expiresInDays: Math.floor((Date.parse(String(r.not_after)) - Date.now()) / 86_400_000),
    }));
  }

  async createCa(input: { id: string; name: string; algorithm?: KeyAlgorithm; validityDays?: number }, actor: string) {
    if (!/^[a-z0-9][a-z0-9_-]{0,40}$/.test(input.id)) throw new Error("invalid CA id");
    if (this.db.get("SELECT 1 AS x FROM pki_cas WHERE id = ?", input.id)) throw new Error("CA exists");
    const alg = input.algorithm ?? "ecdsa-p256";
    const keys = (await subtle.generateKey(algorithm(alg), true, ["sign", "verify"])) as CryptoKeyPair;
    const notBefore = new Date(Date.now() - 60_000);
    const notAfter = new Date(Date.now() + (input.validityDays ?? 3650) * 86_400_000);
    const cert = await x509.X509CertificateGenerator.createSelfSigned({
      serialNumber: serial(),
      name: cn(input.name),
      notBefore,
      notAfter,
      signingAlgorithm: signing(alg),
      keys,
      extensions: [
        new x509.BasicConstraintsExtension(true, 0, true),
        new x509.KeyUsagesExtension(x509.KeyUsageFlags.keyCertSign | x509.KeyUsageFlags.cRLSign, true),
        await x509.SubjectKeyIdentifierExtension.create(keys.publicKey),
      ],
    });
    const keySecret = `pki-ca-${input.id}-key`;
    this.secrets.put({ id: keySecret, kind: "pem-private-key", value: pem("PRIVATE KEY", await subtle.exportKey("pkcs8", keys.privateKey)), nodeScope: [], description: `CA ${input.name} signing key` });
    const certPem = cert.toString("pem");
    this.secrets.put({ id: `pki-ca-${input.id}-cert`, kind: "pem-certificate", value: certPem, publicValue: certPem, nodeScope: [], description: `CA ${input.name}` });
    this.db.run("INSERT INTO pki_cas (id, name, cert_pem, key_secret_id, created_at) VALUES (?, ?, ?, ?, ?)", input.id, input.name, certPem, keySecret, nowIso());
    await this.generateCrl(input.id);
    this.audit.record(actor, "pki.ca.create", input.id, { name: input.name, algorithm: alg });
    return this.listCas().find((c) => c.id === input.id)!;
  }

  private shareCa(caId: string, node: string | undefined): void {
    if (!node) return;
    for (const ref of [`pki-ca-${caId}-cert`, `pki-ca-${caId}-crl`]) {
      const meta = this.secrets.meta(ref);
      if (meta && !meta.nodeScope.includes(node)) this.secrets.setScope(ref, [...meta.nodeScope, node]);
    }
  }

  private async issue(ca: CaRecord, input: { commonName: string; usage: "server" | "client"; publicKey: CryptoKey; validityDays?: number; dnsNames?: string[] }) {
    const caCert = new x509.X509Certificate(ca.certPem);
    const sn = serial();
    const notBefore = new Date(Date.now() - 60_000);
    const notAfter = new Date(Math.min(Date.now() + (input.validityDays ?? 825) * 86_400_000, caCert.notAfter.getTime()));
    const ec = input.publicKey.algorithm.name === "ECDSA";
    const ku = x509.KeyUsageFlags.digitalSignature | (ec ? x509.KeyUsageFlags.keyAgreement : x509.KeyUsageFlags.keyEncipherment);
    const extensions: x509.Extension[] = [
      new x509.BasicConstraintsExtension(false, undefined, true),
      new x509.KeyUsagesExtension(ku, true),
      new x509.ExtendedKeyUsageExtension([input.usage === "server" ? x509.ExtendedKeyUsage.serverAuth : x509.ExtendedKeyUsage.clientAuth]),
      await x509.SubjectKeyIdentifierExtension.create(input.publicKey),
      await x509.AuthorityKeyIdentifierExtension.create(caCert),
    ];
    if (input.dnsNames?.length) {
      extensions.push(new x509.SubjectAlternativeNameExtension(input.dnsNames.map((d) => ({ type: "dns" as const, value: d }))));
    }
    const cert = await x509.X509CertificateGenerator.create({
      serialNumber: sn,
      subject: cn(input.commonName),
      issuer: caCert.subject,
      notBefore,
      notAfter,
      signingAlgorithm: signing(ca.algorithm),
      publicKey: input.publicKey,
      signingKey: await this.caKey(ca),
      extensions,
    });
    return { serial: sn, cert, notBefore, notAfter };
  }

  /** Issues a certificate and key on the manager; both are delivered only to `node`. */
  async issueWithKey(input: { caId: string; commonName: string; usage: "server" | "client"; node?: string; validityDays?: number; dnsNames?: string[]; algorithm?: KeyAlgorithm }, actor: string) {
    const ca = this.ca(input.caId);
    const keys = (await subtle.generateKey(algorithm(input.algorithm ?? "ecdsa-p256"), true, ["sign", "verify"])) as CryptoKeyPair;
    const issued = await this.issue(ca, { ...input, publicKey: keys.publicKey });
    const keyRef = `pki-${issued.serial}-key`;
    this.secrets.put({ id: keyRef, kind: "pem-private-key", value: pem("PRIVATE KEY", await subtle.exportKey("pkcs8", keys.privateKey)), nodeScope: input.node ? [input.node] : [], description: `${input.usage} key ${input.commonName}` });
    this.store(ca, issued, input.commonName, input.usage, input.node, keyRef);
    this.audit.record(actor, "pki.cert.issue", issued.serial, { ca: input.caId, commonName: input.commonName, usage: input.usage, node: input.node });
    return { serial: issued.serial, certRef: `pki-${issued.serial}-cert`, keyRef, caRef: `pki-ca-${input.caId}-cert`, crlRef: `pki-ca-${input.caId}-crl` };
  }

  /** Signs a CSR: the private key never reaches the manager. */
  async signCsr(input: { caId: string; csrPem: string; usage: "server" | "client"; node?: string; validityDays?: number }, actor: string) {
    const ca = this.ca(input.caId);
    const csr = new x509.Pkcs10CertificateRequest(input.csrPem);
    if (!(await csr.verify())) throw new Error("CSR signature is invalid");
    const commonName = /CN=([^,]+)/.exec(csr.subject)?.[1] ?? "unnamed";
    const publicKey = await csr.publicKey.export();
    const issued = await this.issue(ca, { commonName, usage: input.usage, publicKey, validityDays: input.validityDays });
    this.store(ca, issued, commonName, input.usage, input.node, null);
    this.audit.record(actor, "pki.csr.sign", issued.serial, { ca: input.caId, commonName, usage: input.usage, node: input.node });
    return { serial: issued.serial, certRef: `pki-${issued.serial}-cert`, certPem: issued.cert.toString("pem") };
  }

  private store(ca: CaRecord, issued: { serial: string; cert: x509.X509Certificate; notBefore: Date; notAfter: Date }, commonName: string, usage: string, node: string | undefined, keyRef: string | null) {
    const certPem = issued.cert.toString("pem");
    this.secrets.put({ id: `pki-${issued.serial}-cert`, kind: "pem-certificate", value: certPem, publicValue: certPem, nodeScope: node ? [node] : [], description: `${usage} certificate ${commonName}` });
    this.db.run(
      "INSERT INTO pki_certs (serial, ca_id, subject, usage, not_before, not_after, cert_pem, key_secret_id, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
      issued.serial, ca.id, issued.cert.subject, usage, issued.notBefore.toISOString(), issued.notAfter.toISOString(), certPem, keyRef, nowIso(),
    );
    this.shareCa(ca.id, node);
  }

  async revoke(serialNumber: string, reason: string, actor: string) {
    const r = this.db.get<{ ca_id: string; revoked_at: string | null }>("SELECT ca_id, revoked_at FROM pki_certs WHERE serial = ?", serialNumber);
    if (!r) throw new Error("certificate not found");
    if (!r.revoked_at) this.db.run("UPDATE pki_certs SET revoked_at = ?, revocation_reason = ? WHERE serial = ?", nowIso(), reason, serialNumber);
    await this.generateCrl(r.ca_id);
    this.audit.record(actor, "pki.cert.revoke", serialNumber, { reason });
  }

  /** Regenerates CRLs stored with the non-standard "BEGIN CRL" label by earlier versions (OpenSSL rejects them). */
  async repairCrlLabels(): Promise<string[]> {
    const fixed: string[] = [];
    for (const { id } of this.db.all<{ id: string }>("SELECT id FROM pki_cas")) {
      if (!this.secrets.meta(`pki-ca-${id}-crl`)?.publicValue?.includes("BEGIN X509 CRL")) {
        await this.generateCrl(id);
        fixed.push(id);
      }
    }
    return fixed;
  }

  /** Regenerates the CA's CRL (next update in 30 days). Nodes receive it via `crlRef` at the next publish. */
  async generateCrl(caId: string): Promise<string> {
    const ca = this.ca(caId);
    const caCert = new x509.X509Certificate(ca.certPem);
    const number = (this.db.get<{ n: number }>("SELECT crl_number AS n FROM pki_cas WHERE id = ?", caId)?.n ?? 0) + 1;
    const revoked = this.db.all<{ serial: string; revoked_at: string }>("SELECT serial, revoked_at FROM pki_certs WHERE ca_id = ? AND revoked_at IS NOT NULL", caId);
    const crl = await x509.X509CrlGenerator.create({
      issuer: caCert.subject,
      thisUpdate: new Date(),
      nextUpdate: new Date(Date.now() + 30 * 86_400_000),
      entries: revoked.map((r) => ({ serialNumber: r.serial, revocationDate: new Date(r.revoked_at) })),
      signingAlgorithm: signing(ca.algorithm),
      signingKey: await this.caKey(ca),
      extensions: [new x509.Extension("2.5.29.20", false, encodeCrlNumber(number))],
    });
    // RFC 7468 label: the library writes "BEGIN CRL", OpenSSL (OpenVPN's crl-verify) only reads "BEGIN X509 CRL".
    const crlPem = crl.toString("pem").replace(/-----(BEGIN|END) CRL-----/g, "-----$1 X509 CRL-----");
    const ref = `pki-ca-${caId}-crl`;
    const scope = this.secrets.meta(ref)?.nodeScope ?? this.secrets.meta(`pki-ca-${caId}-cert`)?.nodeScope ?? [];
    this.secrets.put({ id: ref, kind: "pem-crl", value: crlPem, publicValue: crlPem, nodeScope: scope, description: `CRL of ${ca.name}` });
    this.db.run("UPDATE pki_cas SET crl_number = ? WHERE id = ?", number, caId);
    return crlPem;
  }

  createStaticKey(id: string, nodes: string[], actor: string, replace = false) {
    if (this.secrets.meta(id) && !replace) throw new Error("key exists; pass replace=true to regenerate (all peers must be republished)");
    const rec = this.secrets.put({ id, kind: "openvpn-tls-crypt", value: openvpnStaticKey(), nodeScope: nodes, description: "OpenVPN tls-crypt / static key" });
    this.audit.record(actor, replace ? "pki.static-key.regenerate" : "pki.static-key.create", id, { nodes });
    return rec;
  }

  /** Access client profile (`.ovpn`): remote, CA, tls-crypt and auth-user-pass; optional client cert/key. */
  accessClientProfile(input: { remoteHost: string; port: number; proto: string; caRef: string; tlsCryptRef?: string; certRef?: string; keyRef?: string; verifyName?: string; name: string }) {
    const lines = [
      `# Meduza NP Access profile: ${input.name}`,
      "client",
      "dev tun",
      `proto ${input.proto.startsWith("tcp") ? "tcp-client" : input.proto}`,
      `remote ${input.remoteHost} ${input.port}`,
      "nobind",
      "persist-key",
      "persist-tun",
      "remote-cert-tls server",
      ...(input.verifyName ? [`verify-x509-name "${input.verifyName}" name`] : []),
      "auth-user-pass",
      "auth-nocache",
      "verb 3",
      "<ca>",
      this.secrets.reveal(input.caRef).trim(),
      "</ca>",
    ];
    if (input.tlsCryptRef) lines.push("<tls-crypt>", this.secrets.reveal(input.tlsCryptRef).trim(), "</tls-crypt>");
    if (input.certRef && input.keyRef) lines.push("<cert>", this.secrets.reveal(input.certRef).trim(), "</cert>", "<key>", this.secrets.reveal(input.keyRef).trim(), "</key>");
    return `${lines.join("\n")}\n`;
  }
}

/** DER INTEGER for the CRL number extension. */
function encodeCrlNumber(n: number): ArrayBuffer {
  const bytes: number[] = [];
  let v = n;
  do {
    bytes.unshift(v & 0xff);
    v = Math.floor(v / 256);
  } while (v > 0);
  if (bytes[0]! & 0x80) bytes.unshift(0);
  return new Uint8Array([0x02, bytes.length, ...bytes]).buffer;
}
