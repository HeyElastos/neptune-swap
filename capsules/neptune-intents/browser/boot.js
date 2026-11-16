// The whole app is Rust and Slint (crates/intents-ui); this only starts the wasm.
import init from './pkg/intents-ui.js';
try {
  await init({ module_or_path: './pkg/intents-ui_bg.wasm' });
} catch (e) {
  // Slint's event loop never returns: it leaves by a JS exception, by design.
  if (!String(e).includes('Using exceptions for control flow')) throw e;
}
