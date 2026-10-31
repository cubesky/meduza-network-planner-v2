import { test } from "node:test";
import assert from "node:assert/strict";
import * as x509 from "@peculiar/x509";
import { startManager } from "./helpers.js";

test("CA, server/client certificates with correct usages, CSR signing, revocation and CRL", async () => {
  const { app, s } = await startManager();
  const ca = await s.pki.createCa({ id: "vpn", name: "Meduza VPN CA" }, "test");
  assert.equal(ca.certRef, "pki-ca-vpn-cert");
  const caCert = new x509.X509Certificate(s.secrets.reveal("pki-ca-vpn-cert"));
  assert.ok(caCert.getExtension(x509.BasicConstraintsExtension)?.ca, "CA:TRUE");
  assert.ok(await caCert.isSelfSigned());

  s.nodes.createNode({ id: "gw-a" }, "test");
  const server = await s.pki.issueWithKey({ caId: "vpn", commonName: "gw-a access", usage: "server", node: "gw-a", dnsNames: ["vpn.example.test"] }, "test");
  const serverCert = new x509.X509Certificate(s.secrets.reveal(server.certRef));
  assert.equal(serverCert.issuer, caCert.subject);
  assert.ok(await serverCert.verify({ publicKey: caCert.publicKey }), "signed by the CA");
  const eku = serverCert.getExtension(x509.ExtendedKeyUsageExtension)!;
  assert.deepEqual([...eku.usages].map(String), [x509.ExtendedKeyUsage.serverAuth]);
  assert.equal(serverCert.getExtension(x509.BasicConstraintsExtension)?.ca, false);
  assert.ok(s.secrets.reveal(server.keyRef).includes("BEGIN PRIVATE KEY"));
  assert.deepEqual(s.secrets.meta(server.keyRef)!.nodeScope, ["gw-a"], "key delivered only to its node");
  assert.ok(s.secrets.meta("pki-ca-vpn-cert")!.nodeScope.includes("gw-a"), "CA certificate shared with the node");

  const client = await s.pki.issueWithKey({ caId: "vpn", commonName: "alice", usage: "client" }, "test");
  const clientCert = new x509.X509Certificate(s.secrets.reveal(client.certRef));
  assert.deepEqual([...clientCert.getExtension(x509.ExtendedKeyUsageExtension)!.usages].map(String), [x509.ExtendedKeyUsage.clientAuth]);

  // CSR from a key the manager never sees.
  const alg = { name: "ECDSA", namedCurve: "P-256", hash: "SHA-256" };
  const keys = (await crypto.subtle.generateKey(alg, true, ["sign", "verify"])) as CryptoKeyPair;
  const csr = await x509.Pkcs10CertificateRequestGenerator.create({ name: "CN=router-b", keys, signingAlgorithm: alg });
  const signed = await s.pki.signCsr({ caId: "vpn", csrPem: csr.toString("pem"), usage: "client" }, "test");
  assert.ok(new x509.X509Certificate(signed.certPem).subject.includes("router-b"));

  await s.pki.revoke(client.serial, "keyCompromise", "test");
  const crlPem = s.secrets.reveal("pki-ca-vpn-crl");
  assert.match(crlPem, /^-----BEGIN X509 CRL-----\n[\s\S]+\n-----END X509 CRL-----\n?$/, "the label OpenSSL reads");
  const crl = new x509.X509Crl(crlPem);
  assert.ok(await crl.verify({ publicKey: caCert.publicKey }));
  assert.ok(crl.entries.some((e) => e.serialNumber.toLowerCase().replace(/^0+/, "") === client.serial.replace(/^0+/, "")), "revoked serial listed");

  const tc = s.pki.createStaticKey("tls-crypt-vpn", ["gw-a"], "test");
  const text = s.secrets.reveal(tc.id);
  assert.match(text, /^-----BEGIN OpenVPN Static key V1-----\n([0-9a-f]{32}\n){16}-----END OpenVPN Static key V1-----\n$/);
  assert.throws(() => s.pki.createStaticKey("tls-crypt-vpn", ["gw-a"], "test"), /exists/);
  await app.close();
});
