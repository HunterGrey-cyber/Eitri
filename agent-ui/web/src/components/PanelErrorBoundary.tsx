import { Component } from "react";
import type { ErrorInfo, ReactNode } from "react";

type Props = {
  /** Which boundary this is, for the console line a failure leaves (`"panel"`, `"chooser"`, ...). */
  name: string;
  children: ReactNode;
  /** Called once, when a child throws while rendering. An overlay closes itself here (`App.tsx`), so
   *  the boundary that caught it is unmounted with it and the next opening starts fresh. */
  onError?: (error: Error) => void;
  /** What to draw in place of the children once one has thrown. Absent: nothing, so an overlay's
   *  own site simply reads as closed. */
  fallback?: (error: Error) => ReactNode;
};

type State = { error: Error | null };

/** K03 (kbux 2026-09-29): one throw during render unmounted the whole React tree -- `#root` went empty,
 *  every key went dead, nothing was logged. React keeps a tree only where an error boundary catches.
 *  Overlays close through `onError`; the root draws `fallback` instead of nothing. */
export class PanelErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  /** Anything can be thrown, not only an `Error`; `fallback` and `onError` are typed to get one. */
  static getDerivedStateFromError(error: unknown): State {
    return { error: error instanceof Error ? error : new Error(String(error)) };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error(`[agent-ui] ${this.props.name} failed:`, error, info.componentStack);
    this.props.onError?.(this.state.error ?? error);
  }

  render() {
    return this.state.error !== null ? (this.props.fallback?.(this.state.error) ?? null) : this.props.children;
  }
}
