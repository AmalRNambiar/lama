// libm shims for wasm32-unknown-unknown, which provides no math intrinsics: the
// Rust wasm imports `fmod`/`round` from the "env" module (float `%` and
// `.round()` in a dependency). Mapped to this file by the import map in
// index.html so the module instantiates.
export function fmod(a, b) {
  return a % b; // JS `%` is C fmod for floats (truncated remainder).
}
export function round(x) {
  // C round(): half away from zero (JS Math.round is half toward +inf).
  return Math.sign(x) * Math.round(Math.abs(x));
}

// `critical-section` (pulled by a dep) emits acquire/release imports with no impl
// set. wasm is single-threaded, so a critical section is a no-op: nothing can
// preempt the held section. acquire returns a restore-state token release ignores.
export function _critical_section_1_0_acquire() {
  return 0;
}
export function _critical_section_1_0_release(_token) {}
