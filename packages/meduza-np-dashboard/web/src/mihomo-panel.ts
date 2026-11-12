export interface MihomoPanel { address: string; secret: string }

/** MetaCubeXD consumes the setup query inside the fragment; the secret never enters HTTP request URLs. */
export function mihomoPanelUrl(info: MihomoPanel, pageHostname: string): string {
  const url = new URL(`http://${info.address}`);
  const loopback = (host: string) => host === "localhost" || host === "[::1]" || host === "::1" || /^127\./.test(host);
  if (loopback(url.hostname) && !loopback(pageHostname)) throw new Error("Mihomo control API only listens on loopback. Set api.address to the node LAN address or 0.0.0.0 in Manager.");
  if (url.hostname === "0.0.0.0" || url.hostname === "[::]") url.hostname = pageHostname;
  url.pathname = "/ui/";
  url.hash = `/setup?${new URLSearchParams({ http: "true", hostname: url.hostname, port: url.port || "80", secret: info.secret })}`;
  return url.toString();
}
