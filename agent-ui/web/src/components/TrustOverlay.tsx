import type { Dispatch, RefObject, SetStateAction } from "react";
import { nextRequestId, postToRust } from "../bridge";
import type { CommandRecord, PageModel } from "../page_model";
import { PanelErrorBoundary } from "./PanelErrorBoundary";
import { TrustPrompt } from "./TrustPrompt";

/** The trust question, over whichever layout is on screen. A render that throws puts the start off, as
 *  `Escape` would: leaving the tab asking with nothing to answer would strand it. */
export function TrustOverlay({ confirm, setConfirm, overlayFailed, trackRequest, trustOverlayRef }: {
  confirm: PageModel["confirm"];
  setConfirm: Dispatch<SetStateAction<PageModel["confirm"]>>;
  overlayFailed: (what: string, close: () => void) => void;
  trackRequest: (id: string, record: CommandRecord) => void;
  trustOverlayRef: RefObject<HTMLDivElement>;
}) {
  return (
    confirm?.kind === "trust" ? (
      <PanelErrorBoundary
        name="trust prompt"
        onError={() =>
          overlayFailed("trust prompt", () => {
            const cancelId = nextRequestId();
            trackRequest(cancelId, { kind: "trust", tab: confirm.tab });
            postToRust({ type: "trust_cancel", request_id: cancelId, tab: confirm.tab, nonce: confirm.nonce });
            setConfirm(null);
          })
        }
      >
        <TrustPrompt ref={trustOverlayRef} envelope={confirm.view} />
      </PanelErrorBoundary>
    ) : null
  );
}
