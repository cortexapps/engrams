import { LogOut, Moon, Sun } from "lucide-react";
import { signOut } from "../auth/AuthProvider";
import { useAuth } from "../auth/AuthProvider";
import { useTheme } from "./theme-provider";
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

// The avatar row at the foot of the spine: who is signed in, on one line, with
// the role and the workspace domain under it. The menu holds what is about
// this PERSON's session — the theme and signing out. Settings is a spine
// destination, not a menu item.
export function UserMenu() {
  const { principal } = useAuth();
  const { theme, toggle } = useTheme();
  const { isMobile, state } = useSidebar();
  const label = principal.display_name || principal.email;
  const initial = label.charAt(0).toUpperCase();
  const domain = principal.email.split("@")[1];
  const subline = domain ? `${principal.role} · ${domain}` : principal.role;

  return (
    <SidebarMenu>
      <SidebarMenuItem>
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <SidebarMenuButton
              aria-label={label}
              data-testid="user-menu-trigger"
              // The 26px tile sits in a 32px button when the spine collapses to
              // icons: 3px of padding fits it exactly.
              className="h-10 gap-2.5 rounded-[10px] px-2 group-data-[collapsible=icon]:p-[3px]!"
            >
              <span className="flex size-[26px] shrink-0 items-center justify-center rounded-[6px] border border-sidebar-border font-mono text-xs font-semibold">
                {initial}
              </span>
              <span className="grid min-w-0 flex-1 text-left leading-tight">
                <span className="truncate text-xs font-medium">{label}</span>
                <span className="truncate text-[10.5px] text-sidebar-foreground/60">{subline}</span>
              </span>
            </SidebarMenuButton>
          </DropdownMenuTrigger>
          <DropdownMenuContent
            // Expanded spine: anchor the menu directly above the avatar, matching
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
            <DropdownMenuItem onClick={toggle}>
              {theme === "dark" ? <Sun /> : <Moon />}
              {theme === "dark" ? "Light theme" : "Dark theme"}
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
