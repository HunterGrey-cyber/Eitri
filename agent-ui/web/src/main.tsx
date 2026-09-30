import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "./App";
import { PanelErrorBoundary } from "./components/PanelErrorBoundary";
import "./index.css";

const container = document.getElementById("root");
if (!container) {
  throw new Error("agent-ui: #root element missing from index.html");
}
// K03 (kbux 2026-09-29): a throw while rendering used to unmount everything -- `#root` empty, every key dead,
// nothing logged -- until `prefix r`. The conversation lives in Rust, so a reload brings all of it back.
createRoot(container).render(
  <StrictMode>
    <PanelErrorBoundary
      name="panel"
      fallback={(e) => (
        <div className="panel-crashed" role="alert">
          The panel stopped drawing ({e.message}). prefix r (Ctrl+b r by default) reloads it; the conversation lives in
          neovibe and comes back.
        </div>
      )}
    >
      <App />
    </PanelErrorBoundary>
  </StrictMode>,
);
