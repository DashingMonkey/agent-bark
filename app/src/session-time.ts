/** 从回合发起时间计算时长，后台暂停或页面隐藏后仍能直接校准。 */
export function formatDuration(fromMs: number, toMs: number): string {
  const secs = Math.max(0, Math.floor((toMs - fromMs) / 1000));
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  const p = (n: number) => String(n).padStart(2, "0");
  if (h > 0) return `${h}小时${p(m)}分${p(s)}秒`;
  if (m > 0) return `${m}分${p(s)}秒`;
  return `${s}秒`;
}
