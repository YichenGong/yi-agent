import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { connectHostOrNull, type PluginHostApi } from "./host";
import "./styles.css";

/**
 * Entry point.
 *
 * The host decides which view to show by opening a distinct entry document, so
 * this module reads the requested view from the URL fragment rather than from a
 * host global: `#settings` renders the settings panel, anything else the board.
 * Reading it from the URL keeps the bundle a plain static asset — the host does
 * not need to know how the page routes internally.
 */
function requestedView(): "main" | "settings" {
  return window.location.hash.replace(/^#/, "") === "settings" ? "settings" : "main";
}

const root = document.getElementById("root");
if (root === null) throw new Error("the page has no #root element");

let host: PluginHostApi | null = null;
try {
  host = await connectHostOrNull();
} catch {
  // A failed handshake is rendered as "not connected" rather than a blank page:
  // an empty board and an unconnected board must not look the same.
  host = null;
}

createRoot(root).render(
  <StrictMode>
    <App host={host} view={requestedView()} />
  </StrictMode>,
);
