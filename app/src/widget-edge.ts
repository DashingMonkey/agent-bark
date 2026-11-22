import { fitPosition, type Rect } from "./widget-layout";

export type EdgeSide = "top" | "left" | "right";
export interface Point { x: number; y: number }
export const EDGE_THRESHOLD = 6;
export const EDGE_PEEK = 6;

/**
 * 平台判定：tao 的 cursorPosition 在 macOS Retina 上 y 值错一整屏（物理像素高减
 * 点坐标后仍按逻辑乘 scale，见 widget.ts dockTarget 的注释），macOS 的吸附判定
 * 不能依赖全局光标坐标，需要退化到卡片几何。
 * 测试沙箱没有 navigator，视为非 macOS。
 */
export function isMacOS(): boolean {
  return typeof navigator === "undefined" ? false : navigator.userAgent.includes("Macintosh");
}

/**
 * 吸附判定带（逻辑像素）。macOS 略放宽到 24：
 * 顶边是菜单栏（workArea.y ≈ 25 逻辑 px），且把窗口拖到菜单栏处停留会触发
 * 系统的「进入全屏/调度中心」手势——手势接管拖动，松手窗口被弹回原位。
 * 判定的意图信号是「鼠标贴边」（点能精确贴到 x=0，见 dockTarget 的说明），
 * 24px 对指针足够精确；其它平台保持原有的 6px 精确吸附。
 */
export function edgeThreshold(): number {
  return isMacOS() ? 24 : EDGE_THRESHOLD;
}

/** Animate content in a full-sized viewport; shrink the native window only after settling. */
export function edgeTransform(width: number, height: number, side: EdgeSide, progress: number) {
  const amount = Math.max(0, Math.min(1, progress));
  const x = side === "top" ? 0 : (width - EDGE_PEEK) * amount * (side === "left" ? -1 : 1);
  const y = side === "top" ? -(height - EDGE_PEEK) * amount : 0;
  return `translate3d(${x}px, ${y}px, 0)`;
}

export function edgeMotion(from: number, to: number, reduced: boolean) {
  // Keep brief spatial feedback even with reduced motion; no bounce, zoom or instant jump.
  const duration = reduced ? 160 : to === 0 ? 260 : 220;
  return {
    duration: Math.max(80, duration * Math.abs(to - from)),
    easing: to === 0 ? "cubic-bezier(0.22, 1, 0.36, 1)" : "cubic-bezier(0.4, 0, 0.6, 1)",
    fill: "both" as const,
  };
}

export function containsPoint(rect: Rect, point: Point) {
  return point.x >= rect.x && point.x < rect.x + rect.width && point.y >= rect.y && point.y < rect.y + rect.height;
}

/** All arguments use physical desktop pixels, including monitors with different DPI. */
export function edgeAtPoint(point: Point, work: Rect, otherScreens: Rect[], threshold: number, card?: Rect): EdgeSide | null {
  // 普通拖法会让卡片先碰到边缘，而鼠标还在卡片内部；两种情况都应触发。
  const side = point.y <= work.y + threshold || (card && card.y <= work.y + threshold) ? "top"
    : point.x <= work.x + threshold || (card && card.x <= work.x + threshold) ? "left"
    : point.x >= work.x + work.width - threshold || (card && card.x + card.width >= work.x + work.width - threshold) ? "right" : null;
  if (!side) return null;
  const probe = side === "top" ? { x: point.x, y: work.y - 2 }
    : { x: side === "left" ? work.x - 2 : work.x + work.width + 2, y: point.y };
  return otherScreens.some((screen) => containsPoint(screen, probe)) ? null : side;
}

/** Remember the expanded card, never the narrow native window used while collapsed. */
export function dockRect(card: Rect, screen: Rect, side: EdgeSide): Rect {
  const position = fitPosition(card, screen);
  if (side === "top") position.y = screen.y;
  else position.x = side === "left" ? screen.x : Math.max(screen.x, screen.x + screen.width - card.width);
  return { ...card, ...position };
}

/** Clip the sliding card inside the work area, so no invisible window covers the desktop/taskbar. */
export function edgeFrame(card: Rect, side: EdgeSide, progress: number) {
  const amount = Math.max(0, Math.min(1, progress));
  const width = side === "top" ? card.width : Math.round(card.width - (card.width - EDGE_PEEK) * amount);
  const height = side === "top" ? Math.round(card.height - (card.height - EDGE_PEEK) * amount) : card.height;
  return {
    x: side === "right" ? card.x + card.width - width : card.x,
    y: card.y,
    width,
    height,
    offsetX: side === "left" ? width - card.width : 0,
    offsetY: side === "top" ? height - card.height : 0,
  };
}
