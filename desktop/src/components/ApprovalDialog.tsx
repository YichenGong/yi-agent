import type { ApprovalRequest, Decision } from "../lib/protocol";

export function ApprovalDialog({
  request,
  onDecide,
}: {
  request: ApprovalRequest;
  onDecide: (d: Decision) => void;
}) {
  const { tool_name, tool_input } = request.params;
  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60">
      <div className="w-full max-w-lg rounded-lg border border-neutral-700 bg-neutral-900 p-4 shadow-xl">
        <h2 className="mb-2 text-sm font-semibold text-neutral-100">
          Approve tool call: <span className="font-mono">{tool_name}</span>
        </h2>
        <pre className="mb-4 max-h-64 overflow-auto rounded bg-neutral-950 p-2 font-mono text-xs whitespace-pre-wrap text-neutral-300">
          {JSON.stringify(tool_input, null, 2)}
        </pre>
        <div className="flex justify-end gap-2">
          <button
            type="button"
            onClick={() => onDecide({ decision: "deny" })}
            className="rounded-md border border-neutral-600 px-3 py-1.5 text-sm text-neutral-200 hover:bg-neutral-800"
          >
            Deny
          </button>
          <button
            type="button"
            onClick={() => onDecide({ decision: "allow_once" })}
            className="rounded-md bg-blue-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-blue-500"
          >
            Allow once
          </button>
        </div>
      </div>
    </div>
  );
}
