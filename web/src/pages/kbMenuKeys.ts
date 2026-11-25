/* Which keys the group row and its submenu recognise (KbMenuList). Consistent with the menu convention:
   Enter / Space / → open and move to the first item; Esc / ← return to the row; ↑↓ move within the submenu */
const OPENS = new Set(["Enter", " ", "ArrowRight"]);
const IN_SUBMENU: Record<string, "close" | 1 | -1> = {
  Escape: "close",
  ArrowLeft: "close",
  ArrowDown: 1,
  ArrowUp: -1,
};

export function groupRowKey(key: string): "open" | undefined {
  return OPENS.has(key) ? "open" : undefined;
}

export function submenuKey(key: string): "close" | 1 | -1 | undefined {
  return IN_SUBMENU[key];
}
