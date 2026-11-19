import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { create } from "@bufbuild/protobuf";
import { Code, ConnectError } from "@connectrpc/connect";

import { TagSchema, TagSummarySchema, type TagSummary } from "@/gen/engram/app/v1/tag_pb";
import { renderWithProviders } from "../../test-utils";
import { Tags } from "./Tags";

const createTag = vi.hoisted(() => vi.fn());
const updateTag = vi.hoisted(() => vi.fn());
const deleteTag = vi.hoisted(() => vi.fn());
const toastError = vi.hoisted(() => vi.fn());
const state = vi.hoisted(() => ({ tags: [] as unknown[] }));

vi.mock("@/hooks/useTags", () => ({
  useTags: () => ({ data: { tags: state.tags }, isPending: false, error: null }),
  useCreateTag: () => ({ mutateAsync: createTag, isPending: false }),
  useUpdateTag: () => ({ mutateAsync: updateTag, isPending: false }),
  useDeleteTag: () => ({ mutateAsync: deleteTag, isPending: false }),
}));
vi.mock("sonner", async (orig) => {
  const real = await orig<typeof import("sonner")>();
  return { ...real, toast: { ...real.toast, error: toastError, success: vi.fn() } };
});

function summary(id: string, name: string, automationCount: number, description = ""): TagSummary {
  return create(TagSummarySchema, {
    tag: create(TagSchema, { id, name, color: "#1f6feb", description }),
    automationCount,
  });
}

function rowOf(name: string): HTMLElement {
  return screen.getByRole("row", { name: new RegExp(name) });
}

beforeEach(() => {
  for (const spy of [createTag, updateTag, deleteTag, toastError]) spy.mockReset();
  createTag.mockResolvedValue({});
  updateTag.mockResolvedValue({});
  deleteTag.mockResolvedValue({});
  state.tags = [
    summary("t-billing", "Billing", 1),
    summary("t-platform", "Platform", 12, "Owned by platform"),
  ];
});

describe("Tags", () => {
  it("lists each tag with its description and a count that links to the filtered list", async () => {
    renderWithProviders(<Tags />);
    const platform = await screen.findByRole("row", { name: /Platform/ });
    expect(within(platform).getByText("Owned by platform")).toBeTruthy();
    const link = within(platform).getByRole("link", { name: "12 automations" });
    const href = decodeURIComponent(link.getAttribute("href") ?? "");
    expect(href.startsWith("/automations?")).toBe(true);
    expect(href).toContain("Platform");
    expect(within(rowOf("Billing")).getByRole("link", { name: "1 automation" })).toBeTruthy();
  });

  it("filters the list by name with the search box, without regard to case", async () => {
    renderWithProviders(<Tags />);
    await userEvent.type(await screen.findByRole("textbox", { name: "Search tags" }), "plat");
    expect(screen.queryByRole("row", { name: /Billing/ })).toBeNull();
    expect(screen.getByRole("row", { name: /Platform/ })).toBeTruthy();
  });

  it("creates a tag with a random color unless the admin picks one", async () => {
    renderWithProviders(<Tags />);
    await userEvent.click(await screen.findByRole("button", { name: "New tag" }));
    await userEvent.type(screen.getByRole("textbox", { name: "Tag name" }), "Ops");
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(createTag).toHaveBeenCalledTimes(1));
    const sent = createTag.mock.calls[0]![0] as {
      name: string;
      color: string;
      description: string;
    };
    expect(sent.name).toBe("Ops");
    expect(sent.description).toBe("");
    expect(sent.color).toMatch(/^#[0-9a-f]{6}$/);
  });

  it("edits the name, description and color in place", async () => {
    renderWithProviders(<Tags />);
    await userEvent.click(await screen.findByRole("button", { name: "Edit Platform" }));
    const name = screen.getByRole("textbox", { name: "Tag name" });
    await userEvent.clear(name);
    await userEvent.type(name, "Platform team");
    const description = screen.getByRole("textbox", { name: "Tag description" });
    await userEvent.clear(description);
    await userEvent.type(description, "Runs the platform");
    fireEvent.change(screen.getByRole("textbox", { name: "Hex color" }), {
      target: { value: "#8250df" },
    });
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() =>
      expect(updateTag).toHaveBeenCalledWith({
        id: "t-platform",
        name: "Platform team",
        description: "Runs the platform",
        color: "#8250df",
      }),
    );
  });

  it("asks before deleting and says how many automations lose the tag", async () => {
    renderWithProviders(<Tags />);
    await userEvent.click(await screen.findByRole("button", { name: "Delete Platform" }));
    const dialog = await screen.findByRole("alertdialog");
    expect(within(dialog).getByText('Delete "Platform"?')).toBeTruthy();
    expect(within(dialog).getByText("It is removed from 12 automations.")).toBeTruthy();
    expect(deleteTag).not.toHaveBeenCalled();
    await userEvent.click(within(dialog).getByRole("button", { name: "Delete" }));
    await waitFor(() => expect(deleteTag).toHaveBeenCalledWith({ id: "t-platform" }));
  });

  it("shows the server's message when a save fails, and keeps the form open", async () => {
    createTag.mockRejectedValue(
      new ConnectError("a tag with this name already exists", Code.AlreadyExists),
    );
    renderWithProviders(<Tags />);
    await userEvent.click(await screen.findByRole("button", { name: "New tag" }));
    await userEvent.type(screen.getByRole("textbox", { name: "Tag name" }), "billing");
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() =>
      expect(toastError).toHaveBeenCalledWith("a tag with this name already exists"),
    );
    expect(screen.getByRole("textbox", { name: "Tag name" })).toBeTruthy();
  });

  it("shows an empty state when there are no tags", async () => {
    state.tags = [];
    renderWithProviders(<Tags />);
    expect(
      await screen.findByText(
        "No tags yet. Create one here, or from the tag picker on an automation.",
      ),
    ).toBeTruthy();
  });
});
