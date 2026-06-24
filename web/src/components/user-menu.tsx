import { ChevronsUpDown, LogOut, Settings } from "lucide-react";
import { useNavigate } from "@tanstack/react-router";
import { signOut } from "../auth/AuthProvider";
import { useAuth } from "../auth/AuthProvider";
import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  useSidebar,
} from "@/components/ui/sidebar";

export function UserMenu() {
  const { principal } = useAuth();
  const navigate = useNavigate();
  const { isMobile, state } = useSidebar();
  const label = principal.display_name || principal.email;
  const initial = label.charAt(0).toUpperCase();

  return (
    <SidebarMenu>
      <SidebarMenuItem>
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <SidebarMenuButton size="lg" aria-label={label} data-testid="user-menu-trigger">
              <Avatar className="size-8 rounded-md">
                <AvatarFallback className="rounded-md">{initial}</AvatarFallback>
              </Avatar>
              <div className="grid flex-1 text-left text-sm leading-tight">
                <span className="truncate font-medium">{label}</span>
                <span className="truncate text-xs text-muted-foreground">{principal.email}</span>
              </div>
              <ChevronsUpDown className="ml-auto size-4" />
            </SidebarMenuButton>
          </DropdownMenuTrigger>
          <DropdownMenuContent
            // Expanded rail: anchor the menu directly above the avatar, matching
            // its width (the standard bottom-of-sidebar account menu). Only the
            // icon-collapsed spine flies out to the right; mobile drops down.
            side={isMobile ? "bottom" : state === "collapsed" ? "right" : "top"}
            align="end"
            sideOffset={4}
            className="w-(--radix-dropdown-menu-trigger-width) min-w-56"
          >
            <DropdownMenuLabel className="font-normal">
              <div className="grid text-sm">
                <span className="font-medium">{label}</span>
                <span className="text-xs text-muted-foreground">{principal.email}</span>
              </div>
            </DropdownMenuLabel>
            <DropdownMenuSeparator />
            <DropdownMenuItem onClick={() => navigate({ to: "/settings" })}>
              <Settings /> Settings
            </DropdownMenuItem>
            {principal.can_sign_out && (
              <DropdownMenuItem onClick={() => void signOut()}>
                <LogOut /> Sign out
              </DropdownMenuItem>
            )}
          </DropdownMenuContent>
        </DropdownMenu>
      </SidebarMenuItem>
    </SidebarMenu>
  );
}
