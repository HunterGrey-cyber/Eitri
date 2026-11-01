import { runLongReplyBench, runLongSessionBench, type BenchHost, type ReplyShape } from "./longSessionBench";

/* Run the long-session benchmark as a separately bundled script in real WebKit after `dist/index.html` loads.
   Use the shipped page unchanged; this script supplies the timing and conversation data. */

type BenchHandler = { postMessage: (message: string) => void };
const benchHandler = (window as unknown as { webkit: { messageHandlers: { eitriBench: BenchHandler } } }).webkit.messageHandlers.eitriBench;

/** One turn of the event loop, after everything already queued: React's scheduler posts its work the same way. */
const macrotask = () =>
  new Promise<void>((resolve) => {
    const channel = new MessageChannel();
    channel.port1.onmessage = () => resolve();
    channel.port2.postMessage(0);
  });

const nextFrame = () => new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));

const host: BenchHost = {
  work: async (act) => {
    const t0 = performance.now();
    act();
    // Render and commit, then the passive effects they scheduled, then whatever layout is still owed.
    for (let i = 0; i < 3; i++) await macrotask();
    void document.documentElement.offsetHeight;
    return performance.now() - t0;
  },
  pause: async () => {
    await nextFrame();
    await nextFrame();
  },
};

const post = (message: unknown) => benchHandler.postMessage(JSON.stringify(message));
const mounted = async () => {
  for (let i = 0; i < 400 && window.__eitriDispatch === undefined; i++) await new Promise((resolve) => setTimeout(resolve, 25));
};

(window as unknown as { __longReplyBench: (bytes: number, shape: ReplyShape) => void }).__longReplyBench = (bytes, shape) => {
  mounted().then(() => runLongReplyBench(bytes, shape, host)).then(
    (result) => post({ result }),
    (error: unknown) => post({ error: String(error) }),
  );
};

(window as unknown as { __longSessionBench: (rows: number) => void }).__longSessionBench = (rows) => {
  // Held for the memory reading the harness takes when it hears a stage.
  const stage = async (name: string) => {
    post({ stage: name });
    await new Promise((resolve) => setTimeout(resolve, 2000));
  };
  mounted().then(() => runLongSessionBench(rows, host, 60, stage)).then(
    (result) => post({ result }),
    (error: unknown) => post({ error: String(error) }),
  );
};
