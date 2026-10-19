# UI preview

`preview.html` loads the real desktop UI (`desktop/ui`) with a fake app
(status, live, banner) so the layout can be checked in any browser:

    cd PhoneCast && python -m http.server 8777
    python tools/ui-preview/make_preview.py   (after UI changes)
    open http://127.0.0.1:8777/tools/ui-preview/preview.html

Edit `window.__previewStatus` in the browser console to try states (scene,
reconnecting, phone gone...).
