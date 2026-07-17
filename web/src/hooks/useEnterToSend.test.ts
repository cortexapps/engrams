import { afterEach, expect, test } from "vitest";
import { act, renderHook } from "@testing-library/react";
import { useEnterToSend } from "./useEnterToSend";

afterEach(() => localStorage.clear());

test("defaults to on (Enter-to-send, matching Claude desktop)", () => {
  const { result } = renderHook(() => useEnterToSend());
  expect(result.current[0]).toBe(true);
});

test("persists an explicit opt-out to localStorage", () => {
  const { result } = renderHook(() => useEnterToSend());
  act(() => result.current[1](false));
  expect(result.current[0]).toBe(false);
  expect(localStorage.getItem("engrams-enter-to-send")).toBe("false");
});

test("keeps separate mounts in sync when one flips the toggle", () => {
  const composer = renderHook(() => useEnterToSend());
  const settings = renderHook(() => useEnterToSend());
  act(() => settings.result.current[1](false));
  expect(composer.result.current[0]).toBe(false);
});

test("reads an already-saved opt-out on mount", () => {
  localStorage.setItem("engrams-enter-to-send", "false");
  const { result } = renderHook(() => useEnterToSend());
  expect(result.current[0]).toBe(false);
});
