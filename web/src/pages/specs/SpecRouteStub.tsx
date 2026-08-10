import { FilePenLine } from "lucide-react";

export function SpecRouteStub({ surface }: { surface: "spec" | "templates" }) {
  return (
    <div className="flex flex-1 flex-col items-center justify-center gap-3 p-8 text-center text-muted-foreground">
      <FilePenLine className="size-9" strokeWidth={1.25} aria-hidden />
      <p className="text-sm">
        {surface === "templates"
          ? "Template management is not available yet."
          : "The spec read view is not available yet."}
      </p>
    </div>
  );
}
