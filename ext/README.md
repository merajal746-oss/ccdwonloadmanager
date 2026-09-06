# Browser extension (bridge to `ccdm-host`)

Forwards new downloads to the native host, which queues them in ccdM.
If the host is missing, the browser just downloads normally.

## 1. Build the host

From CI artifacts (or `cargo build --release`), get `ccdm-host(.exe)`.

## 2. Register the host

```sh
ccdm-host install
```

- Writes `com.ccdm.native.json` next to the exe.
- On Windows, registers Chrome / Edge / Firefox keys via `reg add`
  (inspect the output; `ccdm-host manifest [--firefox]` reprints the JSON).
- On Linux/macOS, copy the printed manifest to the browser's
  `NativeMessagingHosts` dir (paths are printed by `install`).
- Edit the manifest's `allowed_origins` (Chrome) to your extension id,
  or `allowed_extensions` (Firefox).

## 3. Load the extension

- **Chrome/Edge/Brave**: `chrome://extensions` → Developer mode → Load
  unpacked → `ext/chrome`. Copy its id into the manifest (step 2).
- **Firefox**: `about:debugging#/runtime/this-firefox` → Load Temporary
  Add-on → `ext/firefox/manifest.json`. The gecko id is
  `ccdm-bridge@example.com` — put that in `allowed_extensions`.

## 4. Try it

Download any `http(s)` file. The browser download is cancelled and the
URL appears in the ccdM GUI/CLI queue instead. Failures are silent on
purpose (the browser download then proceeds as usual); run the host
manually to debug:

```sh
ccdm-host --stdio
```

Check `ccdm-cli list` afterwards.
