import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "./App";
import { RemoteProjectChooser } from "./RemoteProjectChooser";
import { PanelErrorBoundary } from "./components/PanelErrorBoundary";
import "./index.css";

const container = document.getElementById("root");
if (!container) {
  throw new Error("agent-ui: #root element missing from index.html");
}
// A throw while rendering would unmount everything -- `#root` empty, every key dead, nothing logged --
// until `prefix r`. The conversation lives in Rust, so a reload brings all of it back.
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
      {window.location.pathname === "/open-on-host" ? <RemoteProjectChooser /> : <App />}
    </PanelErrorBoundary>
  </StrictMode>,
);
