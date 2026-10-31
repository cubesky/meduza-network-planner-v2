// Package catalog (AGENT.md §5.3): manifests, UI descriptors and config schemas of every known package,
// loaded from verified package metadata shipped with the manager image. Node capability views combine this
// catalog with what each node reports as installed/registered.

import { readFileSync, existsSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { createRequire } from "node:module";
import type { ValidateFunction, ErrorObject } from "ajv";
import { canonicalDigest } from "./canonical.js";

const require = createRequire(import.meta.url);
// ajv's 2020-12 build is CommonJS; load it through require for NodeNext compatibility.
const Ajv2020 = require("ajv/dist/2020.js") as new (opts: Record<string, unknown>) => {
  compile(schema: unknown): ValidateFunction;
  addSchema(schema: unknown, key?: string): unknown;
};

export interface PackageManifest {
  manifestVersion: number;
  package: string;
  version: string;
  role: string;
  plannerApi: { min: number; max: number };
  requires: string[];
  conflicts: string[];
  provides: string[];
  capabilitySwitches?: CapabilitySwitch[];
  hostNetworkCapabilities?: string[];
  providerSlot?: string;
  schemas?: { config?: string; status?: string };
  ui?: { descriptor: string; version: number };
  entrypoint?: string;
  operations?: string[];
  consumes?: string[];
  platforms?: string[];
}

/** A capability a configuration switch turns on separately from its service (crates/protocol `CapabilitySwitch`). */
export interface CapabilitySwitch {
  capability: string;
  pointer: string;
  equals?: unknown;
  item?: Record<string, unknown>;
}

export interface CatalogEntry {
  manifest: PackageManifest;
  manifestDigest: string;
  shortName: string;
  ui: unknown | null;
  uiDigest: string | null;
  configSchema: unknown | null;
  validateConfig: ValidateFunction | null;
}

export const PREFIX = "meduza-np-";
export const CONFLICTS: Array<[string, string]> = [
  ["meduza-np-mihomo-openwrt", "meduza-np-mihomo"],
  ["meduza-np-mihomo-openwrt", "meduza-np-mihomo-pkg"],
  ["meduza-np-dashboard-openwrt", "meduza-np-dashboard"],
];

export function shortName(pkg: string): string {
  return pkg.startsWith(PREFIX) ? pkg.slice(PREFIX.length) : pkg;
}

export function packagesConflict(a: string, b: string): boolean {
  return CONFLICTS.some(([x, y]) => (x === a && y === b) || (x === b && y === a));
}

export function formatAjvErrors(errors: ErrorObject[] | null | undefined, prefix = ""): Array<{ path: string; message: string }> {
  return (errors ?? []).map((e) => ({ path: `${prefix}${e.instancePath}` || "/", message: e.message ?? "invalid" }));
}

export function newAjv() {
  return new Ajv2020({ strict: false, allErrors: true, validateFormats: false });
}

export class PackageCatalog {
  readonly entries = new Map<string, CatalogEntry>();
  readonly schemas = new Map<string, ValidateFunction>();
  readonly rawSchemas = new Map<string, unknown>();

  /** `packagesDir` holds `<package>/manifest.json` (+ ui/, schema/); `schemasDir` the shared protocol schemas. */
  constructor(packagesDir: string, schemasDir: string) {
    const ajv = newAjv();
    if (existsSync(schemasDir)) {
      for (const file of readdirSync(schemasDir)) {
        if (!file.endsWith(".json")) continue;
        const schema = JSON.parse(readFileSync(join(schemasDir, file), "utf8"));
        this.schemas.set(file.replace(/\.json$/, ""), ajv.compile(schema));
        this.rawSchemas.set(file.replace(/\.json$/, ""), schema);
      }
    }
    if (!existsSync(packagesDir)) return;
    for (const dir of readdirSync(packagesDir)) {
      const manifestPath = join(packagesDir, dir, "manifest.json");
      if (!existsSync(manifestPath)) continue;
      const manifest = JSON.parse(readFileSync(manifestPath, "utf8")) as PackageManifest;
      const readOpt = (rel: string | undefined) => {
        if (!rel) return null;
        const p = join(packagesDir, dir, rel);
        return existsSync(p) ? (JSON.parse(readFileSync(p, "utf8")) as unknown) : null;
      };
      const ui = readOpt(manifest.ui?.descriptor);
      const configSchema = readOpt(manifest.schemas?.config);
      this.entries.set(manifest.package, {
        manifest,
        manifestDigest: canonicalDigest(manifest),
        shortName: shortName(manifest.package),
        ui,
        uiDigest: ui ? canonicalDigest(ui) : null,
        configSchema,
        validateConfig: configSchema ? newAjv().compile(configSchema) : null,
      });
    }
  }

  get(pkg: string): CatalogEntry | undefined {
    return this.entries.get(pkg);
  }

  byShortName(short: string): CatalogEntry | undefined {
    return this.entries.get(PREFIX + short);
  }

  list(): CatalogEntry[] {
    return [...this.entries.values()].sort((a, b) => a.manifest.package.localeCompare(b.manifest.package));
  }

  /** Validates a document against a shared protocol schema (`node-config`, `node-report`, ...). */
  validate(schema: string, value: unknown): Array<{ path: string; message: string }> {
    const fn = this.schemas.get(schema);
    if (!fn) return [{ path: "/", message: `schema ${schema} not available` }];
    return fn(value) ? [] : formatAjvErrors(fn.errors);
  }
}
