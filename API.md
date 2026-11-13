# TIDAL API

✅ = called by this repo · ❌ = not called. The list covers resource-level endpoints from `tidlers` 0.5.0 and the endpoints the app calls directly. OpenAPI `/relationships/*` sub-endpoints are collapsed into one row.

| Endpoint | Purpose | Used |
|---|---|---|
| **Auth** | | |
| `POST https://auth.tidal.com/v1/oauth2/device_authorization` | Start device-code login | ✅ `start_device_auth` |
| `POST https://auth.tidal.com/v1/oauth2/token` (`device_code`) | Poll device-code login | ✅ `poll_device_auth` |
| `GET https://login.tidal.com/authorize` | PKCE login page | ✅ `start_pkce_login` |
| `POST https://auth.tidal.com/v1/oauth2/token` (`authorization_code`) | PKCE code exchange | ✅ `finish_pkce_login` |
| `POST https://auth.tidal.com/v1/oauth2/token` (`refresh_token`) | Refresh access token | ✅ `refresh`, `load_session` |
| `POST https://auth.tidal.com/v1/oauth2/token` (`client_credentials`) | App-only token | ❌ |
| `POST https://api.tidal.com/v1/logout` | Revoke token | ✅ `revoke` |
| `GET https://openapi.tidal.com/v2/users/me` | Current user (id, username, country) | ✅ `poll_device_auth`, `finish_pkce_login`, `load_session` |
| `GET https://api.tidal.com/v1/users/{id}` | User profile | ❌ |
| `GET https://api.tidal.com/v1/profiles/{id}` | Public profile | ❌ |
| `GET https://api.tidal.com/v1/users/{id}/subscription` | Subscription info | ❌ |
| `GET https://api.tidal.com/v1/users/{id}/playlists` | User's playlists | ❌ |
| **Favourites (My Collection)** | | |
| `GET https://api.tidal.com/v1/users/{me}/favorites/tracks` | Favourite tracks | ✅ `favorite_tracks` |
| `POST https://api.tidal.com/v1/users/{me}/favorites/tracks` | Add favourite track | ✅ `set_favorite` |
| `DELETE https://api.tidal.com/v1/users/{me}/favorites/tracks/{id}` | Remove favourite track | ✅ `set_favorite` |
| `GET https://api.tidal.com/v1/users/{me}/favorites/albums` | Favourite albums | ✅ `albums` |
| `POST https://api.tidal.com/v1/users/{me}/favorites/albums` | Add favourite album | ✅ `set_album_favorite` |
| `DELETE https://api.tidal.com/v1/users/{me}/favorites/albums/{id}` | Remove favourite album | ✅ `set_album_favorite` |
| `GET https://api.tidal.com/v1/users/{me}/favorites/artists` | Followed artists | ✅ `followed_artists` |
| `POST https://api.tidal.com/v1/users/{me}/favorites/artists` | Follow artist | ✅ `set_artist_followed` |
| `DELETE https://api.tidal.com/v1/users/{me}/favorites/artists/{id}` | Unfollow artist | ✅ `set_artist_followed` |
| `GET https://api.tidal.com/v1/users/{me}/favorites/videos` | Favourite videos | ✅ `videos` |
| `POST https://api.tidal.com/v1/users/{me}/favorites/videos` | Add favourite video | ✅ `set_video_favorite` |
| `DELETE https://api.tidal.com/v1/users/{me}/favorites/videos/{id}` | Remove favourite video | ✅ `set_video_favorite` |
| `POST https://api.tidal.com/v1/users/{me}/favorites/playlists` | Save a playlist | ✅ `set_playlist_favorite` |
| `DELETE https://api.tidal.com/v1/users/{me}/favorites/playlists/{uuid}` | Unsave a playlist | ✅ `set_playlist_favorite` |
| `GET https://api.tidal.com/v1/users/{me}/favorites/ids` | Ids of favourite albums, playlists, videos, artists | ✅ `favorite_ids`, `is_favorite` |
| `GET https://api.tidal.com/v1/users/{me}/favorites/later` | Save for Later (v1) | ✅ `later` |
| `GET https://api.tidal.com/v2/favorites/later` | Save for Later (v2 fallback) | ✅ `later` |
| `GET https://api.tidal.com/v2/favorites/mixes` | Favourite mixes | ✅ `mixes` |
| **Playlists & folders** | | |
| `GET https://api.tidal.com/v1/playlists/{id}` | Playlist metadata and ETag | ✅ `add_to_playlist` |
| `GET https://api.tidal.com/v1/playlists/{id}/items` | Playlist tracks | ✅ `tracks_of` |
| `POST https://api.tidal.com/v1/playlists/{id}/items` | Append tracks | ✅ `add_to_playlist` |
| `DELETE https://api.tidal.com/v1/playlists/{id}/items/{indices}` | Remove tracks by index | ❌ |
| `GET https://api.tidal.com/v1/playlists/{id}/recommendations/items` | Playlist recommendations | ❌ |
| `GET https://api.tidal.com/v2/user-playlists/{id}/public` | A user's public playlists | ❌ |
| `GET https://api.tidal.com/v2/my-collection/playlists/folders` | List folders and playlists | ✅ `playlists` |
| `GET https://api.tidal.com/v2/my-collection/playlists/folders/flattened` | All folders, flattened | ❌ |
| `PUT https://api.tidal.com/v2/my-collection/playlists/folders/create-folder` | Create folder | ✅ `create_in_collection` |
| `PUT https://api.tidal.com/v2/my-collection/playlists/folders/create-playlist` | Create playlist | ✅ `create_in_collection` |
| `PUT https://api.tidal.com/v2/my-collection/playlists/folders/rename` | Rename folder | ✅ `rename_folder` |
| `PUT https://api.tidal.com/v2/my-collection/playlists/folders/remove` | Delete folder | ✅ `delete_folder` |
| `GET https://api.tidal.com/v2/my-collection/artists/folders` | Artist collection folders | ❌ |
| **Catalogue** | | |
| `GET https://api.tidal.com/v1/tracks/{id}` | Track metadata | ✅ `track` |
| `GET https://api.tidal.com/v1/tracks/{id}/playbackinfopostpaywall` | Track stream manifest | ✅ `stream` |
| `GET https://api.tidal.com/v1/tracks/{id}/lyrics` | Lyrics (plain and synced) | ✅ `lyrics` |
| `GET https://api.tidal.com/v1/tracks/{id}/mix` | Track mix | ❌ |
| `GET https://api.tidal.com/v1/tracks/{id}/radio` | Track radio | ❌ |
| `GET https://api.tidal.com/v1/albums/{id}` | Album metadata | ✅ `catalog_detail` |
| `GET https://api.tidal.com/v1/albums/{id}/items` | Album tracks | ✅ `tracks_of` |
| `GET https://api.tidal.com/v1/albums/{id}/credits` | Album credits | ✅ `album_credits` |
| `GET https://api.tidal.com/v1/albums/{id}/items/credits` | Per-track credits | ❌ |
| `GET https://api.tidal.com/v1/albums/{id}/review` | Album review | ❌ |
| `GET https://api.tidal.com/v1/artists/{id}` | Artist metadata | ✅ `catalog_detail` |
| `GET https://api.tidal.com/v1/artists/{id}/bio` | Artist biography | ✅ `catalog_detail` |
| `GET https://api.tidal.com/v1/artists/{id}/albums` | Albums and singles | ✅ `artist_releases` |
| `GET https://api.tidal.com/v1/artists/{id}/toptracks` | Top tracks | ✅ `tracks_of` |
| `GET https://api.tidal.com/v1/artists/{id}/similar` | Similar artists | ✅ `similar_artists` |
| `GET https://api.tidal.com/v1/artists/{id}/videos` | Artist videos (paginated) | ✅ `artist_videos` |
| `GET https://api.tidal.com/v1/artists/{id}/links` | Artist links | ❌ |
| `GET https://api.tidal.com/v1/artists/{id}/mix` | Artist mix | ❌ |
| `GET https://api.tidal.com/v1/mixes/{id}/items` | Mix tracks | ✅ `tracks_of` |
| `GET https://api.tidal.com/v1/videos/{id}` | Video metadata | ✅ `video` |
| `GET https://api.tidal.com/v1/videos/{id}/playbackinfopostpaywall` | Video HLS manifest | ✅ `video_stream` |
| `GET https://api.tidal.com/v1/videos/{id}/recommendations` | Similar videos | ✅ `video_recommendations` |
| `GET https://api.tidal.com/v1/search` | Search tracks, artists, albums, videos | ✅ `search_tracks`, `search_catalog`, `search_videos` |
| `GET https://api.tidal.com/v1/suggestions` | Search suggestions | ❌ |
| `GET https://api.tidal.com/v1/pages/{slug}` | Explore and genre pages | ✅ `explore`, `catalog_page` |
| **Feeds & home** | | |
| `GET https://tidal.com/v2/home/feed/{static\|editorial\|uploads}` | Home tabs | ✅ `home`, `home_feed` |
| `GET https://api.tidal.com/v2/feed/activities` | Feed activity | ✅ `feed` |
| `GET https://api.tidal.com/v2/my-activity/timeline` | Activity timeline | ❌ |
| `GET https://api.tidal.com/v2/my-activity/top-artists` | Top artists | ❌ |
| **OpenAPI v2** (`https://openapi.tidal.com/v2`) | | |
| `GET /albums`, `GET /albums/{id}` | Album resources | ❌ |
| `GET /artists`, `GET /artists/{id}` | Artist resources | ❌ |
| `GET /tracks`, `GET /tracks/{id}` | Track resources; list includes user uploads | ❌ |
| `GET /videos`, `GET /videos/{id}` | Video resources | ❌ |
| `GET /playlists`, `GET /playlists/{id}` | Playlist resources | ❌ |
| `GET /credits`, `GET /credits/{id}` | Credit resources | ❌ |
| `GET /lyrics`, `GET /lyrics/{id}` | Lyric resources | ❌ |
| `GET /searchResults/{id}` | Search results | ❌ |
| `GET /userCollections/{id}`, `GET /userCollectionFolders/{id}` | Collection and folder resources | ❌ |
| `GET /userRecommendations/{id}/relationships/newArrivalMixes` | New arrival mixes | ❌ |
| `GET /{resource}/{id}/relationships/{rel}` | All relationship sub-endpoints | ❌ |
| **Media & CDN** | | |
| `GET https://resources.tidal.com/images/{uuid}/{W}x{H}.jpg` | Cover and artist images | ✅ `image_url`, `video_image_url` |
| `GET https://resources.tidal.com/images/{uuid}/origin.jpg` | Full-size image | ✅ `origin_image_url` |
| Pre-signed track stream URLs (JSON `urls`) | Audio segments | ✅ `stream` |
| Pre-signed DASH MPD | Audio manifest (`dash`) | ✅ `parse_dash` |
| Pre-signed HLS master playlist | Video renditions | ✅ `video_stream`, `parse_hls_master` |
| **Web links** (not API) | | |
| `https://tidal.com/android/login/auth` | PKCE redirect URI | ✅ `LOGIN_REDIRECT_PREFIX` |
| `https://tidal.com/{kind}/{id}` | Share link | ✅ `copy_link` |
