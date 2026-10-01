/** Which layer supplied the effective switch value. */
export type SwitchSource = "project" | "global" | "default";

export interface ResolvedSwitch {
  value: boolean;
  source: SwitchSource;
}

/**
 * Two-layer resolution. The project layer wins; both unset means disabled,
 * matching the Rust side so the UI never disagrees with the plugin process.
 */
export function resolveSwitch(
  global: boolean | null,
  project: boolean | null,
): ResolvedSwitch {
  if (project !== null) return { value: project, source: "project" };
  if (global !== null) return { value: global, source: "global" };
  return { value: false, source: "default" };
}

export function formatSwitch(switchOn: boolean, source: SwitchSource): string {
  return `Superpowers 看板: ${switchOn ? "on" : "off"} (${source})`;
}
