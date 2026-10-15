// Event, EventTarget, AbortController, AbortSignal.
// Fragment of the bootstrap function body: all files share one scope and
// are concatenated in file-name order by build.rs.

// ------------------------------------------------------- Event, Abort
const kStop = Symbol("stop");
class Event {
  constructor(type, init = {}) {
    if (arguments.length === 0) throw new TypeError("The \"type\" argument must be specified");
    this.type = String(type);
    this.bubbles = !!init.bubbles;
    this.cancelable = !!init.cancelable;
    this.composed = !!init.composed;
    this.defaultPrevented = false;
    this.timeStamp = ops.now();
    this.target = null;
    this.currentTarget = null;
    this[kStop] = false;
  }
  preventDefault() { if (this.cancelable) this.defaultPrevented = true; }
  stopPropagation() {}
  stopImmediatePropagation() { this[kStop] = true; }
}

class EventTarget {
  #listeners = new Map();
  addEventListener(type, callback, options) {
    if (callback == null) return;
    const once = typeof options === "object" && options !== null && !!options.once;
    const capture = typeof options === "boolean" ? options : !!(options && options.capture);
    const signal = typeof options === "object" && options !== null ? options.signal : undefined;
    if (signal && signal.aborted) return;
    let list = this.#listeners.get(type);
    if (!list) this.#listeners.set(type, (list = []));
    if (list.some((l) => l.callback === callback && l.capture === capture)) return;
    const entry = { callback, once, capture };
    list.push(entry);
    if (signal) signal.addEventListener("abort", () => this.removeEventListener(type, callback, { capture }));
  }
  removeEventListener(type, callback, options) {
    const list = this.#listeners.get(type);
    if (!list) return;
    const capture = typeof options === "boolean" ? options : !!(options && options.capture);
    const i = list.findIndex((l) => l.callback === callback && l.capture === capture);
    if (i !== -1) list.splice(i, 1);
  }
  dispatchEvent(event) {
    if (!(event instanceof Event)) throw new TypeError("The \"event\" argument must be an instance of Event");
    event.target = this;
    event.currentTarget = this;
    const handler = this["on" + event.type];
    const list = (this.#listeners.get(event.type) || []).slice();
    if (typeof handler === "function") list.unshift({ callback: handler, once: false });
    for (const l of list) {
      if (l.once) this.removeEventListener(event.type, l.callback, { capture: l.capture });
      try {
        if (typeof l.callback === "function") l.callback.call(this, event);
        else l.callback.handleEvent(event);
      } catch (e) {
        queueMicrotask(() => { throw e; });
      }
      if (event[kStop]) break;
    }
    event.currentTarget = null;
    return !event.defaultPrevented;
  }
}

const kAbort = Symbol("abort");
class AbortSignal extends EventTarget {
  #aborted = false;
  #reason = undefined;
  onabort = null;
  get aborted() { return this.#aborted; }
  get reason() { return this.#reason; }
  throwIfAborted() { if (this.#aborted) throw this.#reason; }
  [kAbort](reason) {
    if (this.#aborted) return;
    this.#aborted = true;
    this.#reason = reason === undefined ? new DOMException("This operation was aborted", "AbortError") : reason;
    this.dispatchEvent(new Event("abort"));
  }
  static abort(reason) {
    const s = new AbortSignal();
    s[kAbort](reason);
    return s;
  }
  static timeout(ms) {
    const s = new AbortSignal();
    addTimer(() => s[kAbort](new DOMException("The operation was aborted due to timeout", "TimeoutError")), ms, [], false, true);
    return s;
  }
  static any(signals) {
    const s = new AbortSignal();
    for (const sig of signals) {
      if (sig.aborted) { s[kAbort](sig.reason); return s; }
    }
    for (const sig of signals) sig.addEventListener("abort", () => s[kAbort](sig.reason), { once: true });
    return s;
  }
}

class AbortController {
  #signal = new AbortSignal();
  get signal() { return this.#signal; }
  abort(reason) { this.#signal[kAbort](reason); }
}

define(g, "Event", Event);
define(g, "EventTarget", EventTarget);
define(g, "AbortSignal", AbortSignal);
define(g, "AbortController", AbortController);
