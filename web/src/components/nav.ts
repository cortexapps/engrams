import type { LinkProps } from '@tanstack/react-router';
import type { LucideIcon } from 'lucide-react';

// The shape every navigation destination shares — the main rail, the section
// sidebars (Operator, Settings), and the Sessions scope switcher. Each surface
// extends it with what only it needs (active-matching, admin gating, exactness)
// rather than redeclaring the common fields and drifting apart.
export interface NavItem {
  to: LinkProps['to'];
  label: string;
  icon: LucideIcon;
}
