(() => {
  const nativeFrame = window.requestAnimationFrame.bind(window);
  const epoch = Date.now();
  let now = 0, serial = 0, scheduled = 0, completed = 0;
  const timers = new Map(), frames = new Map(), stamps = new WeakMap();
  Object.defineProperty(performance, 'now', { value: () => now });
  Date.now = () => epoch + now;
  Object.defineProperty(Event.prototype, 'timeStamp', { get() {
    if (!stamps.has(this)) stamps.set(this, now);
    return stamps.get(this);
  }});
  window.setTimeout = (fn, delay = 0, ...args) => {
    const id = ++serial;
    timers.set(id, { at: now + Math.max(0, Number(delay) || 0), fn: () => fn(...args), interval: null });
    return id;
  };
  window.setInterval = (fn, delay = 0, ...args) => {
    const id = ++serial, interval = Math.max(1, Number(delay) || 0);
    timers.set(id, { at: now + interval, fn: () => fn(...args), interval });
    return id;
  };
  window.clearTimeout = window.clearInterval = id => timers.delete(id);
  window.requestAnimationFrame = fn => { const id = ++serial; frames.set(id, fn); return id; };
  window.cancelAnimationFrame = id => frames.delete(id);
  window.__eitriTestClock = {
    step(delta, token = scheduled + 1) {
      if (token <= scheduled) {
        if (token <= completed) window.webkit.messageHandlers.eitriTestClock.postMessage(String(token));
        return;
      }
      scheduled = token;
      nativeFrame(() => {
        now += delta;
        const readyFrames = [...frames];
        let turns = 0;
        for (;;) {
          const ready = [...timers].filter(([, t]) => t.at <= now).sort((a, b) => a[1].at - b[1].at || a[0] - b[0]);
          if (!ready.length) break;
          for (const [id, timer] of ready) {
            if (++turns > 10000) throw new Error('the test clock exceeded 10000 timer callbacks in one turn');
            if (!timers.has(id)) continue;
            if (timer.interval === null) timers.delete(id); else timer.at += timer.interval;
            timer.fn();
          }
        }
        for (const [id, fn] of readyFrames) {
          if (!frames.has(id)) continue;
          frames.delete(id);
          fn(now);
        }
        // A native frame after the callbacks lets style, allocation and scrolling reach a paint.
        nativeFrame(() => {
          completed = token;
          window.webkit.messageHandlers.eitriTestClock.postMessage(String(token));
        });
      });
    }
  };
})();
