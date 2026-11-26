import releasesData from './releases.json';

export const github = 'https://github.com/GameTL/tide-tidal-rust';

// The newest release's files, as releases.json lists them. The site is rebuilt
// after every release, so these stay current. A build the newest release does
// not carry is null, and the page shows that platform as "Coming soon".
const newest = (releasesData.releases ?? [])[0] as
  | {
      macUniversalUrl?: string | null;
      linuxUrl?: string | null;
      linuxArmUrl?: string | null;
      windowsUrl?: string | null;
    }
  | undefined;
// Without release data (a local build) the dmg falls back to the releases page.
export const latestMacDmg = newest?.macUniversalUrl ?? `${github}/releases/latest`;
export const latestWindows = newest?.windowsUrl ?? null;
export const latestLinuxTarball = newest?.linuxUrl ?? null;
export const latestLinuxArmTarball = newest?.linuxArmUrl ?? null;

// Homebrew on Linux (Ubuntu, Bazzite). The cask lives in this repository.
export const brewCommand = `brew tap gametl/tide ${github} && brew install --cask tide`;

// One-line installer (website/public/install.sh). curl does not quarantine what
// it downloads, so TIDE installed this way opens without the Gatekeeper prompt.
export const installCommand =
  'curl -fsSL https://gametl.github.io/tide-tidal-rust/install.sh | sh';
