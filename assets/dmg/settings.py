# dmgbuild settings for the SpotiPod installer window.
# Values come from scripts/build-macos-dmg.sh via -D key=value.
import os

app = defines["app"]  # noqa: F821
app_name = os.path.basename(app)

format = "ULFO"
filesystem = "HFS+"
size = None

files = [app]
symlinks = {"Applications": "/Applications"}
hide_extensions = [app_name]

icon = defines.get("volume_icon")  # noqa: F821
background = defines["background"]  # noqa: F821

# Window content matches the 660x400 background.
window_rect = ((200, 120), (660, 400))
default_view = "icon-view"
show_status_bar = False
show_tab_view = False
show_toolbar = False
show_pathbar = False
show_sidebar = False
include_icon_view_settings = True

arrange_by = None
icon_size = 112
text_size = 13
label_pos = "bottom"

# Icon centers, in background points.
icon_locations = {
    app_name: (170, 215),
    "Applications": (490, 215),
}
