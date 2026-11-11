# Track context menu

Right-click a track or activate its **…** button to open the same menu on home
track cards, track tables (including the queue), and the player bar. Secondary
clicks on linked artist and album text also open the track menu; primary clicks
keep their existing behavior. Opening the menu does not change playback.

The menu shows the track artwork, title, and artist, followed by:

| Action | Behavior |
| --- | --- |
| Play next | Inserts the selected track after the current entry in playback order, including during shuffle. If there is no current entry, starts the selected track. |
| Add to / Remove from My Collection | Shows the current collection state and uses the existing favorite action. |
| Go to album / Go to artist | Opens the corresponding page. An action is disabled if its ID is missing. |
| Share → Copy link | Copies the track's TIDAL URL. |

Playlist editing, Save for Later, radio, credits, and embed code are outside
this release. The menu uses the track selected at opening, even if the player
advances or the original row is recycled.

One Slint overlay at the window root owns dismissal and layout. It is anchored
to the pointer or the **…** button, stays inside the window, and flips the Share
flyout left when needed. Clicking outside dismisses the menu. Use Up/Down or
Tab to move, Enter/Space to activate, Right/Left to enter or leave Share, and
Escape to close the flyout or the menu. Focus returns to the trigger when it
still represents the same track.
