import { afterEach, expect, test, vi } from "vitest";
import { act, renderHook } from "@testing-library/react";
import { appendDictation, useDictation } from "./useDictation";

// --- appendDictation (pure) ---

test("appendDictation starts from empty with no leading space", () => {
  expect(appendDictation("", "  hello world  ")).toBe("hello world");
});

test("appendDictation joins with exactly one space", () => {
  expect(appendDictation("hello", "world")).toBe("hello world");
});

test("appendDictation does not double the space when text already ends in one", () => {
  expect(appendDictation("hello ", "world")).toBe("hello world");
});

test("appendDictation ignores an all-whitespace chunk", () => {
  expect(appendDictation("hello", "   ")).toBe("hello");
});

// --- useDictation (Web Speech API) ---

// A stand-in for the browser's SpeechRecognition that lets a test drive the
// result/end callbacks by hand.
class FakeRecognition {
  lang = "";
  continuous = false;
  interimResults = false;
  onresult: ((event: unknown) => void) | null = null;
  onerror: (() => void) | null = null;
  onend: (() => void) | null = null;
  started = false;
  static last: FakeRecognition | null = null;
  constructor() {
    FakeRecognition.last = this;
  }
  start() {
    this.started = true;
  }
  stop() {
    this.onend?.();
  }
  abort() {
    this.onend?.();
  }
  emitFinal(transcript: string) {
    this.onresult?.({
      resultIndex: 0,
      results: { length: 1, 0: { isFinal: true, length: 1, 0: { transcript } } },
    });
  }
}

afterEach(() => {
  delete (window as { SpeechRecognition?: unknown }).SpeechRecognition;
  FakeRecognition.last = null;
});

test("reports unsupported when the browser has no Web Speech API", () => {
  const { result } = renderHook(() => useDictation(() => {}));
  expect(result.current.supported).toBe(false);
});

test("reports supported and toggles listening on/off", () => {
  (window as { SpeechRecognition?: unknown }).SpeechRecognition = FakeRecognition;
  const { result } = renderHook(() => useDictation(() => {}));
  expect(result.current.supported).toBe(true);

  act(() => result.current.toggle());
  expect(result.current.listening).toBe(true);
  expect(FakeRecognition.last?.started).toBe(true);
  expect(FakeRecognition.last?.continuous).toBe(true);

  act(() => result.current.toggle());
  expect(result.current.listening).toBe(false);
});

test("delivers finalized transcript segments to the callback", () => {
  (window as { SpeechRecognition?: unknown }).SpeechRecognition = FakeRecognition;
  const onFinal = vi.fn();
  const { result } = renderHook(() => useDictation(onFinal));

  act(() => result.current.start());
  act(() => FakeRecognition.last?.emitFinal("hello there"));
  expect(onFinal).toHaveBeenCalledWith("hello there");
});
