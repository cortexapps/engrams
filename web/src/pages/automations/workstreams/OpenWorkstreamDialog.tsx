import { useMemo, useState } from "react";
import { toast } from "sonner";

import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Field, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import type { AutomationInstance } from "@/gen/engram/app/v1/automation_pb";
import { useRunNow } from "@/hooks/useAutomations";
import { useInvalidateInstances } from "@/hooks/useInstances";
import {
  buildInputsPayload,
  resolveInputValues,
  validateInputs,
  type InputFieldError,
  type InputFieldSpec,
  type InputValues,
} from "@/lib/automation-inputs";

import { InputsForm } from "../inputs/InputsForm";

export interface WorkstreamAutomationOption {
  id: string;
  name: string;
  inputsSchema: InputFieldSpec[];
  defaultInputs: unknown;
}

export function OpenWorkstreamDialog({
  automations,
  instances,
  fixedAutomationId,
  open,
  onOpenChange,
}: {
  automations: readonly WorkstreamAutomationOption[];
  instances: readonly AutomationInstance[];
  fixedAutomationId?: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const runNow = useRunNow();
  const invalidateInstances = useInvalidateInstances();
  const [pickedAutomationId, setPickedAutomationId] = useState(fixedAutomationId ?? "");
  const [name, setName] = useState("");
  const [values, setValues] = useState<InputValues | null>(null);
  const [errors, setErrors] = useState<InputFieldError[]>([]);
  const automationId = fixedAutomationId ?? pickedAutomationId;
  const automation = automations.find((candidate) => candidate.id === automationId);
  const resolved = useMemo(
    () => resolveInputValues(automation?.inputsSchema ?? [], automation?.defaultInputs),
    [automation],
  );
  const current = values ?? resolved;
  const joining = instances.some(
    (instance) =>
      instance.automationId === automationId &&
      instance.status === "open" &&
      instance.key === name.trim(),
  );

  const reset = () => {
    setName("");
    setValues(null);
    setErrors([]);
    if (!fixedAutomationId) setPickedAutomationId("");
  };

  const submit = async () => {
    if (!automation) {
      toast.error("Choose an automation");
      return;
    }
    if (name.trim() === "") {
      toast.error("Name the workstream");
      return;
    }
    if (!joining) {
      const clientErrors = validateInputs(automation.inputsSchema, current);
      if (clientErrors.length > 0) {
        setErrors(clientErrors);
        toast.error("Fix the highlighted inputs");
        return;
      }
    }
    try {
      await runNow.mutateAsync({
        automationId,
        instanceKey: name.trim(),
        // The open workstream owns its original input snapshot, so a join must not replace it.
        ...(joining
          ? {}
          : { instanceInputsJson: buildInputsPayload(automation.inputsSchema, current) }),
      });
      await invalidateInstances();
      toast.success(joining ? `Opened the next run for ${name.trim()}` : `Opened ${name.trim()}`);
      onOpenChange(false);
      reset();
    } catch (error) {
      toast.error(error instanceof Error ? error.message : String(error));
    }
  };

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        onOpenChange(next);
        if (!next) reset();
      }}
    >
      <DialogContent className="max-w-lg">
        <DialogHeader>
          <DialogTitle>Open a workstream</DialogTitle>
          <DialogDescription>
            Name an ongoing case. Matching events continue its activity until you close it.
          </DialogDescription>
        </DialogHeader>
        <form
          className="flex flex-col gap-4"
          data-testid="open-workstream-form"
          onSubmit={(event) => {
            event.preventDefault();
            void submit();
          }}
        >
          {!fixedAutomationId && (
            <Field>
              <FieldLabel>Automation</FieldLabel>
              <Select
                value={pickedAutomationId}
                onValueChange={(value) => {
                  setPickedAutomationId(value);
                  setValues(null);
                  setErrors([]);
                }}
              >
                <SelectTrigger className="w-full" aria-label="Automation">
                  <SelectValue placeholder="Choose an automation" />
                </SelectTrigger>
                <SelectContent>
                  {automations.map((candidate) => (
                    <SelectItem key={candidate.id} value={candidate.id}>
                      {candidate.name}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </Field>
          )}
          <Field>
            <FieldLabel>Name</FieldLabel>
            <Input
              value={name}
              onChange={(event) => setName(event.target.value)}
              placeholder="ENG-42"
              aria-label="workstream name"
              autoFocus={!!fixedAutomationId}
            />
          </Field>
          {joining ? (
            <p className="text-sm text-muted-foreground" data-testid="open-workstream-join-hint">
              {name.trim()} is already open — this opens its next run
            </p>
          ) : automation ? (
            <InputsForm
              schema={automation.inputsSchema}
              values={current}
              errors={errors}
              onChange={(key, next) => {
                setValues({ ...current, [key]: next });
                setErrors((previous) => previous.filter((error) => error.key !== key));
              }}
              disabled={runNow.isPending}
            />
          ) : null}
          <DialogFooter>
            <Button type="submit" disabled={runNow.isPending || automations.length === 0}>
              {runNow.isPending ? "Opening…" : "Open a workstream"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
