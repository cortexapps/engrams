import { create } from "@bufbuild/protobuf";
import { fireEvent, render, screen } from "@testing-library/react";
import type { ReactNode } from "react";
import { describe, expect, test, vi } from "vitest";

import { SpecListItemSchema } from "../../gen/engram/app/v1/spec_pb";

vi.mock("@tanstack/react-router", () => ({
  Link: ({ children }: { children: ReactNode }) => <a href="/specs/test">{children}</a>,
  useNavigate: () => () => {},
  useSearch: () => ({}),
}));

import { selectSpecFilter, SpecPagination, SpecPeople, SpecRow } from "./SpecsList";

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
