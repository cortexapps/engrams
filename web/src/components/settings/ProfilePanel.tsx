import type { ReactNode } from "react";
import { Link } from "@tanstack/react-router";
import { Check, X } from "lucide-react";
import { useAuth } from "../../auth/AuthProvider";
import { PageHeading } from "../page-heading";
import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import { Badge } from "@/components/ui/badge";
import { Text } from "@/components/ui/text";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";

export function ProfilePanel() {
  const { principal } = useAuth();
  const label = principal.display_name || principal.email;
  const isAdmin = principal.role === "admin";
  const can = isAdmin
    ? [
        "Launch & manage your own sessions",
        "Oversee every session across the fleet",
        "Inspect host capacity & drain hosts",
        "Read storage durability & snapshots",
        "Curate images & registry credentials",
        "Manage members & their roles",
      ]
    : ["Launch & manage your own sessions", "Save your own Claude Code token"];
  const cannot = isAdmin
    ? []
    : ["The Operator section (fleet, storage, images, registries): admin only"];

  return (
    <div className="max-w-2xl space-y-6">
      <PageHeading title="Profile" />

      <Card>
        <CardHeader className="flex flex-row items-center gap-3 space-y-0">
          <Avatar className="size-12 rounded-md">
            <AvatarFallback className="rounded-md text-lg">
              {label.charAt(0).toUpperCase()}
            </AvatarFallback>
          </Avatar>
          <div>
            <CardTitle>{label}</CardTitle>
            <p className="font-mono text-sm text-muted-foreground">{principal.email}</p>
          </div>
        </CardHeader>
        <CardContent className="space-y-3">
          <Row label="Role">
            <Badge variant={isAdmin ? "default" : "secondary"}>{principal.role}</Badge>
            {principal.role_source && (
              <span className="text-sm text-muted-foreground">
                · set by {principal.role_source}
              </span>
            )}
          </Row>
          <Row label="Tokens">
            {principal.has_claude_token ? (
              <span className="text-sm">
                Claude Code saved · managed under{" "}
                <Link to="/settings/tokens" className="underline">
                  Tokens
                </Link>
              </span>
            ) : (
              <Link to="/settings/tokens" className="text-sm underline">
                None saved — add one under Tokens →
              </Link>
            )}
          </Row>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-sm">What your role can do</CardTitle>
        </CardHeader>
        <CardContent>
          <ul className="space-y-1.5 text-sm">
            {can.map((c) => (
              <li key={c} className="flex items-center gap-2">
                <Check className="size-4 text-foreground" /> {c}
              </li>
            ))}
            {cannot.map((c) => (
              <li key={c} className="flex items-center gap-2 text-muted-foreground">
                <X className="size-4" /> {c}
              </li>
            ))}
          </ul>
        </CardContent>
      </Card>
    </div>
  );
}

function Row({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="grid grid-cols-[8rem_1fr] items-baseline gap-2">
      <Text as="span" variant="label" tone="muted">
        {label}
      </Text>
      <span className="flex flex-wrap items-baseline gap-2">{children}</span>
    </div>
  );
}
