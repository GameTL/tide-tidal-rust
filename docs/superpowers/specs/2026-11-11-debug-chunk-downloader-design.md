# Design Spec: Debug Computer Monitor & Chunk Block Visualizer

## Overview
Add a developer/debug monitor button with a computer SVG icon in the top-right header (next to the user profile icon). Clicking this button opens a Popup Inspector displaying live information about the track audio stream and a bit-torrent style **chunk block matrix** showing DASH segments downloading in real-time.

---

## 1. Top-Right Header Button & Icon

- **Icon**: `crates/tide-app/ui/icons/computer.svg`
  - Feather/Lucide styled desktop computer monitor icon matching the 1.75 stroke width of existing icons (`user.svg`, `search.svg`, `device.svg`).
- **Placement**:
  - Located in `crates/tide-app/ui/chrome.slint` immediately to the left of the user profile avatar:
    - User profile button: `x: parent.width - 48px`
    - Debug computer button: `x: parent.width - 91px` (width: 35px, height: 35px, border-radius: 17.5px)
    - Search box position adjusted: `x: parent.width - 105px - self.width`
- **Active Download Indicator**:
  - When a download is in progress, a small vibrant pulse/badge (cyan or amber) appears over the computer button so the developer knows blocks are currently being fetched even when the popup is closed.

---

## 2. Popup Inspector Window

- **Structure**:
  - Slint `PopupWindow` anchored at `x: root.width - 13px - 340px`, `y: 58px`, `width: 340px`.
  - Dark container `#181818` with subtle border `#2e2e2e`, rounded corners (10px).
- **Header Section**:
  - Title: "Debug • Chunk Downloader"
  - Status pill:
    - `Downloading`: Amber/Cyan pill
    - `Cached on SSD`: Green pill
    - `Idle`: Gray pill
- **Track & Metric Summary**:
  - Current Track Title & Audio Quality tier badge (`MAX`, `HI-RES`, `LOSSLESS`).
  - Chunk counter: e.g. `48 / 112 segments (42.8%)`
  - Transfer statistics: e.g. `19.4 MB / 42.1 MB • 5.2 MB/s`
  - Current batch window: `Window 7 of 14 (8 parallel)`
- **Chunk Block Matrix (Grid)**:
  - Responsive block grid displaying all DASH segments for the track:
    - Grid columns: 12 blocks per row.
    - Each block: 20px $\times$ 14px with rounded corners (3px).
    - Block states:
      - `0` (Gray `#262626`, border `#333333`): Queued / Pending
      - `1` (Amber `#f59e0b`): In-flight downloading (within the current `WINDOW = 8`)
      - `2` (Green `#22c55e` / Cyan `#06b6d4`): Successfully downloaded & written
  - Tooltip / label underneath indicating the legend (`Pending` • `Downloading` • `Downloaded`).

---

## 3. Data Flow & Controller Integration

1. **Progress Callback in `tide-cache`**:
   - `fetch_or_cache` is extended with a progress callback `on_progress: Option<Arc<dyn Fn(usize, usize, usize, u64) + Send + Sync>>`:
     - Parameters: `(segment_index, total_segments, state, total_bytes_so_far)`
     - State `1` when assigned to the active `WINDOW = 8` worker set.
     - State `2` when `file.write_all` flushes the segment.
2. **Controller Bridge**:
   - In `controller.rs`:
     - Spawns a progress sender/receiver channel connected to the UI thread via `on_ui`.
     - When a track begins resolving:
       - If cached: immediately populates all blocks to state `2` (Green) with label `Cached on local SSD`.
       - If cache miss: initializes an array of `0`s with length = segment count.
     - Throttled UI updates (e.g., dispatched on every completed chunk or active window transition) to keep Slint frame rates smooth at 120Hz/240Hz.
3. **Slint Properties in `common.slint`**:
   - `in property <bool> debug-downloading;`
   - `in property <string> debug-track-title;`
   - `in property <string> debug-quality;`
   - `in property <string> debug-status;`
   - `in property <int> debug-chunks-done;`
   - `in property <int> debug-chunks-total;`
   - `in property <string> debug-stats-text;`
   - `in property <[int]> debug-blocks;`
   - `in-out property <bool> debug-open;`
