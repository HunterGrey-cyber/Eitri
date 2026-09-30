import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "./App";
import { PanelErrorBoundary } from "./components/PanelErrorBoundary";
import "./index.css";

const container = document.getElementById("root");
if (!container) {
  throw new Error("agent-ui: #root element missing from index.html");
}
// A render failure can unmount the panel, leaving `#root` empty and every key inactive.
// The conversation lives in Rust, so reloading restores it.
createRoot(container).render(
  <StrictMode>
    <PanelErrorBoundary
      name="panel"
      fallback={(e) => (
        <div className="panel-crashed" role="alert">
          The panel stopped drawing ({e.message}). prefix r (Ctrl+b r by default) reloads it; the conversation lives in
          Eitri and comes back.
        </div>
      )}
    >
      <App />
    </PanelErrorBoundary>
  </StrictMode>,
);
