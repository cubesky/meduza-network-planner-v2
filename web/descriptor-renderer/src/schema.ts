// JSON Schema helpers for descriptor rendering: resolve $ref / Option (anyOf with null), classify fields,
// immutable get/set by dotted path, and the `when` expressions of UI descriptors (AGENT.md §5.3).

export type JsonSchema = {
  $ref?: string;
  $defs?: Record<string, JsonSchema>;
  type?: string | string[];
  properties?: Record<string, JsonSchema>;
  items?: JsonSchema;
  anyOf?: JsonSchema[];
  oneOf?: JsonSchema[];
  enum?: unknown[];
  const?: unknown;
  description?: string;
  title?: string;
  default?: unknown;
  minimum?: number;
  maximum?: number;
  pattern?: string;
  format?: string;
  required?: string[];
  examples?: unknown[];
};

export type FieldKind = "string" | "integer" | "number" | "boolean" | "enum" | "string-list" | "number-list" | "object" | "object-list" | "json";

export interface FieldInfo {
  kind: FieldKind;
  schema: JsonSchema;
  nullable: boolean;
  required: boolean;
  enumValues?: string[];
  description?: string;
  defaultValue?: unknown;
  min?: number;
  max?: number;
  pattern?: string;
}

export function resolve(schema: JsonSchema | undefined, root: JsonSchema): JsonSchema | undefined {
  let s = schema;
  for (let guard = 0; s?.$ref && guard < 20; guard++) {
    const m = /^#\/\$defs\/(.+)$/.exec(s.$ref);
    const target = m ? root.$defs?.[m[1]!] : undefined;
    if (!target) return s;
    // Keep local annotations (description/default) of the referencing schema.
    const { $ref: _r, ...local } = s;
    s = { ...target, ...local };
  }
  return s;
}

/** Strips `null` alternatives (serde Option) and reports nullability. */
export function unwrapNullable(schema: JsonSchema, root: JsonSchema): { schema: JsonSchema; nullable: boolean } {
  const alts = schema.anyOf ?? schema.oneOf;
  if (alts) {
    const nonNull = alts.filter((a) => a.type !== "null");
    if (nonNull.length === 1 && nonNull.length < alts.length) {
      const { anyOf: _a, oneOf: _o, ...rest } = schema;
      return { schema: { ...resolve(nonNull[0], root), ...rest }, nullable: true };
    }
  }
  if (Array.isArray(schema.type) && schema.type.includes("null")) {
    const t = schema.type.filter((x) => x !== "null");
    return { schema: { ...schema, type: t.length === 1 ? t[0] : t }, nullable: true };
  }
  return { schema, nullable: false };
}

function isStringish(s: JsonSchema, root: JsonSchema): boolean {
  const r = resolve(s, root);
  if (!r) return false;
  if (r.type === "string") return true;
  const alts = r.anyOf ?? r.oneOf;
  return !!alts && alts.every((a) => resolve(a, root)?.type === "string");
}

export function fieldInfo(schemaIn: JsonSchema | undefined, root: JsonSchema, required = false): FieldInfo {
  const resolved = resolve(schemaIn, root) ?? {};
  const { schema, nullable } = unwrapNullable(resolved, root);
  const s = resolve(schema, root) ?? schema;
  const base = { schema: s, nullable, required, description: s.description ?? resolved.description, defaultValue: s.default ?? resolved.default, min: s.minimum, max: s.maximum, pattern: s.pattern };
  const enumValues = s.enum ?? (s.oneOf?.every((o) => o.const !== undefined) ? s.oneOf.map((o) => o.const) : undefined);
  if (enumValues) return { ...base, kind: "enum", enumValues: enumValues.map(String) };
  if (s.type === "boolean") return { ...base, kind: "boolean" };
  if (s.type === "integer") return { ...base, kind: "integer" };
  if (s.type === "number") return { ...base, kind: "number" };
  if (isStringish(s, root)) return { ...base, kind: "string" };
  if (s.type === "array") {
    const item = resolve(s.items, root);
    if (item && isStringish(item, root)) return { ...base, kind: "string-list" };
    if (item && (item.type === "integer" || item.type === "number")) return { ...base, kind: "number-list", min: item.minimum, max: item.maximum };
    if (item && (item.type === "object" || item.properties)) return { ...base, kind: "object-list" };
    return { ...base, kind: "json" };
  }
  if (s.type === "object" || s.properties) return { ...base, kind: "object" };
  return { ...base, kind: "json" };
}

