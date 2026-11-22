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

/**
 * 独立详情窗的落位，返回详情窗自身屏幕矩形 {side,x,y,width,height}（不含卡片）。
 * 采用 Floating UI/Popper 的标准 flip+shift 模型（macOS NSPopover 同理）：
 * - 主轴固定在「卡片边 + 间隙」处——天然不盖卡片；
 * - flip 按**自然高度**判定该侧放不放得下：底/上侧空间不足完整详情就翻面，
 *   而不是把详情压扁塞进小缝（右下角离 Dock 一条缝时翻左侧，而非截断）；
 * - 副轴 shift 钳进屏幕保证整窗可见；锚点（卡片）永不移动；
 * - 四侧都放不下自然高度时才 size 收缩兜底（Floating UI 里 size 排在 flip 之后）。
 * 候选序 右→底→左→上，取第一个放得下自然高度的侧。
 */
export function placeTip(card: Rect, screen: Rect, rowTop: number, measuredHeight: number) {
  const maxH = Math.min(measuredHeight, 112);
  const rightSpace = screen.x + screen.width - card.x - card.width - TIP_GAP;
  const leftSpace = card.x - screen.x - TIP_GAP;
  const belowSpace = screen.y + screen.height - card.y - card.height - TIP_GAP;
  const aboveSpace = card.y - screen.y - TIP_GAP;
  const shift = (v: number, size: number, min: number, max: number) => Math.max(min, Math.min(v, max - size));
  const rowY = (h: number) => card.y + Math.max(0, Math.min(rowTop, card.height - h));
  const xShift = shift(card.x, card.width, screen.x, screen.x + screen.width);
  const candidates: Array<{ side: string; x: number; y: number; width: number; height: number }> = [];
  if (rightSpace >= 180) {
    const h = Math.min(maxH, screen.height);
    candidates.push({ side: "right", x: card.x + card.width + TIP_GAP, y: shift(rowY(h), h, screen.y, screen.y + screen.height), width: Math.min(220, rightSpace), height: h });
  }
  if (belowSpace >= maxH) {
    candidates.push({ side: "bottom", x: xShift, y: card.y + card.height + TIP_GAP, width: card.width, height: maxH });
  }
  if (leftSpace >= 180) {
    const h = Math.min(maxH, screen.height);
    candidates.push({ side: "left", x: card.x - TIP_GAP - Math.min(220, leftSpace), y: shift(rowY(h), h, screen.y, screen.y + screen.height), width: Math.min(220, leftSpace), height: h });
  }
  if (aboveSpace >= maxH) {
    candidates.push({ side: "top", x: xShift, y: card.y - TIP_GAP - maxH, width: card.width, height: maxH });
  }
  if (candidates.length) return candidates[0];
  // 退化兜底：四侧都放不下自然高度，取有空间的一侧收缩适配（极小屏才会走到）
  if (belowSpace >= 1) return { side: "bottom", x: xShift, y: card.y + card.height + TIP_GAP, width: card.width, height: Math.min(maxH, belowSpace) };
  if (aboveSpace >= 1) return { side: "top", x: xShift, y: card.y - TIP_GAP - Math.min(maxH, aboveSpace), width: card.width, height: Math.min(maxH, aboveSpace) };
  if (rightSpace >= 1) return { side: "right", x: card.x + card.width + TIP_GAP, y: shift(rowY(maxH), maxH, screen.y, screen.y + screen.height), width: Math.min(220, rightSpace), height: maxH };
  if (leftSpace >= 1) return { side: "left", x: card.x - TIP_GAP - Math.min(220, leftSpace), y: shift(rowY(maxH), maxH, screen.y, screen.y + screen.height), width: Math.min(220, leftSpace), height: maxH };
  return { side: "bottom", x: card.x, y: Math.max(screen.y, screen.y + screen.height - maxH), width: card.width, height: maxH };
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
  /**
   * key → 稳定编号（不缓存拼好的字符串：编号是身份，前缀属于当前语言——
   * 换语言后下一次 get() 自动按新前缀取词，widget.ts 无需重建映射）。
   */
  private numbers = new Map<string, number>();
  private next = 1;
  private format: (n: number) => string;

  /**
   * `format` 由编号生成显示标签。缺省为中文「子代理 NN」——本模块被
   * tests/widget-layout.test.mjs 以 VM 沙箱直接加载，**绝不 import i18n**，
   * 界面语言由 widget.ts 在构造时传 `t("floating.subagent", …)` 进来。
   */
  constructor(format: (n: number) => string = (n) => `子代理 ${String(n).padStart(2, "0")}`) {
    this.format = format;
  }

  get(key: string) {
    let n = this.numbers.get(key);
    if (!n) {
      n = this.next++;
      this.numbers.set(key, n);
    }
    return this.format(n);
  }

  retain(keys: Set<string>) {
    for (const key of this.numbers.keys()) if (!keys.has(key)) this.numbers.delete(key);
  }
}
