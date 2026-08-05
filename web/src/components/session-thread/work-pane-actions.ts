import { createContext, useContext } from "react";

// Lets transcript cards drive the WorkPane (same pattern as
// composer-actions). Provided by SessionDetail, which owns the pane state;
// null wherever the thread renders without a pane (tests, previews) — the
// affordance simply hides there.

export interface WorkPaneActions {
  /**
   * Open the Processes view. With a Bash `toolCallId` it opens straight
   * into that command's live tail; without one it shows the command list.
   */
  openProcesses: (toolCallId?: string) => void;
}

export const WorkPaneActionsContext = createContext<WorkPaneActions | null>(null);

export function useWorkPaneActions(): WorkPaneActions | null {
  return useContext(WorkPaneActionsContext);
}
