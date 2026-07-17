import { afterEach, expect, test } from "vitest";
import { act, renderHook } from "@testing-library/react";
import { useEnterToSend } from "./useEnterToSend";

afterEach(() => localStorage.clear());

test("defaults to off (⌘↵-to-send behaviour preserved)", () => {
  const { result } = renderHook(() => useEnterToSend());
  expect(result.current[0]).toBe(false);
});

test("persists the preference to localStorage", () => {
  const { result } = renderHook(() => useEnterToSend());
  act(() => result.current[1](true));
  expect(result.current[0]).toBe(true);
  expect(localStorage.getItem("engrams-enter-to-send")).toBe("true");
});

test("keeps separate mounts in sync when one flips the toggle", () => {
  const composer = renderHook(() => useEnterToSend());
  const settings = renderHook(() => useEnterToSend());
  act(() => settings.result.current[1](true));
  expect(composer.result.current[0]).toBe(true);
});

test("reads an already-saved preference on mount", () => {
  localStorage.setItem("engrams-enter-to-send", "true");
  const { result } = renderHook(() => useEnterToSend());
  expect(result.current[0]).toBe(true);
});
