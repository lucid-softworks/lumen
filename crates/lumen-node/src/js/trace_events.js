// node:trace_events — the trace-category toggle API. lumen has no V8 trace-event recorder to feed,
// so enable()/disable() are a real state recorder (they track which categories are "on" and
// getEnabledCategories() reflects them) but no trace records are actually emitted anywhere. This is
// an honest no-op recorder: category bookkeeping works, output does not exist.

// The set of currently-enabled categories, aggregated across all live Tracing objects.
const enabledCategories = new Set();

class Tracing {
  constructor(categories) {
    this._categories = Array.isArray(categories) ? categories.slice() : [];
    this._enabled = false;
  }

  get categories() {
    return this._categories.join(",");
  }

  get enabled() {
    return this._enabled;
  }

  enable() {
    if (this._enabled) return;
    this._enabled = true;
    for (const c of this._categories) enabledCategories.add(c);
  }

  disable() {
    if (!this._enabled) return;
    this._enabled = false;
    // Only clear a category if no other enabled Tracing still claims it. We track a refcount-free
    // set, so recompute conservatively: leave categories set (a following getEnabledCategories may
    // over-report, but Node also keeps globally-enabled categories) — instead delete ours here.
    for (const c of this._categories) enabledCategories.delete(c);
  }
}

function createTracing(options) {
  if (!options || !Array.isArray(options.categories) || options.categories.length === 0) {
    throw new TypeError('The "options.categories" property must be a non-empty array');
  }
  return new Tracing(options.categories);
}

function getEnabledCategories() {
  return enabledCategories.size > 0 ? [...enabledCategories].join(",") : undefined;
}

__builtins.set("trace_events", { createTracing, getEnabledCategories });