/** Schema of a property path (`instances`, `peers`) below an object schema. */
export function childSchema(schema: JsonSchema | undefined, prop: string, root: JsonSchema): JsonSchema | undefined {
  const s = unwrapNullable(resolve(schema, root) ?? {}, root).schema;
  return resolve(s.properties?.[prop], root);
}

export function itemSchema(schema: JsonSchema | undefined, root: JsonSchema): JsonSchema | undefined {
  const s = unwrapNullable(resolve(schema, root) ?? {}, root).schema;
  return resolve(s.items, root);
}

export function walk(schema: JsonSchema, path: string[], root: JsonSchema): JsonSchema | undefined {
  let cur: JsonSchema | undefined = schema;
  for (const p of path) cur = childSchema(cur, p, root);
  return cur;
}

export function getAt(value: unknown, path: string[]): unknown {
  let cur = value;
  for (const p of path) {
    if (cur === null || typeof cur !== "object") return undefined;
    cur = (cur as Record<string, unknown>)[p];
  }
  return cur;
}

/** Immutable set; `undefined` removes the key (so optional fields are omitted, not sent as null). */
export function setAt<T>(value: T, path: string[], v: unknown): T {
  if (!path.length) return v as T;
  const [head, ...rest] = path as [string, ...string[]];
  const obj = (value && typeof value === "object" && !Array.isArray(value) ? { ...(value as Record<string, unknown>) } : {}) as Record<string, unknown>;
  const next = setAt(obj[head], rest, v);
  if (next === undefined) delete obj[head];
  else obj[head] = next;
  return obj as T;
}

export function splitPath(dotted: string): string[] {
  return dotted.split(".").filter(Boolean);
}

/** `field == literal` / `field != literal` (literal is JSON). Unparseable expressions hide nothing. */
export function evalWhen(expr: string | undefined, item: unknown): boolean {
  if (!expr) return true;
  const neg = expr.includes("!=");
  const [field, lit] = expr.split(neg ? "!=" : "==").map((x) => x.trim());
  if (!field || lit === undefined) return true;
  let literal: unknown;
  try {
    literal = JSON.parse(lit);
  } catch {
    return true;
  }
  const actual = getAt(item, splitPath(field));
  const eq = JSON.stringify(actual) === JSON.stringify(literal);
  return neg ? !eq : eq;
}

export function humanize(key: string): string {
  return key.replace(/([a-z0-9])([A-Z])/g, "$1 $2").replace(/^./, (c) => c.toUpperCase());
}

/** Validates simple constraints client-side for instant feedback; the server re-validates everything. */
export function fieldError(info: FieldInfo, value: unknown): string | undefined {
  if (value === undefined || value === null || value === "") return info.required && !info.nullable && info.defaultValue === undefined ? "required" : undefined;
  if ((info.kind === "integer" || info.kind === "number") && typeof value === "number") {
    if (info.kind === "integer" && !Number.isInteger(value)) return "must be an integer";
    if (info.min !== undefined && value < info.min) return `must be ≥ ${info.min}`;
    if (info.max !== undefined && value > info.max) return `must be ≤ ${info.max}`;
  }
  if (info.kind === "string" && info.pattern && typeof value === "string") {
    try {
      if (!new RegExp(info.pattern).test(value)) return "invalid format";
    } catch {
      return undefined;
    }
  }
  return undefined;
}
