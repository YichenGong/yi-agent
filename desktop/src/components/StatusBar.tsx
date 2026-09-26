export function StatusBar({
  cwd,
  model,
  status,
  usage,
}: {
  cwd: string | null;
  model: string | null;
  status: string;
  usage: { model: string; input: number; output: number } | null;
}) {
  const connected = status === "connected";
  return (
    <div className="flex items-center gap-3 border-b border-neutral-800 bg-neutral-900 px-4 py-2 text-xs text-neutral-400">
      <span
        className={`inline-block h-2 w-2 rounded-full ${connected ? "bg-emerald-500" : "bg-red-500"}`}
        title={status}
      />
      <span className="text-neutral-300">{status}</span>
      {cwd && <span className="truncate font-mono">{cwd}</span>}
      {model && <span className="truncate font-mono">{model}</span>}
      {usage && (
        <span className="ml-auto font-mono">
          {usage.input} in / {usage.output} out
        </span>
      )}
    </div>
  );
}
