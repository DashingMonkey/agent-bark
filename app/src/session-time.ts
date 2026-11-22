/** 时长各段位的单位词（中文「小时/分/秒」，英文缩写「h/m/s」）。
 *  本模块被 tests/session-time.test.mjs 以 VM 沙箱直接加载，
 *  **绝不 import i18n.ts 或 @tauri-apps 任何东西**——单位由调用方按当前语言传入。 */
export type DurationUnits = { hour: string; minute: string; second: string };

/** 按语言 id 取时长单位词：en → h/m/s 缩写，其余（zh）→ 中文单位 */
export function durationUnits(locale: string): DurationUnits {
  if (locale === "en") return { hour: "h", minute: "m", second: "s" };
  return { hour: "小时", minute: "分", second: "秒" };
}

/** 从回合发起时间计算时长，后台暂停或页面隐藏后仍能直接校准。
 *  units 缺省为中文单位（词表化之前的原行为，测试依赖这一默认值）。 */
export function formatDuration(fromMs: number, toMs: number, units: DurationUnits = durationUnits("zh")): string {
  const secs = Math.max(0, Math.floor((toMs - fromMs) / 1000));
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  const p = (n: number) => String(n).padStart(2, "0");
  if (h > 0) return `${h}${units.hour}${p(m)}${units.minute}${p(s)}${units.second}`;
  if (m > 0) return `${m}${units.minute}${p(s)}${units.second}`;
  return `${s}${units.second}`;
}
