// Resolves the chromium binary used by the browser-driven security gates.
//
// The order matters and is deliberate: on the maintainer's NixOS dev box the
// Playwright-bundled chromium (the one `npx playwright install` lands) is
// present but NOT runnable -- its shared-library set does not match the
// host, so `chromium.launch()` dies with `libnspr4.so: cannot open shared
// object file`. The system chromium at the NixOS store path IS runnable
// there, and it is what the gates used for years. So a bundled-browser
// lookup that runs before the system one silently converts a working local
// suite into a failing one.
//
//   1. PLAYWRIGHT_BROWSER_EXECUTABLE, when set. CI sets it to the exact
//      binary it just installed, so an explicit override always wins.
//   2. The NixOS store path, when it exists. Restores the long-standing
//      local behavior on the dev box.
//   3. playwright's own `chromium.executablePath()`, when it exists. This is
//      the contributor path: `npx playwright install chromium` on
//      Ubuntu/Debian/macOS lands a runnable bundled browser, which the old
//      hardcoded `/run/current-system/sw/bin/chromium` ENOENTed on.
//   4. `which chromium` / `which chromium-browser` for the remaining
//      platforms that ship a system chromium without the NixOS layout.
//
// The function is shared between the standalone security check
// (`scripts/web-console-security-check.mjs`) and the test suite
// (`test/security.test.mjs`) so the contract lives in one place.
import { existsSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { chromium } from 'playwright';

const NIXOS_FALLBACK = '/run/current-system/sw/bin/chromium';

function onPath(name) {
  try {
    return execFileSync('which', [name], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] }).trim() || undefined;
  } catch {
    return undefined;
  }
}

export function resolveChromium() {
  const fromEnv = process.env.PLAYWRIGHT_BROWSER_EXECUTABLE;
  if (fromEnv && existsSync(fromEnv)) return fromEnv;
  if (existsSync(NIXOS_FALLBACK)) return NIXOS_FALLBACK;
  try {
    const bundled = chromium.executablePath();
    if (bundled && existsSync(bundled)) return bundled;
  } catch {
    // executablePath() throws when no playwright install is present in this
    // checkout; fall through to the PATH lookup below.
  }
  return onPath('chromium') ?? onPath('chromium-browser') ?? NIXOS_FALLBACK;
}
