import { useCallback, useEffect, useRef, useState } from "react";

// Voice dictation for the composer, on the browser-native Web Speech API. This
// is a CLIENT-SIDE transcriber (Chromium/Safari expose `SpeechRecognition`):
// no backend route, no API key, no per-word cost — the mic audio never touches
// engrams. Where it isn't available (Firefox today) the hook reports
// `supported: false` and the composer simply omits the mic button.
//
// v1 is raw dictation: finalized speech segments are appended into the composer
// textarea verbatim. A later pass could pipe the transcript through the agent
// for punctuation/cleanup, but that needs a server route and is out of scope.

// The Web Speech API is still vendor-prefixed in Chromium and isn't in the DOM
// lib on every TS version, so declare the minimal surface we actually touch
// rather than reaching for `any`.
interface SpeechRecognitionAlternativeLike {
  readonly transcript: string;
}
interface SpeechRecognitionResultLike {
  readonly isFinal: boolean;
  readonly length: number;
  [index: number]: SpeechRecognitionAlternativeLike;
}
interface SpeechRecognitionResultListLike {
  readonly length: number;
  [index: number]: SpeechRecognitionResultLike;
}
interface SpeechRecognitionEventLike {
  readonly resultIndex: number;
  readonly results: SpeechRecognitionResultListLike;
}
interface SpeechRecognitionLike {
  lang: string;
  continuous: boolean;
  interimResults: boolean;
  start(): void;
  stop(): void;
  abort(): void;
  onresult: ((event: SpeechRecognitionEventLike) => void) | null;
  onerror: (() => void) | null;
  onend: (() => void) | null;
}
type SpeechRecognitionConstructor = new () => SpeechRecognitionLike;

// Honest widening for the real-but-untyped vendor globals (per AGENTS.md: this
// is a genuine untyped field, not an `as unknown as` launder).
type WindowWithSpeech = Window & {
  SpeechRecognition?: SpeechRecognitionConstructor;
  webkitSpeechRecognition?: SpeechRecognitionConstructor;
};

function getRecognitionCtor(): SpeechRecognitionConstructor | null {
  if (typeof window === "undefined") return null;
  const w = window as WindowWithSpeech;
  return w.SpeechRecognition ?? w.webkitSpeechRecognition ?? null;
}

/**
 * Merge a newly-finalized speech segment into the existing composer text,
 * keeping exactly one space at the seam. Pure so it's trivially testable.
 */
export function appendDictation(existing: string, chunk: string): string {
  const piece = chunk.trim();
  if (!piece) return existing;
  if (!existing) return piece;
  return /\s$/.test(existing) ? existing + piece : `${existing} ${piece}`;
}

export interface Dictation {
  /** Whether this browser exposes the Web Speech API at all. */
  supported: boolean;
  /** True while a recognition session is live. */
  listening: boolean;
  start: () => void;
  stop: () => void;
  toggle: () => void;
}

/**
 * Drive browser speech-to-text for the composer. `onFinalTranscript` fires with
 * each finalized segment (never interim guesses), so the caller appends stable
 * text and never has to un-write a revised interim result.
 */
export function useDictation(onFinalTranscript: (text: string) => void): Dictation {
  const [supported] = useState(() => getRecognitionCtor() !== null);
  const [listening, setListening] = useState(false);
  const recognitionRef = useRef<SpeechRecognitionLike | null>(null);
  // Keep the newest callback without re-instantiating recognition each render.
  const onFinalRef = useRef(onFinalTranscript);
  onFinalRef.current = onFinalTranscript;

  const stop = useCallback(() => {
    recognitionRef.current?.stop();
  }, []);

  const start = useCallback(() => {
    const Ctor = getRecognitionCtor();
    // Already listening → no-op (toggle handles the stop side).
    if (!Ctor || recognitionRef.current) return;
    const recognition = new Ctor();
    recognition.lang = navigator.language || "en-US";
    // Keep the mic open across natural pauses; stream interim results so we can
    // pick finals out of them as they settle.
    recognition.continuous = true;
    recognition.interimResults = true;
    recognition.onresult = (event) => {
      let finalChunk = "";
      // `resultIndex` marks where this event's new results begin; earlier ones
      // were already delivered on prior events.
      for (let i = event.resultIndex; i < event.results.length; i++) {
        const result = event.results[i];
        if (result.isFinal) finalChunk += result[0]?.transcript ?? "";
      }
      if (finalChunk.trim()) onFinalRef.current(finalChunk);
    };
    // Permission denied / no-speech / network drop: `onend` always follows, so
    // teardown lives there and this stays a no-op.
    recognition.onerror = () => {};
    recognition.onend = () => {
      recognitionRef.current = null;
      setListening(false);
    };
    recognitionRef.current = recognition;
    setListening(true);
    recognition.start();
  }, []);

  const toggle = useCallback(() => {
    if (recognitionRef.current) stop();
    else start();
  }, [start, stop]);

  // Hard-stop a live session if the composer unmounts mid-dictation.
  useEffect(() => () => recognitionRef.current?.abort(), []);

  return { supported, listening, start, stop, toggle };
}
