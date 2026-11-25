import { Button, Chip, MenuSelect } from "../ui";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
/* User menu: the avatar capsule on the right of the top bar + the dropdown panel (profile / system administration / log out).
   Shared by Shell (KB workspace) and AccountShell (account level). */
import { useState, type ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import {
  Languages,
  Layers,
  Monitor,
  Moon,
  LogOut,
  ShieldCheck,
  Sun,
  UserRound,
  SunMoon,
} from "lucide-react";
import { api, type User } from "../api";
import { LANGS, LANG_NAMES, S, lang, setLang } from "../i18n";
import { clearSessionState } from "../authSession";

/** Initials avatar: a neutral grey background (no colour bias from chrome); for Latin, the first two letters of the first word; for CJK, the first two characters. */
export function Avatar({ name, size = 24 }: { name: string; size?: number }) {
  const trimmed = name.trim();
  const words = trimmed.split(/\s+/).filter(Boolean);
  const initials =
    words.length >= 2
      ? (words[0][0] + words[1][0]).toUpperCase()
      : [...trimmed].slice(0, 2).join("").toUpperCase();
  return (
    <span
      className="inline-grid place-items-center rounded-full bg-surface-3 border border-line text-ink select-none shrink-0"
      style={{ width: size, height: size, fontSize: Math.round(size * 0.38) }}
    >
      {initials}
    </span>
  );
}

import { getTheme, setTheme, type Theme } from "../theme";

// The order of the three tiers is the order in the menu: the native colour first, "follow system" last
const THEMES: Theme[] = ["dark", "light", "system"];
/** The symbol for each of the three theme tiers: moon / sun / a screen (the tier that follows the system, which means "this machine") */
const THEME_ICONS: Record<Theme, ReactNode> = {
  dark: <Moon size={13} />,
  light: <Sun size={13} />,
  system: <Monitor size={13} />,
};

/** One row in the menu: the same as the rows of the other two panels in the top bar (px-4 py-2, body size, icon gap 3) */
const ITEM = "gap-3 rounded-none px-4 py-2 text-body";

export function UserMenu({ user }: { user: User }) {
  const [theme, setThemeState] = useState<Theme>(() => getTheme());
  const [open, setOpen] = useState(false);
  const navigate = useNavigate();
  const queryClient = useQueryClient();

  const go = (to: string) => {
    setOpen(false);
    navigate({ to });
  };

  const logout = async () => {
    await api.logout();
    clearSessionState(queryClient);
    navigate({ to: "/login" });
  };

  // Rows run to the panel edge (same vocabulary as Dropdown): the container has no inner padding, and the height is set by the row itself

  return (
    /* The user menu in the top bar. **It is a menu, not a panel**, so DropdownMenu rather than Popover is used:
       it needs "a column of actions" plus a second-level menu. Language and theme are folded into the second level: languages will have several kinds later,
       and there are three theme tiers; if all were laid flat at the first level, this menu would keep growing, and they all belong to "preferences",
       not to actions sitting alongside profile and administration. */
    <DropdownMenu open={open} onOpenChange={setOpen}>
      <DropdownMenuTrigger asChild>
        <Button variant="ghost" className="u-avatar-btn">
          {/* 24: the capsule is 32 high (every control in the top bar has the same height), with 4 left over on each side of the avatar */}
          <Avatar name={user.display_name} size={24} />
          <span className="text-body text-ink-2">{user.display_name}</span>
        </Button>
      </DropdownMenuTrigger>
      {/* **The rhythm of the rows matches the other two panels in the top bar** (px-4 py-2 text-body, the whole row bleeds out):
          the default shadcn menu item is px-2 py-1.5, which looks even more cramped next to the KB switcher and the alert panel.
          The container drops p-1 so the rows reach the edge; hover belongs to the row, consistent across the site. */}
      <DropdownMenuContent align="end" className="w-64 p-0">
        {/* The identity header: not a clickable item, it only says "who you are" */}
        <div className="flex items-center gap-3 border-b border-line px-4 py-3">
          <Avatar name={user.display_name} size={32} />
          <div className="min-w-0 flex-1">
            <div className="flex items-center gap-2">
              <span className="truncate text-body font-medium text-ink">
                {user.display_name}
              </span>
              {user.is_admin && (
                <Chip tone="neutral" className="text-fine">
                  {S.account.adminChip}
                </Chip>
              )}
            </div>
            <div className="truncate text-fine text-ink-2">{user.email}</div>
          </div>
        </div>

        <div className="py-1">
          <DropdownMenuItem className={ITEM} onSelect={() => go("/account")}>
            <UserRound size={13} />
            {S.account.profile}
          </DropdownMenuItem>
          {/* Visible to everyone: all visible KBs + my identity in each KB */}
          <DropdownMenuItem className={ITEM} onSelect={() => go("/account/kbs")}>
            <Layers size={13} />
            {S.account.kbsNav}
          </DropdownMenuItem>
          {user.is_admin && (
            <DropdownMenuItem className={ITEM} onSelect={() => go("/admin")}>
              <ShieldCheck size={13} />
              {S.account.administration}
            </DropdownMenuItem>
          )}
        </div>

        {/* Language and theme: **the pop-out appears at the value end**, not with the options laid out below, nor a submenu flying off to the right.
            This row reads as "Language: English": what is to be changed is the cell after the colon,
            so the popover should appear on that cell, like filling in a blank.
            Uses shadcn's Select: it brings its own portal and stacking, so when open it sits over the menu,
            and after a choice both layers close together. The whole row is the trigger, so clicking anywhere opens it. */}
        <div className="border-t border-line py-1">
          <MenuSelect
            icon={<Languages size={13} />}
            label={S.account.language}
            value={lang}
            onChange={(v) => setLang(v as (typeof LANGS)[number])}
            options={LANGS.map((l) => ({ value: l, label: LANG_NAMES[l] }))}
          />
          <MenuSelect
            icon={<SunMoon size={13} />}
            label={S.account.theme}
            value={theme}
            onChange={(v) => {
              setTheme(v as Theme);
              setThemeState(v as Theme);
            }}
            options={THEMES.map((t) => ({
              value: t,
              label: S.account.themeNames[t],
              icon: THEME_ICONS[t],
            }))}
          />
        </div>

        <div className="border-t border-line py-1">
          <DropdownMenuItem className={ITEM} variant="destructive" onSelect={logout}>
            <LogOut size={13} />
            {S.nav.signOut}
          </DropdownMenuItem>
        </div>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
