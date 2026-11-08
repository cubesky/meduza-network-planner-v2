import type { ComponentChildren } from "preact";
import { useState } from "preact/hooks";
import { Badge, Button, StateBadge, StatusDot, stateTone, t } from "./index.tsx";
import { buildInstanceTree, type InstanceTreeNode, type RuntimeStatus } from "./instance-tree-model.ts";

export { buildInstanceTree, withWireGuardNames, type RuntimeStatus } from "./instance-tree-model.ts";

function label(node: InstanceTreeNode): string {
  if (node.item?.kind === "process") {
    if (!node.label) return t("Main process");
    if (node.path.startsWith("wireguard/") && node.label === "endpoints") return t("Endpoint resolver");
  }
  if (!node.path.includes("/") && node.path === "wireguard") return "WireGuard";
  return node.label;
}

function kind(item?: RuntimeStatus): string {
  switch (item?.kind) {
    case undefined: return t("Group");
    case "process": return t("Process");
    case "wireguard-interface": return t("Interface");
    case "wireguard-peer": return t("Peer");
    default: return item?.kind ?? "";
  }
}

/** Expandable status table: virtual folders have counts, never a fabricated healthy state. */
export function InstanceTree(props: { items: RuntimeStatus[]; empty?: string; actions?: (item: RuntimeStatus) => ComponentChildren }) {
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  const roots = buildInstanceTree(props.items);
  const stats = new Map<string, { total: number; issues: number }>();
  const count = (node: InstanceTreeNode) => {
    const tone = node.item ? stateTone(node.item.state) : undefined;
    const result = { total: node.item ? 1 : 0, issues: tone === "bad" || tone === "warn" ? 1 : 0 };
    for (const child of node.children) {
      const sub = count(child);
      result.total += sub.total;
      result.issues += sub.issues;
    }
    stats.set(node.key, result);
    return result;
  };
  roots.forEach(count);
  const visible: Array<{ node: InstanceTreeNode; depth: number }> = [];
  const visit = (node: InstanceTreeNode, depth: number) => {
    visible.push({ node, depth });
    if (!collapsed.has(node.key)) for (const child of node.children) visit(child, depth + 1);
  };
  roots.forEach((node) => visit(node, 0));
  if (!props.items.length) return <div class="state-box">{props.empty ?? t("No entries")}</div>;
  return <div class="instance-tree">
    <div class="row instance-tree-tools">
      <Button small onClick={() => setCollapsed(new Set())}>{t("Expand all")}</Button>
      <Button small onClick={() => setCollapsed(new Set([...stats.keys()]))}>{t("Collapse all")}</Button>
    </div>
    <div class="table-wrap"><table class="table instance-tree-table">
      <thead><tr><th>{t("Service / instance")}</th><th>{t("Kind")}</th><th>{t("State")}</th><th>{t("Reason")}</th><th>{t("Restarts")}</th>{props.actions && <th />}</tr></thead>
      <tbody>{visible.map(({ node, depth }) => {
        const item = node.item;
        const summary = stats.get(node.key)!;
        const expanded = !collapsed.has(node.key);
        const shortId = node.path.split("/").pop()!;
        const identity = [label(node) !== shortId && shortId, typeof item?.device === "string" && item.device].filter(Boolean).join(" · ");
        return <tr key={node.key} data-status-id={item?.id} data-status-kind={item?.kind}>
          <td><div class="instance-tree-name" style={{ paddingLeft: `${depth * 22}px` }}>
            {node.children.length ? <button type="button" class="instance-tree-toggle" aria-expanded={expanded}
              aria-label={t("Toggle {name}", { name: label(node) })} title={node.path}
              onClick={() => setCollapsed((old) => { const next = new Set(old); next.has(node.key) ? next.delete(node.key) : next.add(node.key); return next; })}>{expanded ? "▾" : "▸"}</button>
              : <span class="instance-tree-branch" aria-hidden="true">└</span>}
            {item && <StatusDot state={item.state} />}
            <span title={item?.id ?? node.path}><strong>{label(node)}</strong>{identity && <span class="small faint instance-tree-identity">{identity}</span>}</span>
            {node.children.length > 0 && <span class="small faint">{summary.total}</span>}
          </div></td>
          <td class="small muted">{kind(item)}</td>
          <td><span class="row instance-tree-state">{item && <StateBadge state={item.state} />}
            {node.children.length > 0 && summary.issues > (item && ["bad", "warn"].includes(stateTone(item.state)) ? 1 : 0) && <Badge tone="warn">{t("{count} issues", { count: summary.issues })}</Badge>}
          </span></td>
          <td class="instance-tree-reason">{item?.reason ?? (typeof item?.lastError === "string" ? item.lastError : "")}</td>
          <td>{item?.kind === "process" ? String(item.restarts ?? 0) : "—"}</td>
          {props.actions && <td class="actions">{item && props.actions(item)}</td>}
        </tr>;
      })}</tbody>
    </table></div>
  </div>;
}
