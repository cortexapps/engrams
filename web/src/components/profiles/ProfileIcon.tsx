import * as React from "react";
import * as Lucide from "lucide-react";
import { Box } from "lucide-react";
import type { LucideProps } from "lucide-react";

/** Resolve a stored lucide icon name to its component, falling back to Box for
 * unknown names. Profiles store the icon name as a string (ADR §1). */
export function ProfileIcon({ name, ...props }: { name: string } & LucideProps) {
  const Icon = (Lucide as unknown as Record<string, React.ComponentType<LucideProps>>)[name] ?? Box;
  return <Icon {...props} />;
}

/** Curated, dev-relevant starter set for the icon picker (ADR §7). */
export const PROFILE_ICON_CHOICES = [
  "Bot",
  "Terminal",
  "Bug",
  "Wrench",
  "FlaskConical",
  "GitBranch",
  "Rocket",
  "Cpu",
  "Code",
  "Database",
  "Server",
  "Shield",
  "Box",
  "Boxes",
  "Cog",
  "Hammer",
] as const;
