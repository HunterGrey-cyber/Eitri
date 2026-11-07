import { createRoot } from "react-dom/client";
import { PanelErrorBoundary } from "../components/PanelErrorBoundary";
import { detectTransport } from "../transport";
import { TranscriptApp } from "./TranscriptApp";
import "../index.css";
import "./transcript.css";

const container = document.getElementById("root");
if (!container) {
  throw new Error("agent-ui: #root element missing from index.transcript.html");
}
// No StrictMode: its doubled effects would post `ready` twice to a host that counts what the page says.
createRoot(container).render(
  <PanelErrorBoundary
    name="transcript"
    fallback={(e) => (
      <div className="panel-crashed" role="alert">
        The transcript stopped drawing ({e.message}). Close the chat and open it again; the conversation lives at the
        desk and comes back.
      </div>
    )}
  >
    <TranscriptApp transport={detectTransport(window)} />
  </PanelErrorBoundary>,
);
