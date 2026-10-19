"""Rebuilds preview.html from the real desktop/ui/index.html plus a fake app
(stub.js.html), so the layout can be checked in any browser."""
import os
import re

here = os.path.dirname(os.path.abspath(__file__))
ui = os.path.join(here, "..", "..", "desktop", "ui")
html = open(os.path.join(ui, "index.html"), encoding="utf-8").read()
stub = open(os.path.join(here, "stub.js.html"), encoding="utf-8").read()
html = re.sub(
    r'(src|href)="(?!https?:|data:|#)([^"]+\.(?:js|css|svg|png|ico))"',
    lambda m: f'{m.group(1)}="../../desktop/ui/{m.group(2)}"',
    html,
)
html = html.replace(
    '<script src="../../desktop/ui/app.js"></script>',
    stub + '<script src="../../desktop/ui/app.js"></script>',
    1,
)
open(os.path.join(here, "preview.html"), "w", encoding="utf-8").write(html)
print("preview.html rebuilt")
