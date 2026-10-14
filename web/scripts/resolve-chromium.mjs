// Resolves the chromium binary used by the browser-driven security gates.
//
// Resolution order:
//   1. PLAYWRIGHT_BROWSER_EXECUTABLE if set and the file exists
//   2. `chromium.executablePath()` from playwright's own bundled browser
//      (the path that `npx playwright install chromium` lands). This is
//      the path CI installs into on every PR, and the path the local
//      `npm test --prefix web` uses after `playwright install` has run
//      at least once. Returning it from playwright itself means we
//      cannot drift from what playwright actually launches.
//   3. The NixOS-only `/run/current-system/sw/bin/chromium` that the
//      maintainer's dev box uses, since `executablePath()` returns
//      `undefined` when playwright has not been installed in this
//      checkout. The fallback is a real, runnable binary on the box
//      the script was written on and a clear ENOENT everywhere else.
//
// The function is shared between the standalone security check
// (`scripts/web-console-security-check.mjs`) and the test suite
// (`test/security.test.mjs`) so the contract is one place.
import { existsSync } from 'node:fs';
import { chromium } from 'playwright';

const NIXOS_FALLBACK = '/run/current-system/sw/bin/chromium';

export function resolveChromium() {
  const fromEnv = process.env.PLAYWRIGHT_BROWSER_EXECUTABLE;
  if (fromEnv && existsSync(fromEnv)) return fromEnv;
  try {
    const bundled = chromium.executablePath();
    if (bundled && existsSync(bundled)) return bundled;
  } catch {
    // executablePath() throws when no playwright install is present;
    // fall through to the NixOS fallback below.
  }
  return NIXOS_FALLBACK;
}
