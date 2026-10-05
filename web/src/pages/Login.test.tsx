import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import type { AuthConfig } from "../lib/sign-in";

const startOAuthSignIn = vi.fn();
const getSession = vi.fn();
const signInEmail = vi.fn();
const signUpEmail = vi.fn();

vi.mock("@/lib/auth-client", () => ({
  startOAuthSignIn: (...args: unknown[]) => startOAuthSignIn(...args),
  authClient: {
    getSession: () => getSession(),
    signIn: { email: (...args: unknown[]) => signInEmail(...args) },
    signUp: { email: (...args: unknown[]) => signUpEmail(...args) },
  },
}));

import { Login } from "./Login";

const ORIGIN = "https://engrams.example.com";
const assign = vi.fn();
const realLocation = window.location;

/** Put the page at `/login<search>` with a spy-able `location.assign`. */
function atLogin(search = ""): void {
  Object.defineProperty(window, "location", {
    configurable: true,
    value: { origin: ORIGIN, search, pathname: "/login", href: `${ORIGIN}/login${search}`, assign },
  });
}

/** Answer GET /api/v1/auth-config with `posture`, or fail it. */
function serveAuthConfig(posture: AuthConfig | "fail"): void {
  vi.stubGlobal(
    "fetch",
    vi.fn(async () =>
      posture === "fail"
        ? new Response("nope", { status: 503 })
        : new Response(JSON.stringify(posture), { status: 200 }),
    ),
  );
}

const OAUTH: AuthConfig = {
  mode: "oauth",
  oauth: { providerId: "sso", displayName: "Google" },
  passwordAuth: false,
  signup: false,
  previewBaseDomain: "preview.example.com",
};

beforeEach(() => {
  atLogin();
  getSession.mockResolvedValue({ data: null });
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
  vi.unstubAllGlobals();
  Object.defineProperty(window, "location", { configurable: true, value: realLocation });
});

describe("Login — oauth mode", () => {
  it("shows the provider button and no password form", async () => {
    serveAuthConfig(OAUTH);
    render(<Login />);
    expect(await screen.findByRole("button", { name: "Continue with Google" })).toBeTruthy();
    expect(screen.queryByLabelText("Password")).toBeNull();
    expect(screen.queryByText("Sign up")).toBeNull();
  });

  it("starts the round trip and navigates to the provider", async () => {
    serveAuthConfig(OAUTH);
    startOAuthSignIn.mockResolvedValue({ url: "https://accounts.google.com/o/oauth2/v2/auth?x=1" });
    render(<Login />);
    await userEvent.click(await screen.findByRole("button", { name: "Continue with Google" }));

    expect(startOAuthSignIn).toHaveBeenCalledWith({
      providerId: "sso",
      callbackURL: `${ORIGIN}/`,
      errorCallbackURL: `${ORIGIN}/login`,
    });
    await waitFor(() =>
      expect(assign).toHaveBeenCalledWith("https://accounts.google.com/o/oauth2/v2/auth?x=1"),
    );
  });

  it("with ?next=, the round trip returns to this page so it can validate the hop", async () => {
    const search = "?next=https%3A%2F%2Fweb-abc.preview.example.com%2F";
    atLogin(search);
    serveAuthConfig(OAUTH);
    startOAuthSignIn.mockResolvedValue({ url: "https://idp.example/authorize" });
    render(<Login />);
    await userEvent.click(await screen.findByRole("button", { name: "Continue with Google" }));
    expect(startOAuthSignIn).toHaveBeenCalledWith(
      expect.objectContaining({ callbackURL: `${ORIGIN}/login${search}` }),
    );
  });

  it("back from the provider with a session and ?next=, it finishes the hop to the app", async () => {
    atLogin("?next=https%3A%2F%2Fweb-abc.preview.example.com%2F");
    serveAuthConfig(OAUTH);
    getSession.mockResolvedValue({ data: { session: { id: "s" } } });
    render(<Login />);
    await waitFor(() =>
      expect(assign).toHaveBeenCalledWith("https://web-abc.preview.example.com/"),
    );
  });

  it("a next outside the preview domain goes to the dashboard, not to that host", async () => {
    atLogin("?next=https%3A%2F%2Fevil.example%2F");
    serveAuthConfig(OAUTH);
    getSession.mockResolvedValue({ data: { session: { id: "s" } } });
    render(<Login />);
    await waitFor(() => expect(assign).toHaveBeenCalledWith("/"));
  });

  it("explains a refused sign-in and still offers the button", async () => {
    atLogin("?error=account_not_allowed");
    serveAuthConfig(OAUTH);
    render(<Login />);
    expect((await screen.findByRole("alert")).textContent).toMatch(/does not have access/);
    expect(screen.getByRole("button", { name: "Continue with Google" })).toBeTruthy();
  });

  it("shows the server's reason when the round trip cannot start", async () => {
    serveAuthConfig(OAUTH);
    startOAuthSignIn.mockResolvedValue({ error: "Invalid callbackURL" });
    render(<Login />);
    await userEvent.click(await screen.findByRole("button", { name: "Continue with Google" }));
    expect((await screen.findByRole("alert")).textContent).toBe("Invalid callbackURL");
    expect(assign).not.toHaveBeenCalled();
  });
});

describe("Login — password mode", () => {
  it("sign-up open: the form, with the registration toggle", async () => {
    serveAuthConfig({ mode: "password", passwordAuth: true, signup: true });
    render(<Login />);
    expect(await screen.findByLabelText("Password")).toBeTruthy();
    expect(screen.getByText("Sign up")).toBeTruthy();
    expect(screen.queryByRole("button", { name: /Continue with/ })).toBeNull();
  });

  it("sign-up closed: the form, without the registration toggle", async () => {
    serveAuthConfig({ mode: "password", passwordAuth: true, signup: false });
    render(<Login />);
    expect(await screen.findByLabelText("Password")).toBeTruthy();
    expect(screen.queryByText("Sign up")).toBeNull();
  });
});

describe("Login — iap mode", () => {
  it("renders no door", async () => {
    serveAuthConfig({ mode: "iap", passwordAuth: false, signup: false });
    render(<Login />);
    expect(await screen.findByText("Single sign-on")).toBeTruthy();
    expect(screen.queryByLabelText("Password")).toBeNull();
    expect(screen.queryByRole("button", { name: /Continue with/ })).toBeNull();
  });
});

describe("Login — posture unavailable", () => {
  // A guess here would render a door this deployment may not have (a password
  // form on an OAuth deployment), so the failure is shown as a failure.
  it("shows the failure and retries on request", async () => {
    serveAuthConfig("fail");
    render(<Login />);
    expect(await screen.findByText("Sign-in is unavailable")).toBeTruthy();
    expect(screen.queryByLabelText("Password")).toBeNull();

    serveAuthConfig(OAUTH);
    await userEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(await screen.findByRole("button", { name: "Continue with Google" })).toBeTruthy();
  });
});
