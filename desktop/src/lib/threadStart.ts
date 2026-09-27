/** params for `thread/start`: include `cwd` only when provided. */
export function threadStartParams(cwd?: string): Record<string, string> {
  return cwd ? { cwd } : {};
}
