// Entry point: start the app and register the service worker (needed for install and notifications).

import { App } from "./app";

const app = new App(document.getElementById("app")!);
app.start();

if ("serviceWorker" in navigator && isSecureContext) {
  navigator.serviceWorker.register("/sw.js").catch((e) => console.warn("service worker", e));
}
