import releasesData from './releases.json';

export const github = 'https://github.com/GameTL/tide-tidal-rust';

// The newest release's dmg, TIDE-<version>-macos-universal.dmg, as releases.json
// lists it. The site is rebuilt after every release, so this stays current;
// without release data (a local build) it falls back to the releases page.
const newest = (releasesData.releases ?? [])[0] as
  | { macUniversalUrl?: string | null }
  | undefined;
export const latestMacDmg = newest?.macUniversalUrl ?? `${github}/releases/latest`;

// One-line installer (website/public/install.sh). curl does not quarantine what
// it downloads, so TIDE installed this way opens without the Gatekeeper prompt.
export const installCommand =
  'curl -fsSL https://gametl.github.io/tide-tidal-rust/install.sh | sh';
