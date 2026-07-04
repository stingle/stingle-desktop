import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { initVideoServer } from "./api";
import "./styles.css";

// Suppress the webview's native context menu (Save image as / Copy image / etc.).
// For an E2E-encrypted gallery it would leak decrypted bytes to disk/clipboard
// and expose internal stingle:// URLs. Still allowed on editable fields so
// copy/paste keeps working in inputs.
document.addEventListener("contextmenu", (e) => {
  const el = e.target as HTMLElement | null;
  if (el?.closest("input, textarea, [contenteditable='true']")) return;
  e.preventDefault();
});

// On Linux videos stream from a loopback HTTP server (WebKitGTK can't play
// custom-scheme media); resolve its address before the first render so
// videoUrl() is always ready. One fast IPC call — imperceptible at startup.
initVideoServer().finally(() => {
  ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
    <React.StrictMode>
      <App />
    </React.StrictMode>,
  );
});
