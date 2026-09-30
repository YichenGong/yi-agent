import { useCallback, useEffect, useId, useRef, useState } from "react";
import type { ApprovalRequest, Decision } from "../lib/protocol";

export function ApprovalDialog({
  request,
  onDecide,
}: {
  request: ApprovalRequest;
  onDecide: (d: Decision) => void;
}) {
  const { tool_name, tool_input, prefix_suggestion, kind } = request.params;
  const headingId = useId();
  // Focus starts on the safe action: a reflexive Enter/Space must not approve
  // arbitrary (possibly destructive) tool execution.
  const denyRef = useRef<HTMLButtonElement>(null);
  // Ref is the authoritative guard (state updates are async, so a ref is needed
  // to reject two decisions fired in the same tick); state only drives the
  // disabled styling.
  const submittedRef = useRef(false);
  const [submitted, setSubmitted] = useState(false);

  const decide = useCallback(
    (d: Decision) => {
      if (submittedRef.current) return;
      submittedRef.current = true;
      setSubmitted(true);
      onDecide(d);
    },
    [onDecide],
  );

  useEffect(() => {
    // Never steal focus from a text field the user is typing in. The approval
    // arrives mid-turn, so it can land while the sidebar's rename input holds a
    // half-typed title: focusing Deny would blur that field, whose onBlur
    // commits the draft and unmounts it. Escape still denies, so the dialog
    // keeps a safe default while the user finishes typing.
    const active = document.activeElement;
    if (active instanceof HTMLInputElement || active instanceof HTMLTextAreaElement) return;
    denyRef.current?.focus();
  }, []);

  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        decide({ decision: "deny" });
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [decide]);

  const blacklisted = kind !== "Normal" ? kind.Blacklisted : null;

  return (
    <div className="absolute inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm">
      <div
        role="dialog"
        aria-labelledby={headingId}
        className="w-full max-w-lg rounded-lg border border-neutral-700 bg-neutral-900 p-4 shadow-xl"
      >
        <h2 id={headingId} className="mb-2 text-sm font-semibold text-neutral-100">
          Approve tool call: <span className="font-mono">{tool_name}</span>
        </h2>

        <div className="mb-2 text-xs">
          {blacklisted !== null ? (
            <span className="font-medium text-red-400">Blacklisted: {blacklisted}</span>
          ) : (
            <span className="text-neutral-400">Normal</span>
          )}
        </div>

        {prefix_suggestion !== null && (
          <div className="mb-2 text-xs text-neutral-400">
            Prefix:{" "}
            <code className="rounded bg-neutral-950 px-1 font-mono text-neutral-200">
              {prefix_suggestion}
            </code>
          </div>
        )}

        <pre className="mb-4 max-h-64 overflow-auto rounded bg-neutral-950 p-2 font-mono text-xs whitespace-pre-wrap text-neutral-300">
          {JSON.stringify(tool_input, null, 2)}
        </pre>

        <div className="flex justify-end gap-2">
          <button
            ref={denyRef}
            type="button"
            disabled={submitted}
            onClick={() => decide({ decision: "deny" })}
            className="rounded-md border border-neutral-600 px-3 py-1.5 text-sm text-neutral-200 hover:bg-neutral-800 disabled:opacity-50"
          >
            Deny
          </button>
          <button
            type="button"
            disabled={submitted}
            onClick={() => decide({ decision: "allow_once" })}
            className="rounded-md bg-blue-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-blue-500 disabled:opacity-50"
          >
            Allow once
          </button>
          <button
            type="button"
            disabled={submitted}
            onClick={() => decide({ decision: "always_allow_tool" })}
            className="rounded-md border border-neutral-600 px-3 py-1.5 text-sm text-neutral-200 hover:bg-neutral-800 disabled:opacity-50"
          >
            Always allow tool
          </button>
          {prefix_suggestion !== null && (
            <button
              type="button"
              disabled={submitted}
              onClick={() =>
                decide({ decision: "always_allow_prefix", prefix: prefix_suggestion })
              }
              className="rounded-md border border-neutral-600 px-3 py-1.5 text-sm text-neutral-200 hover:bg-neutral-800 disabled:opacity-50"
            >
              Always allow <span className="font-mono">{prefix_suggestion}</span>
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
