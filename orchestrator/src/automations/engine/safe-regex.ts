/** Backtracking guard for author-supplied `matches` patterns (ADR 0119 D6).
 *
 * The condition evaluator runs regexes synchronously on the event loop, over
 * subjects that come from untrusted webhook payloads. Length caps bound the
 * input, not the work: `(a+)+$` against 4 KiB of `a` followed by `!` is
 * exponential. No RE2 is available under Bun, so the engine refuses the
 * pattern shapes that backtrack super-linearly instead:
 *
 * - **star height > 1**: a quantifier applied to a group that itself
 *   contains a quantifier (`(a+)+`, `(.*)*`, `(\w+\s?)*`) — the whole
 *   exponential class;
 * - **a repeated group with alternation** (`(a|aa)+`): overlapping branches
 *   under repetition are the other exponential shape, and branch overlap is
 *   not decidable cheaply, so every `(x|y)+` is refused — `(?:x|y)?` and an
 *   unrepeated alternation stay allowed;
 * - **backreferences** (`\1`, `\k<n>`): matching with them is NP-hard;
 * - **large counted repetition** (`{n,m}` with a bound above
 *   `REGEX_MAX_REPEAT`), whose expansion multiplies any remaining
 *   polynomial cost.
 *
 * Polynomial shapes (`a*a*b`) survive; with the 4096-char subject cap their
 * worst case is a few million steps. The check is applied when a condition
 * is saved AND when it is evaluated, so a pattern stored before the guard
 * existed fails closed (no match) instead of running.
 */

export const REGEX_MAX_REPEAT = 100;
export const REGEX_MAX_STAR_HEIGHT = 1;

export class UnsafeRegexError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "UnsafeRegexError";
  }
}

/** Throws `UnsafeRegexError` for a pattern the evaluator must not run. The
 * caller has already checked that `new RegExp(pattern, "u")` compiles. */
export function assertSafeRegex(pattern: string): void {
  // One frame per open group: the max star height seen inside it so far.
  // `heightOfLast` is the star height of the atom a quantifier would apply
  // to (0 for a plain atom, n+1 for a group whose content has height n).
  const stack: number[] = [];
  /** Per open group: whether a `|` was seen inside it AT ANY DEPTH. Like
   * star height, the signal propagates up on `)`: `((a|aa))+` is the same
   * machine as `(a|aa)+`. */
  const altStack: boolean[] = [];
  let current = 0;
  let currentHasAlt = false;
  let heightOfLast = 0;
  /** Whether the atom a quantifier would apply to is a group with `|`. */
  let lastIsAltGroup = false;
  let inClass = false;
  let i = 0;
  const n = pattern.length;

  while (i < n) {
    const ch = pattern[i]!;
    if (inClass) {
      if (ch === "\\") i += 2;
      else {
        if (ch === "]") inClass = false;
        i += 1;
      }
      continue;
    }
    switch (ch) {
      case "\\": {
        const next = pattern[i + 1] ?? "";
        if (/[1-9]/.test(next)) throw new UnsafeRegexError("backreferences are not allowed");
        if (next === "k" && pattern[i + 2] === "<") {
          throw new UnsafeRegexError("backreferences are not allowed");
        }
        heightOfLast = 0;
        lastIsAltGroup = false;
        i += 2;
        break;
      }
      case "[":
        inClass = true;
        heightOfLast = 0;
        lastIsAltGroup = false;
        i += 1;
        break;
      case "|":
        currentHasAlt = true;
        heightOfLast = 0;
        lastIsAltGroup = false;
        i += 1;
        break;
      case "(":
        stack.push(current);
        altStack.push(currentHasAlt);
        current = 0;
        currentHasAlt = false;
        i += 1;
        // Skip the group prefix `(?:`, `(?=`, `(?!`, `(?<=`, `(?<!`, `(?<name>`.
        if (pattern[i] === "?") {
          const close = pattern.indexOf(">", i);
          if (pattern[i + 1] === "<" && pattern[i + 2] !== "=" && pattern[i + 2] !== "!" && close !== -1) {
            i = close + 1;
          } else if (pattern[i + 1] === "<") {
            i += 3;
          } else {
            i += 2;
          }
        }
        break;
      case ")": {
        const inner = current;
        current = Math.max(stack.pop() ?? 0, inner);
        heightOfLast = inner;
        lastIsAltGroup = currentHasAlt;
        currentHasAlt = (altStack.pop() ?? false) || currentHasAlt;
        i += 1;
        break;
      }
      case "*":
      case "+":
      case "?": {
        const height = heightOfLast + (ch === "?" ? 0 : 1);
        // `?` is optionality, not repetition: it does not raise star height
        // on its own, but `(a+)?` still carries the inner quantifier.
        if (height > REGEX_MAX_STAR_HEIGHT) {
          throw new UnsafeRegexError("nested quantifiers (e.g. `(a+)+`) are not allowed");
        }
        if (ch !== "?" && lastIsAltGroup) {
          throw new UnsafeRegexError("a repeated group with alternation (e.g. `(a|aa)+`) is not allowed");
        }
        current = Math.max(current, height);
        heightOfLast = height;
        lastIsAltGroup = false;
        i += 1;
        if (pattern[i] === "?") i += 1; // lazy
        break;
      }
      case "{": {
        const close = pattern.indexOf("}", i);
        const body = close === -1 ? "" : pattern.slice(i + 1, close);
        const m = /^(\d+)(?:,(\d*))?$/.exec(body);
        if (!m) {
          // A literal `{` (the `u` flag would have rejected a malformed
          // quantifier already, so this is a plain character).
          heightOfLast = 0;
          lastIsAltGroup = false;
          i += 1;
          break;
        }
        const lo = Number(m[1]);
        const hi = m[2] === undefined ? lo : m[2] === "" ? Number.POSITIVE_INFINITY : Number(m[2]);
        if (hi > REGEX_MAX_REPEAT && hi !== Number.POSITIVE_INFINITY) {
          throw new UnsafeRegexError(`counted repetition above {${REGEX_MAX_REPEAT}} is not allowed`);
        }
        if (lo > REGEX_MAX_REPEAT) {
          throw new UnsafeRegexError(`counted repetition above {${REGEX_MAX_REPEAT}} is not allowed`);
        }
        // `{n}` with n>1 repeats the atom; `{n,}` is unbounded like `+`.
        const repeats = hi !== lo || lo > 1;
        const height = heightOfLast + (repeats ? 1 : 0);
        if (height > REGEX_MAX_STAR_HEIGHT) {
          throw new UnsafeRegexError("nested quantifiers (e.g. `(a+){2,}`) are not allowed");
        }
        if (repeats && lastIsAltGroup) {
          throw new UnsafeRegexError("a repeated group with alternation (e.g. `(a|aa){2,}`) is not allowed");
        }
        current = Math.max(current, height);
        heightOfLast = height;
        lastIsAltGroup = false;
        i = close + 1;
        if (pattern[i] === "?") i += 1; // lazy
        break;
      }
      default:
        heightOfLast = 0;
        lastIsAltGroup = false;
        i += 1;
    }
  }
}

export function isSafeRegex(pattern: string): boolean {
  try {
    assertSafeRegex(pattern);
    return true;
  } catch {
    return false;
  }
}
