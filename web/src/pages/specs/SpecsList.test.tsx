import { create } from "@bufbuild/protobuf";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import type { ReactNode } from "react";
import { describe, expect, test, vi } from "vitest";

import { SpecListItemSchema } from "../../gen/engram/app/v1/spec_pb";
import { UserSchema } from "../../gen/engram/app/v1/user_pb";

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
    render(<SpecPeople participants={[]} activeParticipantCount={0} />);

    expect(screen.getByLabelText("No live collaborators")).toBeTruthy();
    expect(screen.queryByLabelText("Spec agent")).toBeNull();
  });

  test("uses the distinct active count for collaborators outside the SQL sample", () => {
    render(
      <SpecPeople
        participants={[
          create(UserSchema, { id: "person-1", name: "Person One", email: "one@test" }),
          create(UserSchema, { id: "person-2", name: "Person Two", email: "two@test" }),
          create(UserSchema, { id: "person-3", name: "Person Three", email: "three@test" }),
        ]}
        activeParticipantCount={8}
      />,
    );

    expect(screen.getByText("+5")).toBeTruthy();
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
    phase: "drafting",
    updatedAt: "2026-08-10T12:00:00.000Z",
  });
  const secondPageSpec = create(SpecListItemSchema, {
    id: "spec-page-2",
    title: "Second page spec",
    templateName: "Design",
    phase: "drafting",
    updatedAt: "2026-08-10T12:00:00.000Z",
  });
  let totalCount = 51;
  useSpecsMock.mockImplementation((_phase: string, page: number) => ({
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

test("all phase filters reset pagination and select their value", () => {
  for (const [status, expected] of [
    ["all", {}],
    ["ideation", { status: "ideation" }],
    ["drafting", { status: "drafting" }],
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
    phase: "drafting",
    participants: [{ id: "person-1", name: "Taylor Member", email: "taylor@test" }],
    activeParticipantCount: 1,
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
  expect(screen.getByText("Drafting")).toBeTruthy();
  expect(screen.getByLabelText("Taylor Member")).toBeTruthy();
  expect(screen.getByText("2")).toBeTruthy();
  expect(screen.getByText("Sync failed")).toBeTruthy();
  expect(screen.getByText("1m")).toBeTruthy();
});
