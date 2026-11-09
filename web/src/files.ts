// Pictures and files: downloads need the session token in a header, so they're fetched
// here and shown from in-memory copies. Uploads go up with a progress bar.

import { Conn } from "./conn";
import { FILES_PATH, PROFILE_IMAGE_PATH, UPLOAD_PATH } from "./proto";
import type { Attachment, Id } from "./proto";

const cache = new Map<string, Promise<string>>();
/** Rough limit on what's kept in memory, so long chats on phones stay light. */
const MAX_CACHED = 300;

function authed(url: string): Promise<string> {
  let p = cache.get(url);
  if (!p) {
    p = fetch(url, { headers: { Authorization: `Bearer ${Conn.token ?? ""}` } }).then(async (r) => {
      if (!r.ok) throw new Error(`${r.status}`);
      return URL.createObjectURL(await r.blob());
    });
    p.catch(() => cache.delete(url));
    cache.set(url, p);
    if (cache.size > MAX_CACHED) {
      const [oldest, gone] = cache.entries().next().value!;
      cache.delete(oldest);
      gone.then(URL.revokeObjectURL, () => {});
    }
  }
  return p;
}

/** A downloadable URL for an attachment (or its small preview). */
export function fileUrl(a: Attachment, thumb = false): Promise<string> {
  return authed(`${FILES_PATH}/${encodeURIComponent(a.id)}${thumb ? "?thumb=1" : ""}`);
}

export function profileImageUrl(id: string): Promise<string> {
  return authed(`${PROFILE_IMAGE_PATH}/${encodeURIComponent(id)}`);
}

/** Point an <img> or <video> at an authorised file once it has loaded. */
export function setSrc(el: HTMLImageElement | HTMLVideoElement, url: Promise<string>): void {
  url.then((u) => (el.src = u)).catch(() => el.classList.add("broken"));
}

export function humanSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 ** 2) return `${(bytes / 1024).toFixed(0)} KB`;
  if (bytes < 1024 ** 3) return `${(bytes / 1024 ** 2).toFixed(1)} MB`;
  return `${(bytes / 1024 ** 3).toFixed(1)} GB`;
}

/** Upload one file for a message in `channel`. */
export function upload(channel: Id, file: File, onProgress: (fraction: number) => void): Promise<Attachment> {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    const name = encodeURIComponent(file.name || "photo.jpg");
    xhr.open("POST", `${UPLOAD_PATH}?channel_id=${channel}&filename=${name}`);
    xhr.setRequestHeader("Authorization", `Bearer ${Conn.token ?? ""}`);
    xhr.upload.onprogress = (e) => e.lengthComputable && onProgress(e.loaded / e.total);
    xhr.onload = () => {
      if (xhr.status >= 200 && xhr.status < 300) {
        try { resolve(JSON.parse(xhr.responseText) as Attachment); } catch (e) { reject(e); }
      } else {
        reject(new Error(xhr.responseText.trim() || `upload failed (${xhr.status})`));
      }
    };
    xhr.onerror = () => reject(new Error("upload failed: no connection"));
    xhr.send(file);
  });
}

/** Save a file to the device (or open it, on phones that can't download blobs). */
export async function download(a: Attachment): Promise<void> {
  const url = await fileUrl(a);
  const link = document.createElement("a");
  link.href = url;
  link.download = a.filename;
  document.body.appendChild(link);
  link.click();
  link.remove();
}
