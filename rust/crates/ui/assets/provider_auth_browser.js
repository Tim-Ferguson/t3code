// DOM activation only. Rust owns consent, permissions and request settlement.
const navigate = () => {
  if (args.native) {
    // The native WebView navigation handler opens external HTTP(S) URLs in the
    // platform browser and returns false, retaining the application's document.
    // Synthetic anchors outside the Dioxus root do not have its click listener.
    window.location.href = args.url;
    return true;
  }
  if (tab) {
    tab.location.href = args.url;
    return true;
  }
  return !!window.open(args.url, "_blank", "noopener");
};
// Reserve a web popup during the original user gesture, before consent awaits.
const tab = args.native ? null : window.open(args.consent ? "" : args.url, "_blank");
if (tab) tab.opener = null;
try {
  if (args.consent && !(await dioxus.recv())) {
    tab?.close();
    return true;
  }
  return args.consent || args.native ? navigate() : !!tab;
} catch (error) {
  tab?.close();
  throw error;
}
