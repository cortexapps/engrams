import { create } from "@bufbuild/protobuf";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import type { ReactNode } from "react";
import { describe, expect, test, vi } from "vitest";

import { SpecListItemSchema } from "../../gen/engram/app/v1/spec_pb";

const useSpecsMock = vi.hoisted(() => vi.fn());

vi.mock("@tanstack/react-router", () => ({
  Link: ({ children }: { children: ReactNode }) => <a href="/specs/test">{children}</a>,
  useNavigate: () => () => {},
  useSearch: () => ({}),
}));
vi.mock("../../hooks/useSpecs", () => ({ useSpecs: useSpecsMock }));

import { selectSpecFilter, SpecPagination, SpecPeople, SpecRow, SpecsList } from "./SpecsList";

describe("SpecPeople", () => {
  test("an idle spec does not report a live agent", () => {
    render(<SpecPeople participants={[]} />);

    expect(screen.getByLabelText("No live collaborators")).toBeTruthy();
    expect(screen.queryByLabelText("Spec agent")).toBeNull();
  });
});

test("pagination reaches rows after the first page", () => {
  let selectedPage = 1;
  render(
    <SpecPagination
      page={1}
      pageSize={50}
      totalCount={51}
      onPageChange={(page) => {
        selectedPage = page;
      }}
    />,
  );
  fireEvent.click(screen.getByRole("button", { name: "Next" }));
  expect(selectedPage).toBe(2);
});

test("polling returns an empty last page to the new last page", async () => {
  const firstPageSpec = create(SpecListItemSchema, {
    id: "spec-page-1",
    title: "First page spec",
    templateName: "Design",
    lifecycle: "draft",
    updatedAt: "2026-08-10T12:00:00.000Z",
  });
  const secondPageSpec = create(SpecListItemSchema, {
    id: "spec-page-2",
    title: "Second page spec",
    templateName: "Design",
    lifecycle: "draft",
    updatedAt: "2026-08-10T12:00:00.000Z",
  });
  let totalCount = 51;
  useSpecsMock.mockImplementation((_lifecycle: string, page: number) => ({
    data: {
      specs: page === 1 ? [firstPageSpec] : totalCount === 51 ? [secondPageSpec] : [],
      totalCount,
    },
    error: null,
    isPending: false,
  }));

  const view = render(<SpecsList />);
  fireEvent.click(screen.getByRole("button", { name: "Next" }));
  expect(await screen.findByText("Second page spec")).toBeTruthy();

  totalCount = 50;
  view.rerender(<SpecsList />);

  await waitFor(() => expect(useSpecsMock).toHaveBeenLastCalledWith("all", 1, 50));
  expect(screen.getByText("First page spec")).toBeTruthy();
  expect(screen.queryByText("No tech specs yet.")).toBeNull();
});

test("keeps backward navigation while it corrects an out-of-range page", () => {
  render(<SpecPagination page={2} pageSize={50} totalCount={50} onPageChange={() => {}} />);

  expect(screen.getByRole("button", { name: "Previous" }).getAttribute("disabled")).toBeNull();
  expect(screen.getByText("Page 2 of 1")).toBeTruthy();
});

test("All, Drafts, and Published reset pagination and select their filter", () => {
  for (const [status, expected] of [
    ["all", {}],
    ["draft", { status: "draft" }],
    ["published", { status: "published" }],
  ] as const) {
    let selectedPage = 3;
    let search: object = {};
    selectSpecFilter(
      status,
      (page) => {
        selectedPage = page;
      },
      (value) => {
        search = value;
      },
    );
    expect(selectedPage).toBe(1);
    expect(search).toEqual(expected);
  }
});

test("renders every field in a complete list row", () => {
  const updatedAt = "2026-08-10T11:59:00.000Z";
  const spec = create(SpecListItemSchema, {
    id: "spec-1",
    title: "Durable collaboration",
    templateName: "Design",
    repo: "cortexapps/engrams",
    lifecycle: "draft",
    participants: [{ id: "person-1", name: "Taylor Member", email: "taylor@test" }],
    openQuestionCount: 2,
    ticketSyncState: "failed",
    updatedAt,
  });
  render(
    <table>
      <tbody>
        <SpecRow spec={spec} now={new Date("2026-08-10T12:00:00.000Z").getTime()} />
      </tbody>
    </table>,
  );

  expect(screen.getByText("Durable collaboration")).toBeTruthy();
  expect(screen.getByText("Design · cortexapps/engrams")).toBeTruthy();
  expect(screen.getByText("Draft")).toBeTruthy();
  expect(screen.getByLabelText("Taylor Member")).toBeTruthy();
  expect(screen.getByText("2")).toBeTruthy();
  expect(screen.getByText("Sync failed")).toBeTruthy();
  expect(screen.getByText("1m")).toBeTruthy();
});
