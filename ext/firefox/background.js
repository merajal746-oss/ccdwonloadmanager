// ccDM Browser Bridge: forward new downloads to ccdm-host, then cancel the
// browser's own download when the host accepts. Works in Chrome / Edge /
// Brave / Opera (MV3) and Firefox (MV3, see ../firefox/manifest.json).

const HOST_NAME = "com.ccdm.native";

// Only take over real file downloads, not blob:/data: or tiny pings.
function shouldHandle(item) {
  if (!item || item.state !== "in_progress" || item.paused) return false;
  const url = item.finalUrl || item.url || "";
  if (!/^https?:\/\//i.test(url)) return false;
  if (item.fileSize === 0) return false;
  return true;
}

function fileNameOf(item) {
  const name = item.filename || "";
  const base = name.split(/[\\/]/).pop() || "";
  return base || undefined;
}

chrome.downloads.onCreated.addListener((item) => {
  if (!shouldHandle(item)) return;
  const message = {
    type: "download",
    url: item.finalUrl || item.url,
    filename: fileNameOf(item),
    referrer: item.referrer || undefined,
  };
  const send =
    typeof browser !== "undefined" && browser.runtime && browser.runtime.sendNativeMessage
      ? browser.runtime.sendNativeMessage.bind(browser.runtime)
      : chrome.runtime.sendNativeMessage.bind(chrome.runtime);
  try {
    const result = send(HOST_NAME, message, (response) => {
      const error =
        (chrome.runtime && chrome.runtime.lastError && chrome.runtime.lastError.message) || null;
      if (error) {
        // Host not installed: leave the browser download running.
        console.debug("[ccdm] host unavailable:", error);
        return;
      }
      if (response && response.type === "accepted" && item.id !== undefined) {
        chrome.downloads.cancel(item.id, () => {
          chrome.downloads.erase({ id: item.id }, () => {});
        });
      } else {
        console.debug("[ccdm] host response:", response);
      }
    });
    // Firefox returns a Promise; swallow rejections (host missing).
    if (result && typeof result.catch === "function") {
      result.catch((error) => console.debug("[ccdm] host unavailable:", error));
    }
  } catch (error) {
    console.debug("[ccdm] host unavailable:", error);
  }
});
