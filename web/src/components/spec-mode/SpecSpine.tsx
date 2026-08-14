import { Link } from "@tanstack/react-router";
import { ClipboardCheck, Layers3, SquareTerminal } from "lucide-react";

import { useAuth } from "@/auth/AuthProvider";
import { EngramMark } from "@/components/EngramMark";

const NAV_ITEMS = [
  { to: "/sessions", label: "Tasks", icon: SquareTerminal },
  { to: "/specs", label: "Tech Specs", icon: ClipboardCheck },
  { to: "/artifacts", label: "Artifacts", icon: Layers3 },
] as const;

export function SpecSpine() {
  const { principal } = useAuth();
  const identity = principal.display_name || principal.email;
  const initial = identity.trim().charAt(0).toLocaleUpperCase() || "?";

  return (
    <nav className="spec-mode-spine" aria-label="Primary">
      <Link to="/specs" className="spec-mode-mark" aria-label="Back to Tech Specs">
        <EngramMark size={22} mode="static" />
      </Link>
      {NAV_ITEMS.map((item) => (
        <Link
          key={item.to}
          to={item.to}
          className="spec-mode-nav-square"
          data-active={item.to === "/specs" || undefined}
          aria-current={item.to === "/specs" ? "page" : undefined}
          aria-label={item.label}
        >
          <item.icon aria-hidden="true" />
        </Link>
      ))}
      <span className="spec-mode-user-initial" aria-label={identity} title={identity}>
        {initial}
      </span>
    </nav>
  );
}
