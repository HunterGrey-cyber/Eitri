//! The frame log a held-key measurement reads from a real WebKitGTK page, and the numbers made of it.
//!
//! Shared by the tests that hold `j`/`k` (the agent panel's `panel_jk_hold`, the plugin pages'
//! `plugin_page_jk_hold`): the page logs each animation frame's time and scroll position and each
//! keydown/keyup; `analyse` turns that into frame times and the view's movement per frame over the
//! steady part of the hold, and `report` prints one line of it.
use serde_json::Value;

const VOCAB: [&str; 24] = [
    "build", "the", "cache", "layout", "scroll", "when", "cursor", "moves", "across", "a", "long", "reply", "and",
    "then", "lands", "on", "short", "row", "while", "reading", "text", "that", "wraps", "over",
];

pub fn words(seed: usize, n: usize) -> String {
    (0..n)
        .map(|i| VOCAB[(seed * 7 + i * 13 + i * i) % VOCAB.len()])
        .collect::<Vec<_>>()
        .join(" ")
}

/// A long page of plain prose (120 paragraphs), nothing else: WebKitGTK's own held-arrow scroll is measured on
/// it, and a plugin page's held keys are.
pub fn plain_page() -> String {
    let body: String = (0..120)
        .map(|i| format!("<p>{}</p>", words(i, 60)))
        .collect::<Vec<_>>()
        .join("\n");
    format!("<!doctype html><html><head><style>body{{font: 15px/1.65 sans-serif; margin: 12px; max-width: 560px}}</style></head><body>{body}</body></html>")
}

// ---------------------------------------------------------------------------------------------
// The frame log.
// ---------------------------------------------------------------------------------------------

/// Starts a log of every animation frame (time, the scroller's `scrollTop`, the cursor row's top) and every
/// keydown/keyup, on `window.__jkh`. `scroller` is the expression for the element that scrolls.
pub fn start_log_js(scroller: &str) -> String {
    let sampler = std::env::var("JKH_SAMPLER").is_ok();
    format!(
        r#"JSON.stringify((() => {{
  if (window.__jkh && window.__jkh.raf) cancelAnimationFrame(window.__jkh.raf);
  const el = {scroller};
  const log = {{ frames: [], keys: [], trace: [] }};
  window.__jkh = log;
  window.__eitriHoldTrace = (f) => log.trace.push([f.t, f.scrollTop, f.cursor, f.y, f.speed]);
  const tick = (t) => {{
    const cur = el.querySelector ? el.querySelector(':scope > .row-current') : null;
    log.frames.push([t, el.scrollTop, cur ? cur.offsetTop : -1, el.scrollHeight - el.clientHeight, cur ? cur.className : '']);
    log.raf = requestAnimationFrame(tick);
  }};
  log.raf = requestAnimationFrame(tick);
  // With `JKH_SAMPLER`, the page's main thread: a 4ms timer whose late runs say how long the thread was
  // busy, and when. Off by default: the timer is load of its own.
  log.gaps = [];
  if ({sampler}) {{
    let last = performance.now();
    log.sampler = setInterval(() => {{
      const now = performance.now();
      if (now - last > 30) log.gaps.push([last, now - last]);
      last = now;
    }}, 4);
  }}
  if (!window.__jkhKeys) {{
    window.__jkhKeys = true;
    document.addEventListener('keydown', (e) => window.__jkh.keys.push([e.timeStamp, e.key, e.repeat]), true);
    document.addEventListener('keyup', (e) => window.__jkh.keys.push([e.timeStamp, 'up:' + e.key, false]), true);
  }}
  return true;
}})())"#
    )
}

pub const STOP_LOG_JS: &str = r#"JSON.stringify((() => {
  const log = window.__jkh;
  cancelAnimationFrame(log.raf);
  clearInterval(log.sampler);
  return { frames: log.frames, keys: log.keys, trace: log.trace, gaps: log.gaps };
})())"#;

#[derive(Debug, Default)]
pub struct Metrics {
    pub repeats: usize,
    pub frames: usize,
    pub dt_median: f64,
    pub dt_p95: f64,
    pub dt_max: f64,
    /// Frames that took more than 1.5 frame intervals (dropped frames).
    pub long_frames: usize,
    pub step_median: f64,
    pub step_max: f64,
    /// Frames during the hold (after the first repeat) in which the view did not move.
    pub still: usize,
    /// Frames in which it moved against the key.
    pub reversed: usize,
    /// Frames of the steady hold that began with the view already at the end the key is going to (not
    /// counted as still: there was nowhere to move).
    // Each test compiles this module on its own, and the panel's test does not read this one.
    #[allow(dead_code)]
    pub at_end: usize,
    /// Moves more than twice the median move.
    pub jumps: usize,
    /// The spread of the per-frame move, as a share of the median (0: perfectly even).
    pub step_cv: f64,
    /// Velocity in px/ms over the steady part of the hold.
    pub speed: f64,
    pub cursor_rows: usize,
    pub after_release_px: f64,
    pub after_release_ms: f64,
}

pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

