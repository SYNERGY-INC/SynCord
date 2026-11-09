// Sounds, phone notifications and the unread badge. Sounds are made on the fly with the
// same notes as the desktop app (crates/client/src/sound.rs), so there are no sound files.

export type Sound = "ping" | "message" | "join" | "leave";

const NOTES: Record<Sound, [number, number][]> = {
  ping: [[988, 0.09], [1319, 0.16]],
  message: [[784, 0.07], [1047, 0.13]],
  join: [[523.25, 0.08], [659.25, 0.08], [783.99, 0.16]],
  leave: [[783.99, 0.08], [659.25, 0.08], [523.25, 0.16]],
};

export interface Prefs {
  pingSound: boolean;
  dmSound: boolean;
  serverSound: boolean;
  voiceSounds: boolean;
  /** Phone notifications for DMs and mentions. */
  notify: boolean;
  /** ...and for every message in server channels too. */
  notifyAll: boolean;
}

const PREFS_KEY = "celerity.prefs";
const DEFAULTS: Prefs = { pingSound: true, dmSound: true, serverSound: false, voiceSounds: true, notify: true, notifyAll: false };

export const prefs: Prefs = (() => {
  try { return { ...DEFAULTS, ...JSON.parse(localStorage.getItem(PREFS_KEY) ?? "{}") }; } catch { return { ...DEFAULTS }; }
})();

export function savePrefs(): void {
  try { localStorage.setItem(PREFS_KEY, JSON.stringify(prefs)); } catch { /* ignore */ }
}

let ctx: AudioContext | null = null;
const lastPlayed = new Map<Sound, number>();

/** Browsers only allow sound after a tap: call this from any tap. */
export function unlockAudio(): AudioContext {
  ctx ??= new AudioContext();
  if (ctx.state === "suspended") void ctx.resume();
  return ctx;
}

export function play(sound: Sound, volume = 1): void {
  if (!ctx || ctx.state !== "running") return;
  const now = performance.now();
  if (now - (lastPlayed.get(sound) ?? 0) < 600) return; // a burst of messages makes one chime
  lastPlayed.set(sound, now);
  const gainScale = (sound === "message" ? 0.6 : 1) * 0.25 * volume;
  let t = ctx.currentTime + 0.01;
  for (const [freq, secs] of NOTES[sound]) {
    const osc = ctx.createOscillator();
    const gain = ctx.createGain();
    osc.frequency.value = freq;
    gain.gain.setValueAtTime(0, t);
    gain.gain.linearRampToValueAtTime(gainScale, t + 0.004);
    gain.gain.exponentialRampToValueAtTime(0.0001, t + secs);
    osc.connect(gain).connect(ctx.destination);
    osc.start(t);
    osc.stop(t + secs + 0.02);
    t += secs;
  }
}

export function notificationsSupported(): boolean {
  return "Notification" in window && "serviceWorker" in navigator;
}

export async function askPermission(): Promise<NotificationPermission | "unsupported"> {
  if (!notificationsSupported()) return "unsupported";
  return Notification.requestPermission();
}

/** Show a phone notification (through the service worker, which phones require). */
export async function show(title: string, body: string, channel: number): Promise<void> {
  if (!notificationsSupported() || Notification.permission !== "granted") return;
  const reg = await navigator.serviceWorker.getRegistration();
  const options: NotificationOptions = { body, tag: `channel-${channel}`, icon: "/icon-192.png", badge: "/icon-192.png", data: { channel } };
  if (reg) await reg.showNotification(title, options);
  else new Notification(title, options);
}

/** Unread count on the app icon (where supported) and in the tab title. */
export function setBadge(count: number, community: string): void {
  document.title = count > 0 ? `(${count}) ${community}` : community;
  const nav = navigator as Navigator & { setAppBadge?: (n?: number) => Promise<void>; clearAppBadge?: () => Promise<void> };
  if (count > 0) nav.setAppBadge?.(count).catch(() => {});
  else nav.clearAppBadge?.().catch(() => {});
}

/** Is this running as an installed app (from the home screen)? */
export function installed(): boolean {
  return matchMedia("(display-mode: standalone)").matches || (navigator as Navigator & { standalone?: boolean }).standalone === true;
}

export function isIos(): boolean {
  return /iPad|iPhone|iPod/.test(navigator.userAgent) || (navigator.platform === "MacIntel" && navigator.maxTouchPoints > 1);
}
