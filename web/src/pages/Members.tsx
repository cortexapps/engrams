import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { MoreHorizontal } from "lucide-react";
import { authClient } from "@/lib/auth-client";
import { useAuth } from "../auth/AuthProvider";
import { PageHeading } from "../components/page-heading";
import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import type { AdminUser, Role } from "../lib/types";

// BaUser: shape from better-auth admin.listUsers
interface BaUser {
  id: string;
  email: string;
  name: string;
  role: string;
  banned: boolean;
}

function baUserToAdminUser(u: BaUser): AdminUser {
  return {
    id: u.id,
    email: u.email,
    display_name: u.name || null,
    role: u.role === "admin" ? "admin" : "member",
    role_source: "manual",
    active: !u.banned,
  };
}

async function fetchAdminUsers(): Promise<AdminUser[]> {
  const result = await authClient.admin.listUsers({ query: { limit: 100 } });
  const users = (result.data as { users?: BaUser[] } | null)?.users ?? [];
  return users.map(baUserToAdminUser);
}

export function Members() {
  const { principal } = useAuth();
  const qc = useQueryClient();
  const {
    data: users = [],
    isLoading,
    error,
  } = useQuery({ queryKey: ["admin", "users"], queryFn: fetchAdminUsers });

  const mutation = useMutation({
    mutationFn: async ({
      id,
      patch,
    }: {
      id: string;
      patch: { role?: Role; active?: boolean };
    }): Promise<AdminUser> => {
      if (patch.role !== undefined) {
        // better-auth uses "user" | "admin"; our Role type uses "member" | "admin"
        const baRole = patch.role === "member" ? "user" : "admin";
        await authClient.admin.setRole({ userId: id, role: baRole });
      }
      if (patch.active === false) {
        await authClient.admin.banUser({ userId: id });
      } else if (patch.active === true) {
        await authClient.admin.unbanUser({ userId: id });
      }
      // Re-fetch the updated user from the list.
      const result = await authClient.admin.listUsers({ query: { limit: 100 } });
      const users = (result.data as { users?: BaUser[] } | null)?.users ?? [];
      const updated = users.find((u) => u.id === id);
      if (!updated) throw new Error(`User ${id} not found after update`);
      return baUserToAdminUser(updated);
    },
    onSuccess: (u) =>
      qc.setQueryData<AdminUser[]>(["admin", "users"], (prev) =>
        prev ? prev.map((x) => (x.id === u.id ? u : x)) : [u],
      ),
  });

  if (isLoading) return <p className="p-6 text-sm text-muted-foreground">Loading…</p>;
  if (error)
    return (
      <p className="p-6 text-sm text-destructive">
        Could not load members — {(error as Error).message}
      </p>
    );

  const admins = users.filter((u) => u.role === "admin" && u.active).length;
  const disabled = users.filter((u) => !u.active).length;

  return (
    <div className="space-y-6">
      <PageHeading
        title="Members"
        description={`${users.length} people · ${admins} admins · ${disabled} disabled`}
      />
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead>Person</TableHead>
            <TableHead>Role</TableHead>
            <TableHead>Source</TableHead>
            <TableHead>Status</TableHead>
            <TableHead className="w-10" />
          </TableRow>
        </TableHeader>
        <TableBody>
          {users.map((u) => {
            const isYou = u.email === principal.email;
            return (
              <TableRow key={u.id} className={u.active ? "" : "opacity-60"}>
                <TableCell>
                  <span className="flex items-center gap-2">
                    <Avatar className="size-7">
                      <AvatarFallback className="text-xs">
                        {(u.display_name || u.email).charAt(0).toUpperCase()}
                      </AvatarFallback>
                    </Avatar>
                    <span>
                      <span className="block text-sm">
                        {u.display_name || u.email}
                        {isYou && " (you)"}
                      </span>
                      <span className="block font-mono text-xs text-muted-foreground">
                        {u.email}
                      </span>
                    </span>
                  </span>
                </TableCell>
                <TableCell>
                  <Badge variant={u.role === "admin" ? "default" : "secondary"}>{u.role}</Badge>
                </TableCell>
                <TableCell className="text-sm text-muted-foreground">{u.role_source}</TableCell>
                <TableCell className="text-sm text-muted-foreground">
                  {u.active ? "active" : "disabled"}
                </TableCell>
                <TableCell>
                  {!isYou && (
                    <DropdownMenu>
                      <DropdownMenuTrigger asChild>
                        <Button variant="ghost" size="icon" aria-label="Member actions">
                          <MoreHorizontal className="size-4" />
                        </Button>
                      </DropdownMenuTrigger>
                      <DropdownMenuContent align="end">
                        {u.role === "member" ? (
                          <DropdownMenuItem
                            onClick={() => mutation.mutate({ id: u.id, patch: { role: "admin" } })}
                          >
                            Make admin
                          </DropdownMenuItem>
                        ) : (
                          <DropdownMenuItem
                            onClick={() => mutation.mutate({ id: u.id, patch: { role: "member" } })}
                          >
                            Revoke admin
                          </DropdownMenuItem>
                        )}
                        {u.active ? (
                          <DropdownMenuItem
                            variant="destructive"
                            onClick={() => mutation.mutate({ id: u.id, patch: { active: false } })}
                          >
                            Deactivate
                          </DropdownMenuItem>
                        ) : (
                          <DropdownMenuItem
                            onClick={() => mutation.mutate({ id: u.id, patch: { active: true } })}
                          >
                            Reactivate
                          </DropdownMenuItem>
                        )}
                      </DropdownMenuContent>
                    </DropdownMenu>
                  )}
                </TableCell>
              </TableRow>
            );
          })}
        </TableBody>
      </Table>
      <p className="max-w-prose text-sm text-muted-foreground">
        Roles are provisioned from your identity provider on first sign-in and stay in sync over
        SCIM; promote or revoke here and the change is marked "set by an admin". A deactivated
        member keeps their sessions but can't sign in.
      </p>
    </div>
  );
}
