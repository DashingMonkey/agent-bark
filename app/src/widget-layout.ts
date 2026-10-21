/** Pure layout rules shared by the widget and its geometry regression tests. */
export const CARD_WIDTH = 280;
export const MAX_LIST_HEIGHT = 192;
export const TIP_GAP = 8;

export interface Rect {
  x: number;
  y: number;
  width: number;
  height: number;
}

export function fitPosition(rect: Rect, screen: Rect) {
  return {
    x: Math.max(screen.x, Math.min(rect.x, screen.x + screen.width - rect.width)),
    y: Math.max(screen.y, Math.min(rect.y, screen.y + screen.height - rect.height)),
  };
}

export function tipWidth(card: Rect, screen: Rect) {
  const rightSpace = screen.x + screen.width - card.x - card.width - TIP_GAP;
  return rightSpace >= 180 ? { side: "right" as const, width: Math.min(220, rightSpace) }
    : { side: "bottom" as const, width: card.width };
}

/** Bottom details have exactly the same left/right edges as the card. */
export function placeTip(card: Rect, screen: Rect, rowTop: number, measuredHeight: number) {
  const { side, width } = tipWidth(card, screen);
  const height = Math.min(measuredHeight, Math.max(1, screen.height - (side === "bottom" ? card.height + TIP_GAP : 0)));
  const left = side === "right" ? card.width + TIP_GAP : 0;
  const top = side === "right" ? Math.max(0, Math.min(rowTop, card.height - height)) : card.height + TIP_GAP;
  const bounds = { ...card, width: Math.max(card.width, left + width), height: Math.max(card.height, top + height) };
  return { side, left, top, width, height, windowWidth: bounds.width, windowHeight: bounds.height, ...fitPosition(bounds, screen) };
}

/** Keep the discriminating suffix visible without letting it occupy the whole name column. */
export function splitName(name: string): [string, string] {
  if (name.length <= 21) return [name, ""];
  const separator = Math.max(name.lastIndexOf("-"), name.lastIndexOf("_"));
  const tailLength = separator > 0 && name.length - separator <= 12 ? name.length - separator : 10;
  return [name.slice(0, -tailLength), name.slice(-tailLength)];
}

/** Numbers stay stable while siblings start/finish; they are not based on row order. */
export class SubagentLabels {
  private labels = new Map<string, string>();
  private next = 1;

  get(key: string) {
    let label = this.labels.get(key);
    if (!label) {
      label = `子代理 ${String(this.next++).padStart(2, "0")}`;
      this.labels.set(key, label);
    }
    return label;
  }

  retain(keys: Set<string>) {
    for (const key of this.labels.keys()) if (!keys.has(key)) this.labels.delete(key);
  }
}
