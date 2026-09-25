// 陈旧 ：hover 门控——窗口隐藏/最小化期间 WebView 收不到 mouseleave，重新显示后
// Chromium 不重算悬停状态，上次悬停的元素（比如刚点过的关闭按钮）会顶着悬停样式
// 出现，直到鼠标真实动一下才刷新。Chromium 已知问题（minimize/restore、hide/show
// 都中），Electron 侧靠 show 后 sendInputEvent 注入真实 mouseMove 解决，而 wry
// 没有事件注入口，只能在 CSS 层挡：窗口重新可见/重新聚焦时给 body 挂 no-hover
// 把悬停样式压住（选择器统一用 body:not(.no-hover) 门控），第一个真实 mousemove
// 再摘掉——那一刻悬停态必然已被真实事件重算。
export function guardStaleHover(): void {
  const arm = () => document.body.classList.add("no-hover");
  document.addEventListener("visibilitychange", arm);
  window.addEventListener("focus", arm);
  window.addEventListener(
    "mousemove",
    () => document.body.classList.remove("no-hover"),
    { capture: true, passive: true },
  );
}
