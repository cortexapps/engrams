import { useState } from "react";

import { useAutomations } from "@/hooks/useAutomations";
import { useAllRuns } from "@/hooks/useAutomationRuns";
import { useNow } from "@/hooks/useNow";
import { PageHeading } from "@/components/page-heading";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";

import { ActivityList } from "./ActivityList";
import { countToday } from "./activity-format";

const ANY = "__any__";

// Every automation's activity in one ledger. The run service lists per
// automation, so this page fans out one query per automation and merges the
// results newest-first; the picker narrows to one.
export function ActivityPage() {
  const automations = useAutomations();
  const list = automations.data?.automations ?? [];
  const nameOf = (id: string) => list.find((s) => s.automation?.id === id)?.automation?.name;
  const [picked, setPicked] = useState<string>(ANY);
  const [includeFiltered, setIncludeFiltered] = useState(false);
  const now = useNow(10_000);

  const ids = list
    .map((s) => s.automation?.id)
    .filter((id): id is string => !!id)
    .filter((id) => picked === ANY || id === picked);
  const all = useAllRuns(ids, { includeFiltered, limit: 25 });
  const today = countToday(all.runs, now);

  return (
    <div className="space-y-6">
      <PageHeading title="Activity" count={all.isPending ? undefined : `today · ${today}`} />
      <ActivityList
        runs={all.runs}
        windows={all.windows}
        isPending={automations.isPending || (ids.length > 0 && all.isPending)}
        error={automations.error ?? all.error}
        now={now}
        includeFiltered={includeFiltered}
        onIncludeFilteredChange={setIncludeFiltered}
        nameOf={nameOf}
        controls={
          <Select value={picked} onValueChange={setPicked}>
            <SelectTrigger size="sm" className="h-7 w-[200px]" aria-label="Automation">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value={ANY}>Any automation</SelectItem>
              {list.map((s) =>
                s.automation ? (
                  <SelectItem key={s.automation.id} value={s.automation.id}>
                    {s.automation.name}
                  </SelectItem>
                ) : null,
              )}
            </SelectContent>
          </Select>
        }
        emptyText="No activity yet. Turn an automation on and its runs land here."
      />
    </div>
  );
}
