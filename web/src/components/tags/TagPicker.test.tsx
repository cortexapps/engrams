import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { create } from "@bufbuild/protobuf";
import { Code, ConnectError } from "@connectrpc/connect";

import { TagSchema, TagSummarySchema } from "@/gen/engram/app/v1/tag_pb";
import { TagPicker } from "./TagPicker";

const addTag = vi.hoisted(() => vi.fn());
const removeTag = vi.hoisted(() => vi.fn());
const createTag = vi.hoisted(() => vi.fn());
const toastError = vi.hoisted(() => vi.fn());
const state = vi.hoisted(() => ({ all: [] as unknown[] }));

vi.mock("@/hooks/useTags", () => ({
  useTags: () => ({ data: { tags: state.all }, isPending: false, error: null }),
  useCreateTag: () => ({ mutateAsync: createTag, isPending: false }),
}));
vi.mock("@/hooks/useAutomationEditor", () => ({
  useAddAutomationTag: () => ({ mutateAsync: addTag, isPending: false }),
  useRemoveAutomationTag: () => ({ mutateAsync: removeTag, isPending: false }),
}));
vi.mock("sonner", async (orig) => {
  const real = await orig<typeof import("sonner")>();
  return { ...real, toast: { ...real.toast, error: toastError } };
});

const ops = create(TagSchema, { id: "t-ops", name: "Ops", color: "#1f6feb", description: "" });
const billing = create(TagSchema, {
  id: "t-billing",
  name: "Billing",
  color: "#8250df",
  description: "Money",
});

beforeEach(() => {
  for (const spy of [addTag, removeTag, createTag, toastError]) spy.mockReset();
  addTag.mockResolvedValue({});
  removeTag.mockResolvedValue({});
  // The server lists tags by name.
  state.all = [billing, ops].map((tag) => create(TagSummarySchema, { tag, automationCount: 1 }));
});

async function openPicker(label: string) {
  await userEvent.click(screen.getByRole("button", { name: label }));
  return screen.findByRole("listbox");
}

describe("TagPicker", () => {
  it("reads Add tag without tags and Edit tags with some", () => {
    const { rerender } = render(<TagPicker automationId="a1" tags={[]} />);
    expect(screen.getByRole("button", { name: "Add tag" })).toBeTruthy();
    rerender(<TagPicker automationId="a1" tags={[ops]} />);
    expect(screen.getByRole("button", { name: "Edit tags" })).toBeTruthy();
  });

  it("lists the automation's own tags first, then the others, each with its description", async () => {
    render(<TagPicker automationId="a1" tags={[ops]} />);
    const list = await openPicker("Edit tags");
    const options = within(list).getAllByRole("option");
    expect(options.map((option) => option.querySelector(".tag-chip")?.textContent)).toEqual([
      "Ops",
      "Billing",
    ]);
    expect(within(options[1]!).getByText("Money")).toBeTruthy();
  });

  it("checking a tag adds it and unchecking removes it", async () => {
    render(<TagPicker automationId="a1" tags={[ops]} />);
    const list = await openPicker("Edit tags");
    const [ownOption, otherOption] = within(list).getAllByRole("option");
    await userEvent.click(otherOption!);
    await waitFor(() =>
      expect(addTag).toHaveBeenCalledWith({ automationId: "a1", tagId: "t-billing" }),
    );
    await userEvent.click(ownOption!);
    await waitFor(() =>
      expect(removeTag).toHaveBeenCalledWith({ automationId: "a1", tagId: "t-ops" }),
    );
  });

  it("creates a typed tag with a random color, then adds it", async () => {
    createTag.mockResolvedValue({
      tag: { id: "t-new", name: "New team", color: "#009bab", description: "" },
    });
    render(<TagPicker automationId="a1" tags={[]} />);
    await openPicker("Add tag");
    await userEvent.type(screen.getByRole("combobox", { name: "Search tags" }), "New team");
    await userEvent.click(await screen.findByRole("option", { name: 'Create tag "New team"' }));
    await waitFor(() => expect(createTag).toHaveBeenCalledTimes(1));
    const sent = createTag.mock.calls[0]![0] as {
      name: string;
      color: string;
      description: string;
    };
    expect(sent).toMatchObject({ name: "New team", description: "" });
    expect(sent.color).toMatch(/^#[0-9a-f]{6}$/);
    await waitFor(() =>
      expect(addTag).toHaveBeenCalledWith({ automationId: "a1", tagId: "t-new" }),
    );
  });

  it("does not offer Create for a name that exists in other case", async () => {
    render(<TagPicker automationId="a1" tags={[]} />);
    const list = await openPicker("Add tag");
    await userEvent.type(screen.getByRole("combobox", { name: "Search tags" }), "ops");
    expect(within(list).queryByRole("option", { name: /Create tag/ })).toBeNull();
    expect(within(list).getByRole("option", { name: /Ops/ })).toBeTruthy();
  });

  it("shows the server's message when a tag is refused", async () => {
    addTag.mockRejectedValue(
      new ConnectError("an automation can have at most 10 tags", Code.FailedPrecondition),
    );
    render(<TagPicker automationId="a1" tags={[ops]} />);
    const list = await openPicker("Edit tags");
    await userEvent.click(within(list).getAllByRole("option")[1]!);
    await waitFor(() =>
      expect(toastError).toHaveBeenCalledWith("an automation can have at most 10 tags"),
    );
  });
});
