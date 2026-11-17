# dmgbuild settings for the TIDE installer disk image.
# Run by scripts/package-macos.sh as:
#   dmgbuild -s dmg-settings.py -D app=/path/TIDE.app -D background=/path/background.tiff NAME OUT.dmg
# Icon positions must match the empty slots drawn in background.html.
import os.path

application = defines["app"]  # noqa: F821 - injected by dmgbuild
appname = os.path.basename(application)

format = "UDZO"
filesystem = "HFS+"
files = [application]
symlinks = {"Applications": "/Applications"}

background = defines["background"]  # noqa: F821
# The background is 800x620 pt; the extra 28 pt is the window's title bar.
window_rect = ((200, 120), (800, 648))
default_view = "icon-view"
show_status_bar = False
show_tab_view = False
show_toolbar = False
show_pathbar = False
show_sidebar = False
include_icon_view_settings = True
include_list_view_settings = False

arrange_by = None
icon_size = 96
text_size = 13
label_pos = "bottom"
icon_locations = {
    appname: (250, 100),
    "Applications": (550, 100),
}