pub fn analyse(log: &Value, key: &str, down: bool) -> Result<Metrics, String> {
    let frames: Vec<(f64, f64, f64, f64)> = log["frames"]
        .as_array()
        .ok_or("no frames")?
        .iter()
        .map(|f| {
            (
                f[0].as_f64().unwrap_or(0.0),
                f[1].as_f64().unwrap_or(0.0),
                f[2].as_f64().unwrap_or(-1.0),
                f[3].as_f64().unwrap_or(f64::INFINITY),
            )
        })
        .collect();
    let keys = log["keys"].as_array().ok_or("no keys")?;
    let repeats: Vec<f64> = keys
        .iter()
        .filter(|k| k[1].as_str() == Some(key) && k[2].as_bool() == Some(true))
        .map(|k| k[0].as_f64().unwrap_or(0.0))
        .collect();
    let up = keys
        .iter()
        .find(|k| k[1].as_str() == Some(&format!("up:{key}")))
        .and_then(|k| k[0].as_f64())
        .ok_or_else(|| format!("no keyup of {key} logged"))?;
    let first_repeat = *repeats
        .first()
        .ok_or_else(|| format!("no repeat of {key} arrived: the X server's auto-repeat did not run"))?;
    // The steady part: from 300ms after the first repeat (past the glide's ramp) to the key-up.
    let hold: Vec<&(f64, f64, f64, f64)> = frames
        .iter()
        .filter(|f| f.0 >= first_repeat + 300.0 && f.0 <= up)
        .collect();
    if hold.len() < 10 {
        return Err(format!("only {} frames during the hold", hold.len()));
    }
    let mut dts: Vec<f64> = hold.windows(2).map(|w| w[1].0 - w[0].0).collect();
    let sign = if down { 1.0 } else { -1.0 };
    let steps: Vec<f64> = hold.windows(2).map(|w| sign * (w[1].1 - w[0].1)).collect();
    // A frame that starts with the view already at the end it is going to has nowhere to move.
    let at_end: Vec<bool> = hold
        .windows(2)
        .map(|w| if down { w[0].1 >= w[0].3 - 0.5 } else { w[0].1 <= 0.5 })
        .collect();
    dts.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let dt_median = percentile(&dts, 0.5);
    let mut sorted_steps: Vec<f64> = steps.iter().copied().filter(|s| *s > 0.0).collect();
    sorted_steps.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let step_median = percentile(&sorted_steps, 0.5);
    let mean = steps.iter().sum::<f64>() / steps.len() as f64;
    let var = steps.iter().map(|s| (s - mean).powi(2)).sum::<f64>() / steps.len() as f64;
    let span_t = hold.last().unwrap().0 - hold.first().unwrap().0;
    let span_px = sign * (hold.last().unwrap().1 - hold.first().unwrap().1);
    let mut cursor_rows: Vec<f64> = frames
        .iter()
        .filter(|f| f.0 >= first_repeat && f.0 <= up)
        .map(|f| f.2)
        .collect();
    cursor_rows.dedup();
    let at_up = frames.iter().rfind(|f| f.0 <= up).map(|f| f.1).unwrap_or(0.0);
    let last = frames.last().map(|f| f.1).unwrap_or(0.0);
    let settled_at = frames
        .iter()
        .rev()
        .find(|f| (f.1 - last).abs() > 0.5)
        .map(|f| f.0 - up)
        .unwrap_or(0.0);
    Ok(Metrics {
        repeats: repeats.len(),
        frames: hold.len(),
        dt_median,
        dt_p95: percentile(&dts, 0.95),
        dt_max: dts.last().copied().unwrap_or(0.0),
        long_frames: dts.iter().filter(|d| **d > dt_median * 1.5).count(),
        step_median,
        step_max: sorted_steps.last().copied().unwrap_or(0.0),
        still: steps
            .iter()
            .zip(&at_end)
            .filter(|(s, end)| s.abs() < 0.5 && !**end)
            .count(),
        reversed: steps.iter().filter(|s| **s < -0.5).count(),
        at_end: at_end.iter().filter(|end| **end).count(),
        jumps: steps.iter().filter(|s| **s > 2.0 * step_median.max(1.0)).count(),
        step_cv: if mean.abs() > 1e-9 {
            var.sqrt() / mean.abs()
        } else {
            0.0
        },
        speed: if span_t > 0.0 { span_px / span_t } else { 0.0 },
        cursor_rows: cursor_rows.len(),
        after_release_px: sign * (last - at_up),
        after_release_ms: settled_at.max(0.0),
    })
}

pub fn report(tag: &str, run: &str, m: &Metrics) {
    println!(
        "{tag} {run}: repeats={} frames={} dt_median={:.1}ms dt_p95={:.1}ms dt_max={:.1}ms long_frames={} step_median={:.1}px step_max={:.1}px still={} reversed={} jumps={} step_cv={:.2} speed={:.2}px/ms cursor_rows={} after_release={:.0}px/{:.0}ms",
        m.repeats, m.frames, m.dt_median, m.dt_p95, m.dt_max, m.long_frames, m.step_median, m.step_max, m.still,
        m.reversed, m.jumps, m.step_cv, m.speed, m.cursor_rows, m.after_release_px, m.after_release_ms
    );
}
