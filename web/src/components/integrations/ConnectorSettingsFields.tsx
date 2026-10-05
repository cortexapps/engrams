/**
 * ConnectorSettingsFields — the editor for a connector's `settings` facet:
 * administrator-set, non-secret parameters such as the API host a regional or
 * self-hosted provider is reached on. One radio per preset option, plus a
 * free-text row when the connector allows a custom value. Shared by the
 * Connect sheet and the integration detail page so both write the same shape.
 */

import { useState } from "react";

import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import { Text } from "@/components/ui/text";
import type { ParsedSetting } from "@/lib/connectorModel";

const CUSTOM = "__custom__";

/** The value a setting holds in the editor: the draft, else the stored value,
 * else the connector's default, else empty. */
export function settingValue(
  setting: ParsedSetting,
  drafts: Record<string, string>,
  stored?: Record<string, string>,
): string {
  return drafts[setting.name] ?? stored?.[setting.name] ?? setting.default ?? "";
}

export function ConnectorSettingsFields({
  settings,
  valueOf,
  onChange,
}: {
  settings: ParsedSetting[];
  valueOf: (setting: ParsedSetting) => string;
  onChange: (name: string, value: string) => void;
}) {
  // Whether the administrator chose the free-text row. Defaults on when the
  // current value is not one of the presets (a stored self-hosted host).
  const [customMode, setCustomMode] = useState<Record<string, boolean>>({});

  return (
    <>
      {settings.map((setting) => {
        const current = valueOf(setting);
        const isPreset = setting.options.some((option) => option.value === current);
        const isCustom =
          setting.custom !== undefined &&
          (customMode[setting.name] ?? (current !== "" && !isPreset));
        const radioValue = isCustom ? CUSTOM : isPreset ? current : "";
        return (
          <fieldset key={setting.name} className="flex flex-col gap-2">
            <legend className="mb-1.5">
              <Text variant="label">{setting.label}</Text>
            </legend>
            {setting.options.length > 0 && (
              <RadioGroup
                value={radioValue}
                aria-label={setting.label}
                className="gap-1.5"
                onValueChange={(next) => {
                  if (next === CUSTOM) {
                    setCustomMode((m) => ({ ...m, [setting.name]: true }));
                    if (isPreset) onChange(setting.name, "");
                  } else {
                    setCustomMode((m) => ({ ...m, [setting.name]: false }));
                    onChange(setting.name, next);
                  }
                }}
              >
                {setting.options.map((option) => (
                  <Label
                    key={option.value}
                    htmlFor={`setting-${setting.name}-${option.value}`}
                    className="cursor-pointer rounded-md border bg-card px-3 py-2 font-normal has-[[data-state=checked]]:border-primary"
                  >
                    <RadioGroupItem
                      value={option.value}
                      id={`setting-${setting.name}-${option.value}`}
                    />
                    <span className="text-sm">{option.label}</span>
                  </Label>
                ))}
                {setting.custom && (
                  <Label
                    htmlFor={`setting-${setting.name}-custom`}
                    className="cursor-pointer rounded-md border bg-card px-3 py-2 font-normal has-[[data-state=checked]]:border-primary"
                  >
                    <RadioGroupItem value={CUSTOM} id={`setting-${setting.name}-custom`} />
                    <span className="text-sm">{setting.custom.label}</span>
                  </Label>
                )}
              </RadioGroup>
            )}
            {setting.custom && (isCustom || setting.options.length === 0) && (
              <Input
                className="font-mono"
                autoComplete="off"
                spellCheck={false}
                aria-label={`${setting.label}: ${setting.custom.label}`}
                placeholder="host.example.com"
                value={isPreset ? "" : current}
                onChange={(e) => onChange(setting.name, e.target.value.trim())}
              />
            )}
            {isCustom && setting.custom?.hint ? (
              <p className="text-xs leading-relaxed text-muted-foreground">{setting.custom.hint}</p>
            ) : setting.hint ? (
              <p className="text-xs leading-relaxed text-muted-foreground">{setting.hint}</p>
            ) : null}
          </fieldset>
        );
      })}
    </>
  );
}
