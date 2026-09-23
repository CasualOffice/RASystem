// Casual Annotate — top-level entry. Pure core plus the Jitsi transport.
//
// `surface/`, `overlay/` and `latency/` are NOT re-exported here: they need a DOM or Electron, and
// importing this module from a plain Node context (a test, a server) must never drag those in.
export * from './core/index.js';
export * from './transport/jitsi.js';
