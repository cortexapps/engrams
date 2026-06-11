// Minimal ambient typing for the one Node global the e2e files touch.
// Deliberately NOT @types/node: that would auto-include across the whole
// program and leak Node globals (setTimeout typing, Buffer, …) into src/,
// which is a browser-only surface. Widen this if e2e ever needs more.
declare const process: { env: Record<string, string | undefined> };
