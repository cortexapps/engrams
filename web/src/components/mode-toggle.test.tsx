import { expect, test } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { ThemeProvider } from "./theme-provider";
import { ModeToggle } from "./mode-toggle";

test("toggles the dark class on the document root", async () => {
  document.documentElement.classList.remove("dark");
  render(
    <ThemeProvider>
      <ModeToggle />
    </ThemeProvider>,
  );
  const btn = screen.getByRole("button", { name: /toggle theme/i });
  expect(document.documentElement.classList.contains("dark")).toBe(false);
  await userEvent.click(btn);
  expect(document.documentElement.classList.contains("dark")).toBe(true);
});
