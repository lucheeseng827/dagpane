// The other half of `abi.rs`: forty lines that turn four `extern "C"` exports into an object
// with `open` and `send` on it.
//
// No build step, no package manager, nothing fetched at run time beyond the `.wasm` itself.
// That is the same promise the WebSocket client makes, and it is why this is a hand-written
// module rather than wasm-bindgen output — see `crates/wasm/Cargo.toml` for the trade.
//
// THE CONVENTION, once, because every function below depends on it: a call that returns
// anything returns one pointer to `[u32 length, little-endian][that many bytes of UTF-8]`.
// Read the length, read the body, free `4 + length`. Freeing the body length alone leaks the
// prefix; freeing the wrong length is undefined behaviour on the Rust side, so `readReply`
// is the only place that does either.

const DECODER = new TextDecoder();
const ENCODER = new TextEncoder();

/** A view of the module's memory. Re-taken on every use: a `grow` detaches the old buffer. */
function bytes(exports) {
  return new Uint8Array(exports.memory.buffer);
}

/** Copy a string into the module and return `[ptr, len]`, which the caller must free. */
function writeString(exports, text) {
  const encoded = ENCODER.encode(text);
  const ptr = exports.dp_alloc(encoded.length);
  bytes(exports).set(encoded, ptr);
  return [ptr, encoded.length];
}

/** Read a length-prefixed reply, free it, and parse it. */
function readReply(exports, ptr) {
  if (ptr === 0) throw new Error("dagpane: the module returned nothing");
  const memory = bytes(exports);
  const length = new DataView(exports.memory.buffer).getUint32(ptr, true);
  // `slice` copies: the JSON must outlive the buffer we are about to hand back.
  const body = DECODER.decode(memory.slice(ptr + 4, ptr + 4 + length));
  exports.dp_free(ptr, 4 + length);
  return JSON.parse(body);
}

/** Send one string in and get one parsed reply out, freeing both buffers. */
function call(exports, fn, text) {
  const [ptr, len] = writeString(exports, text);
  try {
    return readReply(exports, fn(ptr, len));
  } finally {
    exports.dp_free(ptr, len);
  }
}

/**
 * One app, running in this page.
 *
 * `send` is synchronous and that is not an oversight: the call *is* the pass. A WebSocket
 * client awaits a reply because the answer is somewhere else; here it is on the stack, and
 * wrapping it in a promise would add a turn of the event loop to a call that does not need
 * one. It does block the frame, which at the 0.1-0.9 ms `BENCHMARKS.md` measures is invisible
 * and at a hundred thousand rows would not be — the fix for that is a Web Worker rather than
 * a promise, and ADR-0007 records that nothing measured yet needs one.
 */
class DagpaneApp {
  constructor(exports, init) {
    this.exports = exports;
    /** The `init` frame: title, widgets, panes, first views, and what the first pass cost. */
    this.init = init;
  }

  /** Answer one `ClientMessage`. Returns the `ServerMessage`, already parsed. */
  send(message) {
    return call(this.exports, this.exports.dp_send, JSON.stringify(message));
  }
}

/**
 * Instantiate the module and open an app in it.
 *
 * `source` is anything `WebAssembly.instantiateStreaming` takes — a URL, a `Response`, or an
 * `ArrayBuffer` for a page that already has the bytes.
 *
 * Rejects with the engine's own error text when the manifest will not compile, which is the
 * same sentence `dagpane check` prints. A browser and a terminal disagreeing about why an app
 * is broken would be a second implementation of the compiler's opinions.
 */
export async function open(source, config) {
  const wasm = await instantiate(source);
  const exports = wasm.instance.exports;
  const init = call(exports, exports.dp_open, JSON.stringify(config));
  if (init.type === "failed") throw new Error("dagpane: " + init.message);
  return new DagpaneApp(exports, init);
}

async function instantiate(source) {
  // No imports at all. The module has no clock, no randomness and no I/O — `dagpane-core`'s
  // purity rule, which was written for testability, is what makes the browser build have
  // nothing to ask the host for.
  const imports = {};
  if (source instanceof ArrayBuffer || ArrayBuffer.isView(source)) {
    return WebAssembly.instantiate(source, imports);
  }
  if (typeof source === "string") {
    source = fetch(source);
  }
  if (typeof WebAssembly.instantiateStreaming === "function") {
    try {
      return await WebAssembly.instantiateStreaming(source, imports);
    } catch (e) {
      // A server that serves `.wasm` as `application/octet-stream` fails streaming and works
      // buffered. Common enough on static hosts to be worth the fallback rather than a
      // documentation note nobody reads until the page is blank.
      source = Promise.resolve(source);
    }
  }
  const response = await source;
  return WebAssembly.instantiate(await response.arrayBuffer(), imports);
}

export default { open };
